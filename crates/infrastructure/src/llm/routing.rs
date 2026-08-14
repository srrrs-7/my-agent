//! Provider routing.
//!
//! [`RoutingProvider`] is a composite: it *is* an
//! [`LlmProvider`] and it *contains* several, delegating each request to the
//! one a [`LlmRouter`] picks. Callers never learn that routing happened, which
//! is what makes this safe to add later - or to remove.
//!
//! The two policies shipped today are deliberately simple; the interesting ones
//! (cost-aware, latency-aware, capability-based, failover, A/B) plug in at the
//! same seam by implementing [`LlmRouter`], and can consult
//! [`agent_domain::model::llm::RequestMetadata`] which travels with every
//! request for exactly this purpose.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use agent_domain::error::LlmError;
use agent_domain::model::llm::{
    ChatRequest, ChatResponse, ModelId, ProviderCapabilities, ProviderId,
};
use agent_domain::ports::llm::{LlmProvider, LlmRouter, RouteDecision};
use async_trait::async_trait;
use tracing::debug;

pub struct RoutingProvider {
    providers: BTreeMap<ProviderId, Arc<dyn LlmProvider>>,
    router: Arc<dyn LlmRouter>,
}

impl RoutingProvider {
    pub fn new(providers: Vec<Arc<dyn LlmProvider>>, router: Arc<dyn LlmRouter>) -> Self {
        Self {
            providers: providers
                .into_iter()
                .map(|provider| (provider.id(), provider))
                .collect(),
            router,
        }
    }

    pub fn provider_ids(&self) -> Vec<ProviderId> {
        self.providers.keys().cloned().collect()
    }
}

#[async_trait]
impl LlmProvider for RoutingProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new("router")
    }

    fn capabilities(&self) -> ProviderCapabilities {
        // Only promise what every candidate can deliver: the caller cannot know
        // which one will serve the next request.
        self.providers
            .values()
            .map(|provider| provider.capabilities())
            .reduce(ProviderCapabilities::intersect)
            .unwrap_or_default()
    }

    async fn chat(&self, mut request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let decision = self.router.route(&request).await?;

        let provider = self.providers.get(&decision.provider).ok_or_else(|| {
            LlmError::NoRoute(format!(
                "router `{}` chose `{}`, which is not configured (known: {})",
                self.router.name(),
                decision.provider,
                self.providers
                    .keys()
                    .map(ProviderId::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;

        if let Some(model) = decision.model {
            request.model = Some(model);
        }

        debug!(
            provider = %decision.provider,
            reason = %decision.reason,
            router = self.router.name(),
            "routed request"
        );

        provider.chat(request).await
    }
}

/// Always the same provider. The correct policy when only one is configured.
pub struct StaticRouter {
    default: ProviderId,
}

impl StaticRouter {
    pub fn new(default: ProviderId) -> Self {
        Self { default }
    }
}

#[async_trait]
impl LlmRouter for StaticRouter {
    fn name(&self) -> &str {
        "static"
    }

    async fn route(&self, _request: &ChatRequest) -> Result<RouteDecision, LlmError> {
        Ok(RouteDecision::to(self.default.clone(), "static default"))
    }
}

/// Routes on a `<provider>/<model>` reference, e.g. `cloud/claude-sonnet-5`.
///
/// A slash only counts as a separator when the prefix names a configured
/// provider, so vendor ids that contain slashes (`meta-llama/Llama-3.1-8B`)
/// keep working.
pub struct ModelPrefixRouter {
    default: ProviderId,
    known: BTreeSet<ProviderId>,
}

impl ModelPrefixRouter {
    pub fn new(default: ProviderId, known: impl IntoIterator<Item = ProviderId>) -> Self {
        Self {
            default,
            known: known.into_iter().collect(),
        }
    }
}

#[async_trait]
impl LlmRouter for ModelPrefixRouter {
    fn name(&self) -> &str {
        "model-prefix"
    }

    async fn route(&self, request: &ChatRequest) -> Result<RouteDecision, LlmError> {
        if let Some(model) = &request.model {
            if let Some((prefix, rest)) = model.split_provider_hint() {
                let candidate = ProviderId::new(prefix);
                if self.known.contains(&candidate) && !rest.is_empty() {
                    return Ok(RouteDecision::to(
                        candidate,
                        format!("model reference `{model}` names a provider"),
                    )
                    .with_model(ModelId::new(rest)));
                }
            }
        }
        Ok(RouteDecision::to(
            self.default.clone(),
            "no provider prefix on the model",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::llm::{StopReason, TokenUsage};
    use agent_domain::model::message::Message;

    struct StubProvider(ProviderId);

    #[async_trait]
    impl LlmProvider for StubProvider {
        fn id(&self) -> ProviderId {
            self.0.clone()
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
            Ok(ChatResponse {
                message: Message::assistant_text(format!(
                    "{}|{}",
                    self.0,
                    request
                        .model
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| "-".into())
                )),
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                model: ModelId::new("stub"),
                provider: self.0.clone(),
            })
        }
    }

    fn routing(router: Arc<dyn LlmRouter>) -> RoutingProvider {
        RoutingProvider::new(
            vec![
                Arc::new(StubProvider(ProviderId::new("local"))),
                Arc::new(StubProvider(ProviderId::new("cloud"))),
            ],
            router,
        )
    }

    #[tokio::test]
    async fn static_router_always_picks_the_default() {
        let provider = routing(Arc::new(StaticRouter::new(ProviderId::new("local"))));
        let response = provider
            .chat(ChatRequest::new(vec![Message::user("hi")]).with_model(Some(ModelId::new("x"))))
            .await
            .unwrap();
        assert_eq!(response.message.text(), "local|x");
    }

    #[tokio::test]
    async fn prefix_router_selects_and_rewrites_the_model() {
        let provider = routing(Arc::new(ModelPrefixRouter::new(
            ProviderId::new("local"),
            [ProviderId::new("local"), ProviderId::new("cloud")],
        )));
        let response = provider
            .chat(
                ChatRequest::new(vec![Message::user("hi")])
                    .with_model(Some(ModelId::new("cloud/claude-sonnet-5"))),
            )
            .await
            .unwrap();
        assert_eq!(response.message.text(), "cloud|claude-sonnet-5");
    }

    #[tokio::test]
    async fn unknown_prefixes_are_left_alone() {
        let provider = routing(Arc::new(ModelPrefixRouter::new(
            ProviderId::new("local"),
            [ProviderId::new("local"), ProviderId::new("cloud")],
        )));
        let response = provider
            .chat(
                ChatRequest::new(vec![Message::user("hi")])
                    .with_model(Some(ModelId::new("meta-llama/Llama-3.1-8B"))),
            )
            .await
            .unwrap();
        assert_eq!(response.message.text(), "local|meta-llama/Llama-3.1-8B");
    }

    #[tokio::test]
    async fn an_unconfigured_choice_is_reported_as_no_route() {
        let provider = routing(Arc::new(StaticRouter::new(ProviderId::new("missing"))));
        let error = provider
            .chat(ChatRequest::new(vec![Message::user("hi")]))
            .await
            .unwrap_err();
        assert!(matches!(error, LlmError::NoRoute(_)));
    }
}

//! Provider routing.
//!
//! [`RoutingProvider`] is a composite: it *is* an
//! [`LlmProvider`] and it *contains* several, delegating each request to the
//! one a [`LlmRouter`] picks. Callers never learn that routing happened, which
//! is what makes this safe to add later - or to remove.
//!
//! The policies shipped today are deliberately simple; the interesting ones
//! (cost-aware, latency-aware, capability-based, A/B) plug in at the same seam
//! by implementing [`LlmRouter`], and can consult
//! [`agent_domain::model::llm::RequestMetadata`] which travels with every
//! request for exactly this purpose. Failover is not a router: it is a
//! property of the destination a router picked (see [`super::failover`]).
//!
//! Every decision is logged at `info` under the `agent::route` target with its
//! reason and task kind, which `-v` turns on - that is how "the summary went
//! to `local`" becomes visible without the loop knowing routing exists.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use agent_domain::error::LlmError;
use agent_domain::model::llm::{
    ChatRequest, ChatResponse, ModelId, ProviderCapabilities, ProviderId, TaskKind,
};
use agent_domain::ports::llm::{ChatStream, LlmProvider, LlmRouter, RouteDecision};
use async_trait::async_trait;
use tracing::info;

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

    /// Runs the router and resolves its decision to a concrete provider,
    /// rewriting the request's model when the decision says so. Shared by
    /// every entry point so they cannot drift apart.
    async fn resolve(
        &self,
        mut request: ChatRequest,
    ) -> Result<(&Arc<dyn LlmProvider>, ChatRequest), LlmError> {
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

        info!(
            target: "agent::route",
            "{:?} request -> `{}` ({}; router {})",
            request.metadata.task_kind,
            decision.provider,
            decision.reason,
            self.router.name()
        );

        Ok((provider, request))
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

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let (provider, request) = self.resolve(request).await?;
        provider.chat(request).await
    }

    /// Routes exactly like [`Self::chat`]; the chosen provider decides whether
    /// it truly streams or serves the default single-`Completed` fallback.
    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream, LlmError> {
        let (provider, request) = self.resolve(request).await?;
        provider.chat_stream(request).await
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

/// Routes on what a request is *for*: summaries to a cheap local model, the
/// agent loop to a capable cloud one.
///
/// An explicit `<provider>/<model>` reference still wins, exactly as under
/// [`ModelPrefixRouter`]: someone who typed `-m cloud/claude-sonnet-5` asked
/// for that provider, and the reference is pinned for the whole run -
/// summaries included, since the compactor inherits the run's model.
pub struct TaskKindRouter {
    default: ProviderId,
    routes: HashMap<TaskKind, ProviderId>,
    prefix: ModelPrefixRouter,
    /// Each provider's configured model, for requests routed away from the
    /// provider an unprefixed `-m` was meant for.
    models: BTreeMap<ProviderId, ModelId>,
}

impl TaskKindRouter {
    pub fn new(
        default: ProviderId,
        routes: HashMap<TaskKind, ProviderId>,
        models: impl IntoIterator<Item = (ProviderId, ModelId)>,
    ) -> Self {
        let models: BTreeMap<_, _> = models.into_iter().collect();
        Self {
            prefix: ModelPrefixRouter::new(default.clone(), models.keys().cloned()),
            default,
            routes,
            models,
        }
    }
}

#[async_trait]
impl LlmRouter for TaskKindRouter {
    fn name(&self) -> &str {
        "task-kind"
    }

    async fn route(&self, request: &ChatRequest) -> Result<RouteDecision, LlmError> {
        let explicit = self.prefix.route(request).await?;
        if explicit.model.is_some() {
            return Ok(explicit);
        }

        let kind = request.metadata.task_kind;
        let Some(destination) = self.routes.get(&kind) else {
            return Ok(RouteDecision::to(
                self.default.clone(),
                format!("no route for {kind:?}; default provider"),
            ));
        };
        let decision = RouteDecision::to(destination.clone(), format!("route for {kind:?}"));

        // An unprefixed model names one of the default provider's models;
        // carried to another server it would only produce a 404.
        match (&request.model, self.models.get(destination)) {
            (Some(_), Some(model)) if *destination != self.default => {
                Ok(decision.with_model(model.clone()))
            }
            _ => Ok(decision),
        }
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

    fn task_kind_router() -> Arc<TaskKindRouter> {
        Arc::new(TaskKindRouter::new(
            ProviderId::new("cloud"),
            HashMap::from([
                (TaskKind::Summarize, ProviderId::new("local")),
                (TaskKind::Classify, ProviderId::new("local")),
            ]),
            [
                (ProviderId::new("local"), ModelId::new("qwen3:8b")),
                (ProviderId::new("cloud"), ModelId::new("claude-sonnet-5")),
            ],
        ))
    }

    fn of_kind(kind: TaskKind) -> ChatRequest {
        let mut request = ChatRequest::new(vec![Message::user("hi")]);
        request.metadata.task_kind = kind;
        request
    }

    #[tokio::test]
    async fn task_kind_router_sends_summaries_to_their_route() {
        let provider = routing(task_kind_router());
        let response = provider.chat(of_kind(TaskKind::Summarize)).await.unwrap();
        assert_eq!(response.message.text(), "local|-");
    }

    #[tokio::test]
    async fn task_kind_router_sends_the_agent_loop_to_the_default() {
        let provider = routing(task_kind_router());
        let response = provider.chat(of_kind(TaskKind::Agentic)).await.unwrap();
        assert_eq!(response.message.text(), "cloud|-");
    }

    #[tokio::test]
    async fn task_kind_router_sends_an_unrouted_kind_to_the_default() {
        let provider = routing(task_kind_router());
        let response = provider.chat(of_kind(TaskKind::Chat)).await.unwrap();
        assert_eq!(response.message.text(), "cloud|-");
    }

    #[tokio::test]
    async fn an_explicit_provider_prefix_wins_over_the_task_kind_route() {
        let provider = routing(task_kind_router());
        let response = provider
            .chat(of_kind(TaskKind::Summarize).with_model(Some(ModelId::new("cloud/opus"))))
            .await
            .unwrap();
        assert_eq!(response.message.text(), "cloud|opus");
    }

    #[tokio::test]
    async fn an_unprefixed_model_does_not_follow_a_request_routed_elsewhere() {
        let provider = routing(task_kind_router());

        let summary = provider
            .chat(of_kind(TaskKind::Summarize).with_model(Some(ModelId::new("claude-opus"))))
            .await
            .unwrap();
        assert_eq!(
            summary.message.text(),
            "local|qwen3:8b",
            "the default provider's model name means nothing to the local server"
        );

        let agentic = provider
            .chat(of_kind(TaskKind::Agentic).with_model(Some(ModelId::new("claude-opus"))))
            .await
            .unwrap();
        assert_eq!(agentic.message.text(), "cloud|claude-opus");
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

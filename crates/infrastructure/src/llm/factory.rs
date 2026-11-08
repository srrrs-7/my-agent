//! Builds the provider stack from configuration.

use std::collections::BTreeMap;
use std::sync::Arc;

use agent_domain::error::LlmError;
use agent_domain::model::llm::ProviderId;
use agent_domain::ports::llm::{LlmProvider, LlmRouter};

use crate::config::{LlmSettings, ProviderKind, ProviderSettings, RouterKind};

use super::{
    AnthropicProvider, FailoverProvider, ModelPrefixRouter, OpenAiCompatibleProvider,
    RetryingProvider, RoutingProvider, StaticRouter, TaskKindRouter,
};

/// Assembles `Route({ id: Failover(Retry(client), [Retry(fallback)...]) })`.
///
/// Each client is retried on its own, so failover (see
/// [`super::failover`]) crosses destinations only once a destination's own
/// retries are spent. Without `AGENT_FAILOVER` a destination is just
/// `Retry(client)`, which behaves as the former `Retry(Route(...))` did.
///
/// The routing layer is present even for a single provider. It costs one map
/// lookup per request and means the "add a second model" change is a config
/// edit rather than a code change.
pub fn build_provider(settings: &LlmSettings) -> Result<Arc<dyn LlmProvider>, LlmError> {
    if settings.providers.is_empty() {
        return Err(LlmError::Configuration(
            "no LLM providers are configured".to_string(),
        ));
    }

    let retried = settings
        .providers
        .iter()
        .map(|provider| {
            let client = build_client(provider, settings)?;
            let retried: Arc<dyn LlmProvider> =
                Arc::new(RetryingProvider::new(client, settings.max_retries));
            Ok((provider.id.clone(), retried))
        })
        .collect::<Result<BTreeMap<_, _>, LlmError>>()?;
    let destinations = with_failover(&retried, settings)?;

    let router: Arc<dyn LlmRouter> = match settings.router {
        RouterKind::Static => Arc::new(StaticRouter::new(settings.default_provider.clone())),
        RouterKind::ModelPrefix => Arc::new(ModelPrefixRouter::new(
            settings.default_provider.clone(),
            settings
                .providers
                .iter()
                .map(|provider| provider.id.clone()),
        )),
        RouterKind::TaskKind => Arc::new(TaskKindRouter::new(
            settings.default_provider.clone(),
            settings.routing.by_task.clone(),
            settings
                .providers
                .iter()
                .map(|provider| (provider.id.clone(), provider.model.clone())),
        )),
    };

    Ok(Arc::new(RoutingProvider::new(destinations, router)))
}

/// One destination per provider, each backed by the failover chain minus
/// itself. A destination with nothing to fail over to stays unwrapped.
fn with_failover(
    retried: &BTreeMap<ProviderId, Arc<dyn LlmProvider>>,
    settings: &LlmSettings,
) -> Result<Vec<Arc<dyn LlmProvider>>, LlmError> {
    let chain = settings
        .routing
        .failover
        .iter()
        .map(|id| {
            retried.get(id).cloned().ok_or_else(|| {
                LlmError::Configuration(format!("failover provider `{id}` is not configured"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(retried
        .iter()
        .map(|(id, primary)| {
            let fallbacks: Vec<_> = chain
                .iter()
                .filter(|fallback| fallback.id() != *id)
                .cloned()
                .collect();
            if fallbacks.is_empty() {
                primary.clone()
            } else {
                Arc::new(FailoverProvider::new(
                    primary.clone(),
                    fallbacks,
                    settings.routing.failover_cooldown,
                )) as Arc<dyn LlmProvider>
            }
        })
        .collect())
}

fn build_client(
    provider: &ProviderSettings,
    settings: &LlmSettings,
) -> Result<Arc<dyn LlmProvider>, LlmError> {
    match provider.kind {
        ProviderKind::OpenAiCompatible => Ok(Arc::new(OpenAiCompatibleProvider::new(
            provider.id.clone(),
            provider.base_url.clone(),
            provider.api_key.clone(),
            provider.model.clone(),
            provider.max_tokens_field.clone(),
            settings.request_timeout,
        )?)),
        ProviderKind::Anthropic => {
            let api_key = provider.api_key.clone().ok_or_else(|| {
                LlmError::Configuration(format!(
                    "provider `{}` is Anthropic but has no API key",
                    provider.id
                ))
            })?;
            Ok(Arc::new(AnthropicProvider::new(
                provider.id.clone(),
                provider.base_url.clone(),
                api_key,
                provider.model.clone(),
                settings.request_timeout,
            )?))
        }
    }
}

//! Builds the provider stack from configuration.

use std::sync::Arc;

use agent_domain::error::LlmError;
use agent_domain::ports::llm::{LlmProvider, LlmRouter};

use crate::config::{LlmSettings, ProviderKind, ProviderSettings, RouterKind};

use super::{
    AnthropicProvider, ModelPrefixRouter, OpenAiCompatibleProvider, RetryingProvider,
    RoutingLlmProvider, StaticRouter,
};

/// Assembles `Retry(Route(clients...))`.
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

    let clients = settings
        .providers
        .iter()
        .map(|provider| build_client(provider, settings))
        .collect::<Result<Vec<_>, _>>()?;

    let router: Arc<dyn LlmRouter> = match settings.router {
        RouterKind::Static => Arc::new(StaticRouter::new(settings.default_provider.clone())),
        RouterKind::ModelPrefix => Arc::new(ModelPrefixRouter::new(
            settings.default_provider.clone(),
            settings
                .providers
                .iter()
                .map(|provider| provider.id.clone()),
        )),
    };

    let routing = Arc::new(RoutingLlmProvider::new(clients, router));
    Ok(Arc::new(RetryingProvider::new(
        routing,
        settings.max_retries,
    )))
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

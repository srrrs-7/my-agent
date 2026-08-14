//! LLM adapters and the composable pieces around them.
//!
//! ```text
//!   Arc<dyn LlmProvider>
//!     = RetryingProvider(          <- backoff on transient failures
//!         RoutingProvider(         <- picks a provider per request
//!           { "local": OpenAiCompatibleProvider,
//!             "cloud": AnthropicProvider }))
//! ```
//!
//! Each layer is an [`agent_domain::ports::llm::LlmProvider`] in its own right,
//! so the stack can be assembled, reordered or reduced to a single client
//! without any of the callers noticing. The HTTP plumbing the concrete clients
//! share lives in [`http`].

pub mod anthropic;
pub mod factory;
pub(crate) mod http;
pub mod openai;
pub mod retry;
pub mod routing;
pub(crate) mod sse;

pub use anthropic::AnthropicProvider;
pub use factory::build_provider;
pub use openai::OpenAiCompatibleProvider;
pub use retry::RetryingProvider;
pub use routing::{ModelPrefixRouter, RoutingProvider, StaticRouter};

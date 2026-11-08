//! LLM adapters and the composable pieces around them.
//!
//! ```text
//!   Arc<dyn LlmProvider>
//!     = RoutingProvider(                     <- picks a destination per request
//!         { "cloud": FailoverProvider(       <- crosses destinations (opt-in)
//!                      RetryingProvider(AnthropicProvider),   <- backoff
//!                      [RetryingProvider(OpenAiCompatibleProvider)]),
//!           "local": RetryingProvider(OpenAiCompatibleProvider) })
//! ```
//!
//! Each layer is an [`agent_domain::ports::llm::LlmProvider`] in its own right,
//! so the stack can be assembled, reordered or reduced to a single client
//! without any of the callers noticing. The HTTP plumbing the concrete clients
//! share lives in [`http`].

pub mod anthropic;
pub mod factory;
pub mod failover;
pub(crate) mod http;
pub mod openai;
pub mod retry;
pub mod routing;
pub(crate) mod sse;

pub use anthropic::{AnthropicProvider, PromptCache};
pub use factory::build_provider;
pub use failover::{Clock, FailoverProvider, SystemClock};
pub use openai::OpenAiCompatibleProvider;
pub use retry::RetryingProvider;
pub use routing::{ModelPrefixRouter, RoutingProvider, StaticRouter, TaskKindRouter};

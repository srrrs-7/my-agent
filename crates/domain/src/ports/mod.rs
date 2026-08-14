//! Ports - the interfaces the domain offers to (and demands from) the outside.
//!
//! Every trait here is object safe (`dyn`-compatible) so that the composition
//! root can swap implementations at runtime: a local Ollama for an Anthropic
//! endpoint, a real filesystem for an in-memory fake in tests, an interactive
//! terminal prompt for an auto-approving policy in CI.

pub mod approval;
pub mod context;
pub mod events;
pub mod file_system;
pub mod llm;
pub mod prompt;
pub mod search;
pub mod tool;
pub mod web;

pub use approval::{ApprovalDecision, ApprovalGate, ApprovalRequest};
pub use context::ContextProvider;
pub use events::{AgentEvent, EventSink, FinishReason};
pub use file_system::{DirEntry, EntryKind, FileSystem};
pub use llm::{ChatStream, LlmProvider, LlmRouter, RouteDecision, StreamEvent};
pub use prompt::PromptBuilder;
pub use search::{FileSearcher, SearchHit, SearchQuery};
pub use tool::Tool;
pub use web::{FetchedContent, WebFetcher};

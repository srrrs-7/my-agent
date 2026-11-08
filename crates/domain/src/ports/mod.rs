//! Ports - the interfaces the domain offers to (and demands from) the outside.
//!
//! Every trait here is object safe (`dyn`-compatible) so that the composition
//! root can swap implementations at runtime: a local Ollama for an Anthropic
//! endpoint, a real filesystem for an in-memory fake in tests, an interactive
//! terminal prompt for an auto-approving policy in CI.

pub mod approval;
pub mod command;
pub mod context;
pub mod conversation_log;
pub mod events;
pub mod file_system;
pub mod llm;
pub mod notify;
pub mod prompt;
pub mod search;
pub mod session_store;
pub mod tool;
pub mod tool_server;
pub mod web;

pub use approval::{ApprovalDecision, ApprovalGate, ApprovalRequest, DependencyApprovalRequest};
pub use command::{CommandOutput, CommandRequest, CommandRunner, SandboxKind};
pub use context::ContextProvider;
pub use conversation_log::{ConversationLog, NullConversationLog};
pub use events::{AgentEvent, EventSink, FinishReason};
pub use file_system::{DirEntry, EntryKind, FileSystem};
pub use llm::{ChatStream, LlmProvider, LlmRouter, RouteDecision, StreamEvent};
pub use notify::{Notification, NotificationEvent, NotificationKind, Notifier, NullNotifier};
pub use prompt::PromptBuilder;
pub use search::{FileSearcher, GlobMatches, GlobQuery, SearchHit, SearchQuery};
pub use session_store::{SessionStore, SessionSummary};
pub use tool::Tool;
pub use tool_server::{RemoteContent, RemoteTool, RemoteToolOutput, ToolServer};
pub use web::{
    FetchedContent, WebFetcher, WebSearchError, WebSearchHit, WebSearchQuery, WebSearchResults,
    WebSearcher,
};

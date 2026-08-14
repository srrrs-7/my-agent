//! # agent-domain
//!
//! The innermost layer of the clean architecture. It contains the language the
//! whole system is written in - entities, value objects and the *ports*
//! (traits) through which the outside world is reached.
//!
//! ## Rules
//!
//! * It must not depend on any other crate of this workspace.
//! * It must not know about HTTP, tokio, the filesystem, or any concrete LLM
//!   vendor. Those live behind the ports in [`ports`].
//! * The only external dependencies allowed are data-shape helpers
//!   (`serde`, `serde_json` for JSON-Schema tool definitions), `thiserror`,
//!   `async-trait` for object-safe async ports, and `futures-core` for the
//!   `Stream` trait streamed responses are expressed in. All of them are
//!   runtime-agnostic; anything that needs an executor stays outside.

pub mod error;
pub mod model;
pub mod ports;
pub mod text;

pub use error::{
    ApprovalError, CommandError, DomainError, FetchError, FsError, LlmError, ToolError,
};
pub use model::{
    context::ContextSnapshot,
    conversation::Conversation,
    llm::{
        ChatRequest, ChatResponse, GenerationParams, ModelId, ProviderCapabilities, ProviderId,
        RequestMetadata, StopReason, TaskKind, TokenUsage,
    },
    message::{ContentBlock, Message, Role},
    tool::{ToolCall, ToolCallId, ToolDefinition, ToolName, ToolOutcome, ToolResult, ToolSafety},
    workspace::{WorkspacePath, WorkspaceRoot},
};
pub use ports::{
    approval::{ApprovalDecision, ApprovalGate, ApprovalRequest},
    command::{CommandOutput, CommandRequest, CommandRunner, SandboxKind},
    context::ContextProvider,
    events::{AgentEvent, EventSink, FinishReason, NullEventSink},
    file_system::{DirEntry, EntryKind, FileSystem},
    llm::{ChatStream, LlmProvider, LlmRouter, RouteDecision, StreamEvent},
    prompt::PromptBuilder,
    search::{FileSearcher, SearchHit, SearchQuery},
    tool::Tool,
    web::{FetchedContent, WebFetcher},
};

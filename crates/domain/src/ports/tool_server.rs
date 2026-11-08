//! Tool-server port: a long-lived process that offers tools of its own.
//!
//! Unlike [`super::command::CommandRunner`], which runs one command and
//! returns its output, a tool server stays up for the whole session and is
//! spoken to many times. Everything it says is untrusted: the tool names,
//! their descriptions and schemas, and every result. Turning that into
//! [`super::tool::Tool`]s the model may call - naming, safety class,
//! sanitising - is a use case and lives in the application layer; the wire
//! protocol and the process live in infrastructure. This port is the seam
//! between them and deliberately names neither.
//!
//! [`ToolServer::sandbox`] reports the confinement actually in force, like
//! [`super::command::CommandRunner::sandbox`].

use async_trait::async_trait;
use serde_json::Value;

use crate::error::ToolServerError;
use crate::ports::command::SandboxKind;

/// One tool as the server describes it, before anything is checked.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteTool {
    /// The server's own name for it. Kept verbatim, because calls must use
    /// it even when the name shown to the model had to be rewritten.
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// One piece of a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteContent {
    Text(String),
    /// A kind this client does not render (an image, an embedded resource).
    /// Kept as a marker so the model learns something was left out.
    Unsupported {
        kind: String,
    },
}

/// What a call produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteToolOutput {
    pub content: Vec<RemoteContent>,
    /// The server reported that the tool itself failed. Distinct from a
    /// [`ToolServerError`], where no usable answer came back at all.
    pub is_error: bool,
}

#[async_trait]
pub trait ToolServer: Send + Sync {
    /// The operator's name for this server, already validated.
    fn server_name(&self) -> &str;

    /// The confinement actually in force for the server process.
    fn sandbox(&self) -> SandboxKind;

    /// The tools the server offered at startup. Fixed for the life of the
    /// server: a list that could change mid-session would change the system
    /// prompt under the model and skip every startup check.
    fn tools(&self) -> Vec<RemoteTool>;

    /// Calls `tool` (the server's raw name) with `arguments`. Dropping the
    /// returned future abandons the call.
    async fn call(&self, tool: &str, arguments: Value)
    -> Result<RemoteToolOutput, ToolServerError>;
}

//! One MCP server process: started confined, spoken to over stdio, taken
//! down with its whole process group.
//!
//! ## Confinement
//!
//! The same mechanism as `run_command` ([`crate::exec::confined`]), with a
//! narrower grant. The only writable root is a directory of the server's own,
//! unless the operator opted the workspace in: a server stays up between
//! calls, where neither the approval prompt nor the dependency watch looks.
//! Network goes only through a proxy of the server's own, limited to its
//! `allow_hosts`; with none, there is no proxy and no network at all. Reads
//! are not confined - see [`super::config::WorkspaceAccess`].
//!
//! ## Lifetime
//!
//! Started once, handshaken once, and its tool list fixed. A server that
//! crashes stays down: every later call fails with the reason. Dropping the
//! handle closes the server's stdin (the protocol's shutdown signal) and then
//! kills its process group, grandchildren included - and, where
//! [`crate::exec::detached_reaping`] holds, the descendants that left the
//! group with `setsid` as well.
//!
//! The child is never reaped before that. A process that has exited but not
//! been waited for keeps its pid, and with it the process-group id the kill is
//! aimed at, so the kill can never land on an unrelated process that was
//! handed a recycled pid. The cost is one zombie per crashed server until the
//! session ends.

use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use agent_domain::error::{CommandError, ToolServerError};
use agent_domain::ports::command::SandboxKind;
use agent_domain::ports::tool_server::{RemoteContent, RemoteTool, RemoteToolOutput, ToolServer};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinHandle;

use super::config::{ServerConfig, WorkspaceAccess};
use super::rpc::{MAX_MESSAGE_BYTES, RpcClient, RpcError};
use crate::exec::confined::{ConfinementPolicy, confine_command};
use crate::exec::process::GuardedChild;
use crate::exec::proxy::EgressProxy;
use crate::fs::scratch::ScratchDir;
use crate::net::guard::HostPolicy;

/// The protocol revision this client speaks. The server's answer is accepted
/// whatever it is: the subset used here (`tools/list`, `tools/call`, text
/// content) has not changed between revisions.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// How long a server gets to start, handshake and list its tools.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Pages of `tools/list` read before giving up on a server that keeps
/// returning a cursor.
const MAX_LIST_PAGES: usize = 16;

/// The tail of the server's stderr kept for a startup failure message.
const STDERR_TAIL_BYTES: usize = 2048;

/// How long a failed startup waits for the server's last words on stderr.
const STDERR_GRACE: Duration = Duration::from_millis(500);

/// Why a server could not be started. Read by the operator.
#[derive(Debug, Error)]
pub enum McpStartError {
    #[error("MCP server `{server}`: {source}")]
    Sandbox {
        server: String,
        #[source]
        source: CommandError,
    },

    #[error("MCP server `{server}`: {message}")]
    Setup { server: String, message: String },

    #[error("MCP server `{server}` did not complete `{step}`: {reason}{}", stderr_note(.stderr))]
    Handshake {
        server: String,
        step: &'static str,
        reason: String,
        /// The end of what the server wrote to stderr, cleaned for a
        /// terminal. Usually the actual explanation.
        stderr: String,
    },
}

fn stderr_note(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!(". Its stderr ended with: {stderr}")
    }
}

/// A running, handshaken server.
pub struct McpServer {
    name: String,
    kind: SandboxKind,
    tools: Vec<RemoteTool>,
    rpc: RpcClient,
    child: Option<GuardedChild>,
    stderr: JoinHandle<()>,
    egress: Option<EgressProxy>,
    /// Last, so that it is removed after the drop has killed the process
    /// group that writes into it.
    dir: ScratchDir,
}

impl McpServer {
    /// Starts `config` under `kind` - which must already be the outcome of
    /// [`crate::exec::resolve_sandbox`] - and completes the handshake.
    ///
    /// `workspace` is the canonical workspace root; it is writable for the
    /// server only when the configuration says so. `withheld` are the
    /// operator's configured secrets, kept out of the inherited environment
    /// (see [`crate::exec::ExecConfig::withheld`]).
    pub async fn start(
        name: &str,
        config: &ServerConfig,
        kind: SandboxKind,
        workspace: &Path,
        withheld: &[String],
    ) -> Result<Self, McpStartError> {
        let setup = |message: String| McpStartError::Setup {
            server: name.to_string(),
            message,
        };

        // Owner-only and always new: a name already taken in the shared temp
        // dir - by anyone, a planted link included - is skipped, never
        // reused. Removed on every exit path from here on.
        let dir = ScratchDir::empty(&format!("my-agent-mcp-{name}-"))
            .map_err(|error| setup(format!("cannot create its directory: {error}")))?;

        let egress = if config.allow_hosts.is_empty() {
            None
        } else {
            let policy = HostPolicy::allowing(config.allow_hosts.clone());
            Some(
                EgressProxy::start(policy)
                    .await
                    .map_err(|error| McpStartError::Sandbox {
                        server: name.to_string(),
                        source: CommandError::SandboxUnavailable {
                            reason: format!("cannot start the egress proxy: {error}"),
                        },
                    })?,
            )
        };

        let own = dir.path().to_path_buf();
        let mut writable = vec![own.clone()];
        if config.workspace_access == WorkspaceAccess::ReadWrite {
            writable.push(workspace.to_path_buf());
        }
        let policy = ConfinementPolicy {
            kind,
            writable,
            scratch: own.clone(),
            proxy_port: egress.as_ref().map(EgressProxy::port),
            withheld: withheld.to_vec(),
        };

        // The server's own directory, always: a relative path in `args`
        // must not resolve into the workspace.
        let mut command = Command::new(&config.command);
        command.args(&config.args).current_dir(&own);
        let mut command =
            confine_command(command, &policy).map_err(|source| McpStartError::Sandbox {
                server: name.to_string(),
                source,
            })?;
        // After confinement, which scrubs the environment: these are the
        // operator's explicit grant. `inherited` holds secrets the scrub has
        // just removed by value; this server is the one they are meant for.
        command.envs(&config.env).envs(&config.inherited);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut guarded = GuardedChild::spawn(command).map_err(|error| {
            setup(format!(
                "cannot start `{}`: {error}",
                config.command.display()
            ))
        })?;
        let (Some(stdin), Some(stdout), Some(stderr)) = (
            guarded.child.stdin.take(),
            guarded.child.stdout.take(),
            guarded.child.stderr.take(),
        ) else {
            return Err(setup("its standard streams were not piped".to_string()));
        };

        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let mut server = Self {
            name: name.to_string(),
            kind,
            tools: Vec::new(),
            rpc: RpcClient::start(stdout, stdin, MAX_MESSAGE_BYTES),
            child: Some(guarded),
            stderr: tokio::spawn(drain(stderr, tail.clone())),
            egress,
            dir,
        };

        match tokio::time::timeout(STARTUP_TIMEOUT, server.handshake()).await {
            Ok(Ok(())) => Ok(server),
            Ok(Err((step, reason))) => {
                // Usually the server has exited and its stderr is about to
                // end; wait a moment for the explanation it printed.
                let _ = tokio::time::timeout(STDERR_GRACE, &mut server.stderr).await;
                Err(McpStartError::Handshake {
                    server: name.to_string(),
                    step,
                    reason,
                    stderr: stderr_tail(&tail),
                })
            }
            Err(_) => Err(McpStartError::Handshake {
                server: name.to_string(),
                step: "startup",
                reason: format!("no answer within {}s", STARTUP_TIMEOUT.as_secs()),
                stderr: stderr_tail(&tail),
            }),
        }
    }

    /// `initialize`, `notifications/initialized`, then every page of
    /// `tools/list`.
    async fn handshake(&mut self) -> Result<(), (&'static str, String)> {
        let initialized = self
            .rpc
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "my-agent", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await
            .map_err(|error| ("initialize", describe(&error)))?;
        if !initialized.is_object() {
            return Err(("initialize", "the result is not an object".to_string()));
        }
        self.rpc
            .notify("notifications/initialized", json!({}))
            .await
            .map_err(|error| ("initialize", describe(&error)))?;

        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let page = self
                .rpc
                .request("tools/list", params)
                .await
                .map_err(|error| ("tools/list", describe(&error)))?;
            let page: ListPage = serde_json::from_value(page)
                .map_err(|error| ("tools/list", format!("the result is not valid: {error}")))?;
            self.tools
                .extend(page.tools.into_iter().map(|tool| RemoteTool {
                    name: tool.name,
                    description: tool.description.unwrap_or_default(),
                    input_schema: tool.input_schema,
                }));
            match page.next_cursor {
                Some(next) if !next.is_empty() => cursor = Some(next),
                _ => return Ok(()),
            }
        }
        Err((
            "tools/list",
            format!("the list did not end within {MAX_LIST_PAGES} pages"),
        ))
    }

    /// Loopback port of this server's egress proxy, `None` when it has no
    /// network. For `doctor` and for tests that check the kernel rule.
    pub fn egress_port(&self) -> Option<u16> {
        self.egress.as_ref().map(EgressProxy::port)
    }

    /// The server's own writable directory.
    pub fn directory(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        // Stdin first: closing it is how the protocol asks a server to exit.
        // Then the group, without waiting - a server that ignores the request
        // is not allowed to keep running, and `Drop` cannot wait anyway.
        self.rpc.close();
        drop(self.child.take());
        self.stderr.abort();
    }
}

#[async_trait]
impl ToolServer for McpServer {
    fn server_name(&self) -> &str {
        &self.name
    }

    fn sandbox(&self) -> SandboxKind {
        self.kind
    }

    fn tools(&self) -> Vec<RemoteTool> {
        self.tools.clone()
    }

    async fn call(
        &self,
        tool: &str,
        arguments: Value,
    ) -> Result<RemoteToolOutput, ToolServerError> {
        let result = self
            .rpc
            .request("tools/call", json!({"name": tool, "arguments": arguments}))
            .await
            .map_err(|error| match error {
                RpcError::Closed(reason) => ToolServerError::Stopped {
                    server: self.name.clone(),
                    reason,
                },
                RpcError::Remote { message, .. } => ToolServerError::Refused {
                    server: self.name.clone(),
                    message,
                },
            })?;
        let result: CallResult =
            serde_json::from_value(result).map_err(|error| ToolServerError::Protocol {
                server: self.name.clone(),
                reason: error.to_string(),
            })?;
        Ok(RemoteToolOutput {
            content: result.content.iter().map(content).collect(),
            is_error: result.is_error,
        })
    }
}

fn describe(error: &RpcError) -> String {
    match error {
        RpcError::Closed(reason) => reason.clone(),
        RpcError::Remote { code, message } => format!("error {code}: {message}"),
    }
}

/// One content item. Only text is carried; the rest is named so the model
/// knows something was left out.
fn content(item: &Value) -> RemoteContent {
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match (kind, item.get("text").and_then(Value::as_str)) {
        ("text", Some(text)) => RemoteContent::Text(text.to_string()),
        _ => RemoteContent::Unsupported {
            kind: agent_domain::text::truncate(kind, 32).to_string(),
        },
    }
}

#[derive(Deserialize)]
struct ListPage {
    #[serde(default)]
    tools: Vec<WireTool>,
    #[serde(default, rename = "nextCursor")]
    next_cursor: Option<String>,
}

/// Annotations (`readOnlyHint` and the rest) are deliberately not read: the
/// safety class is decided by this client, not claimed by the server.
#[derive(Deserialize)]
struct WireTool {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "empty_schema", rename = "inputSchema")]
    input_schema: Value,
}

fn empty_schema() -> Value {
    json!({"type": "object"})
}

#[derive(Deserialize)]
struct CallResult {
    #[serde(default)]
    content: Vec<Value>,
    #[serde(default, rename = "isError")]
    is_error: bool,
}

/// Reads stderr until it ends, keeping only the tail. A server that writes
/// to a pipe nobody reads blocks once the pipe fills; this keeps it running.
async fn drain<R: AsyncRead + Unpin>(mut stderr: R, tail: Arc<Mutex<VecDeque<u8>>>) {
    let mut chunk = [0_u8; 4096];
    while let Ok(read) = stderr.read(&mut chunk).await {
        if read == 0 {
            break;
        }
        let mut tail = tail.lock().unwrap_or_else(PoisonError::into_inner);
        tail.extend(&chunk[..read]);
        let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
        tail.drain(..excess);
    }
}

/// The kept stderr as one line a terminal can show safely: the server wrote
/// it, so escape sequences and other control characters are removed.
fn stderr_tail(tail: &Mutex<VecDeque<u8>>) -> String {
    let bytes: Vec<u8> = tail
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .copied()
        .collect();
    let text = String::from_utf8_lossy(&bytes);
    let mut clean = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\n' || c == '\t' {
            clean.push(' ');
        } else if !c.is_control() {
            clean.push(c);
        }
    }
    clean.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_text_content_is_carried() {
        assert_eq!(
            content(&json!({"type": "text", "text": "hi"})),
            RemoteContent::Text("hi".into())
        );
        assert_eq!(
            content(&json!({"type": "image", "data": "...", "mimeType": "image/png"})),
            RemoteContent::Unsupported {
                kind: "image".into()
            }
        );
        assert_eq!(
            content(&json!({"text": "no type"})),
            RemoteContent::Unsupported {
                kind: "unknown".into()
            }
        );
    }

    #[test]
    fn the_stderr_tail_is_safe_to_print() {
        let tail = Mutex::new(VecDeque::from(b"boom\x1b[2J\nline two\x07".to_vec()));
        assert_eq!(stderr_tail(&tail), "boom[2J line two");
    }
}

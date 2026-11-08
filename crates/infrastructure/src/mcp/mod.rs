//! MCP servers over stdio, each inside the OS sandbox.
//!
//! The [`agent_domain::ports::tool_server::ToolServer`] implementation. What
//! is here is the transport ([`rpc`]), the process ([`server`]) and the
//! operator's configuration ([`config`]); deciding how a server's tools are
//! named, classified and cleaned up for the model is the application
//! layer's (`agent_application::tools::server`).
//!
//! The protocol is written by hand on `serde_json` rather than taken from an
//! SDK crate: the subset used - a handshake, a paged list and a call - is a
//! few hundred lines, and an SDK would be supply-chain surface in the one
//! component whose whole job is to keep untrusted code contained.
//!
//! Linux only, for now: the servers need the same confinement as
//! `run_command`, and macOS has none implemented yet. Asking for servers
//! where the sandbox is missing fails startup.

pub mod config;
pub(crate) mod rpc;
pub mod server;

use std::path::Path;
use std::sync::Arc;

use agent_domain::ports::command::SandboxKind;

pub use config::{McpConfig, McpConfigError, ServerConfig, WorkspaceAccess};
pub use server::{McpServer, McpStartError};

use crate::exec::{SandboxRequirement, resolve_sandbox};

/// Starts every configured server, in name order, or none.
///
/// The sandbox is resolved once for all of them; a platform that cannot meet
/// `requirement` fails here, before any server runs. Any server failing to
/// start fails the whole call, and the ones already started are stopped as
/// the partial list is dropped: an agent missing tools the operator
/// configured would behave differently from what they set up, without
/// saying so.
///
/// `withheld` are the operator's configured secrets; see
/// [`McpServer::start`].
pub async fn start_servers(
    config: &McpConfig,
    requirement: SandboxRequirement,
    workspace: &Path,
    withheld: &[String],
) -> Result<Vec<Arc<McpServer>>, McpStartError> {
    if config.servers.is_empty() {
        return Ok(Vec::new());
    }
    let kind = resolve_kind(requirement, config)?;
    let mut started = Vec::with_capacity(config.servers.len());
    for (name, server) in &config.servers {
        started.push(Arc::new(
            McpServer::start(name, server, kind, workspace, withheld).await?,
        ));
    }
    Ok(started)
}

fn resolve_kind(
    requirement: SandboxRequirement,
    config: &McpConfig,
) -> Result<SandboxKind, McpStartError> {
    // `Disabled` is refused when the settings are read; refused again here so
    // that no other caller can get an unconfined server by passing it.
    let refused = |source| McpStartError::Sandbox {
        server: config.servers.keys().next().cloned().unwrap_or_default(),
        source,
    };
    if requirement == SandboxRequirement::Disabled {
        return Err(refused(
            agent_domain::error::CommandError::SandboxUnavailable {
                reason: "MCP servers cannot run without a sandbox".to_string(),
            },
        ));
    }
    resolve_sandbox(requirement).map_err(refused)
}

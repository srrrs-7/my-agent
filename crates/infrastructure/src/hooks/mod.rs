//! The operator's hooks: commands run around the agent's tool calls.
//!
//! Three events, each wired where it cannot be bypassed and cannot widen
//! anything:
//!
//! * **`pre_tool_use`** - [`HookGate`], an approval-gate decorator. It runs
//!   before the call is put to the operator's rules or to a human, and a hook
//!   that exits non-zero - or fails to run, or runs out of time - refuses the
//!   call, its stderr becoming the reason the model reads. A hook that exits
//!   0 only lets the question go on inward: **a hook can refuse, never
//!   approve.** The dependency question is never put to hooks at all.
//! * **`post_tool_use`** - [`HookedTool`], a tool decorator. It observes: the
//!   tool's result reaches the model unchanged whatever the hook does, and a
//!   hook that fails is one warning line.
//! * **`stop`** - [`Hooks::stop`], awaited by the subcommand when a turn
//!   ends, next to the notification. Not from an `EventSink`: a sink only
//!   narrates, and a sink that started processes would be a second, hidden
//!   way for events to have effects. A stop hook cannot make the turn go on.
//!
//! Every hook gets one JSON document on stdin (`"v": 1`) and runs under the
//! sandbox [`HookRunner`] describes.
//!
//! ## Which call a post hook is about
//!
//! A tool is handed its arguments, not the call: `Tool::execute` knows no
//! call id. The gate does, and every call that runs was approved by it just
//! before, so the gate notes `(tool, arguments) -> call id` for each call it
//! lets through to a tool that has post hooks, and [`HookedTool`] takes the
//! note back. Two calls with the same tool and the same arguments in one turn
//! are indistinguishable to a hook anyway; which of their ids each gets does
//! not matter. A call the note cannot be found for (a tool run outside the
//! dispatcher) is reported with `"call_id": null`.

mod config;
mod gate;
mod runner;
mod tool;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use agent_domain::error::CommandError;
use agent_domain::model::tool::{ToolCallId, ToolName, ToolSafety};
use agent_domain::ports::command::{CommandOutput, SandboxKind};
use agent_domain::text;
use serde_json::{Value, json};
use tracing::warn;

pub use config::{
    DEFAULT_TIMEOUT_SECS, HooksConfig, HooksError, MAX_HOOKS_BYTES, MAX_HOOKS_PER_EVENT,
    MAX_TIMEOUT_SECS, StopHook, ToolHook,
};
pub use gate::HookGate;
pub use runner::HookRunner;
pub use tool::HookedTool;

use crate::exec::SandboxRequirement;
use crate::secrets::Redactor;

/// The version of the stdin document. Raised only for a change a hook
/// written against the old one would misread.
pub const PAYLOAD_VERSION: u64 = 1;

/// The most of a tool's output a post hook is handed, after redaction.
pub const MAX_POST_OUTPUT_BYTES: usize = 64 * 1024;

/// The most of a refusing hook's stderr the model is given as the reason.
const MAX_REASON_BYTES: usize = 1024;

/// Calls noted for their post hooks and not yet taken back. A call that was
/// approved but never run (the turn was interrupted between the two) leaves
/// its note behind; the cap keeps those from piling up.
const MAX_PENDING_CALLS: usize = 256;

/// Everything the three events share: the configuration, the runner, the
/// session the current turn belongs to, and the notes the gate leaves for
/// the post hooks.
pub struct Hooks {
    config: HooksConfig,
    runner: HookRunner,
    redactor: Arc<Redactor>,
    /// Set by the subcommand when a turn begins: neither the gate nor a tool
    /// is called with a session.
    session_id: Mutex<String>,
    pending: Mutex<VecDeque<(ToolName, Value, ToolCallId)>>,
}

impl Hooks {
    /// Fails when the platform cannot provide `requirement`, so a hook never
    /// runs with less confinement than the operator asked for.
    pub fn start(
        config: HooksConfig,
        requirement: SandboxRequirement,
        workspace: &Path,
        withheld: Vec<String>,
        redactor: Arc<Redactor>,
    ) -> Result<Self, CommandError> {
        Ok(Self {
            config,
            runner: HookRunner::new(requirement, workspace, withheld)?,
            redactor,
            session_id: Mutex::new(String::new()),
            pending: Mutex::new(VecDeque::new()),
        })
    }

    pub fn config(&self) -> &HooksConfig {
        &self.config
    }

    /// The confinement hooks actually run under.
    pub fn sandbox(&self) -> SandboxKind {
        self.runner.sandbox()
    }

    /// A human just handed the agent a prompt in `session_id`. Notes left by
    /// an earlier turn are about calls that will never run now.
    pub fn begin_turn(&self, session_id: &str) {
        *lock(&self.session_id) = session_id.to_string();
        lock(&self.pending).clear();
    }

    /// Whether any post hook is about `tool` - the tools worth wrapping.
    pub fn observes(&self, tool: &str) -> bool {
        self.config
            .post_tool_use
            .iter()
            .any(|hook| hook.matches(tool))
    }

    /// Runs every stop hook, in order, each bounded by its own timeout.
    /// `reason` says how the turn ended (`completed`, `interrupted`, ...).
    /// A hook that fails is a warning; nothing it does changes the turn.
    pub async fn stop(&self, session_id: &str, reason: &str) {
        let payload = json!({
            "v": PAYLOAD_VERSION,
            "event": "stop",
            "session_id": session_id,
            "reason": reason,
        });
        for hook in &self.config.stop {
            let result = self
                .runner
                .run(&hook.command, encode(&payload), hook.timeout())
                .await;
            self.warn_on_failure("stop", &hook.command, result);
        }
    }

    fn session_id(&self) -> String {
        lock(&self.session_id).clone()
    }

    fn note(&self, tool: &ToolName, arguments: &Value, call_id: &ToolCallId) {
        let mut pending = lock(&self.pending);
        if pending.len() == MAX_PENDING_CALLS {
            pending.pop_front();
        }
        pending.push_back((tool.clone(), arguments.clone(), call_id.clone()));
    }

    fn take_note(&self, tool: &ToolName, arguments: &Value) -> Option<ToolCallId> {
        let mut pending = lock(&self.pending);
        let index = pending
            .iter()
            .position(|(name, noted, _)| name == tool && noted == arguments)?;
        pending.remove(index).map(|(_, _, call_id)| call_id)
    }

    /// The document a tool hook reads, before the post hook's additions.
    fn tool_payload(
        &self,
        event: &str,
        tool: &ToolName,
        safety: ToolSafety,
        call_id: Option<&ToolCallId>,
        arguments: &Value,
    ) -> Value {
        json!({
            "v": PAYLOAD_VERSION,
            "event": event,
            "session_id": self.session_id(),
            "tool": tool.as_str(),
            "safety": safety,
            "call_id": call_id.map(ToolCallId::as_str),
            "arguments": arguments,
        })
    }

    /// One line on stderr for a hook that did not exit 0. Never more: a
    /// post or stop hook observes, and its failure is the operator's to fix,
    /// not the model's to work around.
    fn warn_on_failure(
        &self,
        event: &str,
        command: &str,
        result: Result<CommandOutput, CommandError>,
    ) {
        let problem = match result {
            Ok(output) if output.exit_code == Some(0) => return,
            Ok(output) => {
                let said = self.said(&output.stderr);
                match (output.exit_code, said.is_empty()) {
                    (Some(code), true) => format!("exited with code {code}"),
                    (Some(code), false) => format!("exited with code {code}: {said}"),
                    (None, _) => "was killed by a signal".to_string(),
                }
            }
            Err(error) => error.to_string(),
        };
        warn!(
            "{event} hook `{}` {problem}",
            text::printable(&text::clip(command, 120))
        );
    }

    /// The head of what a hook wrote to stderr, on one line, with known
    /// secrets hidden: it goes to the model and to the terminal.
    fn said(&self, stderr: &str) -> String {
        let redacted = self.redactor.redact(stderr);
        let one_line = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
        text::printable(&text::clip(&one_line, MAX_REASON_BYTES))
    }
}

fn encode(payload: &Value) -> Vec<u8> {
    let mut bytes = payload.to_string().into_bytes();
    // A trailing newline, so `read line` in a shell hook sees a whole line.
    bytes.push(b'\n');
    bytes
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // The data is a string and a queue of notes; a panic elsewhere leaves
    // them usable.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

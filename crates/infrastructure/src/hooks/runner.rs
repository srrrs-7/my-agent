//! Running one hook: a `/bin/sh -c` line, confined like `run_command`, with
//! a JSON document on its stdin.
//!
//! Everything that makes a command safe to start is borrowed from
//! [`crate::exec`] rather than repeated: the same shell invocation, the same
//! [`confine_command`] (scrubbed environment, scratch directory, kernel
//! rules), the same capture with its cap, its timeout and its group kill.
//! The one thing a hook needs that a model's command never gets is stdin,
//! which is why this does not go through `CommandRequest`.
//!
//! ## What a hook may write
//!
//! The workspace and a scratch directory of the hooks' own. The workspace
//! because the use a post hook is for is a formatter or a linter's `--fix`
//! run on the file the model just wrote - a hook that could not write the
//! workspace could only observe, and observing needs no hook. Nothing else:
//! not the operator's home, not the shared `/tmp`. A hook that keeps state
//! between runs keeps it in the workspace or does without.
//!
//! ## No network
//!
//! There is no egress proxy, so a confined hook cannot connect anywhere.
//! A hook reads the model's arguments and a tool's output; one that could
//! send them on would be an exfiltration path the operator did not ask for.
//! Opening one is a feature for a later version, with its own allowlist.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_domain::error::CommandError;
use agent_domain::ports::command::{CommandOutput, SandboxKind};

use crate::exec::capture::run_capped_with_input;
use crate::exec::confined::{ConfinementPolicy, confine_command};
use crate::exec::{SandboxRequirement, resolve_sandbox, shell_line};
use crate::fs::scratch::ScratchDir;

/// What is kept of a hook's stdout and stderr together. A hook speaks
/// through its exit code; this is for the reason it gives when it refuses.
const MAX_HOOK_OUTPUT_BYTES: usize = 64 * 1024;

/// Runs hook commands under one confinement for the life of the process.
pub struct HookRunner {
    workspace: PathBuf,
    kind: SandboxKind,
    withheld: Vec<String>,
    /// Last, so it is removed after anything that could still write to it.
    scratch: ScratchDir,
}

impl HookRunner {
    /// Fails when the platform cannot provide `requirement`, so a hook never
    /// runs with less confinement than the operator asked for.
    ///
    /// `workspace` is the canonical workspace root; `withheld` the operator's
    /// configured secrets, removed from the hook's environment by value.
    pub fn new(
        requirement: SandboxRequirement,
        workspace: &Path,
        withheld: Vec<String>,
    ) -> Result<Self, CommandError> {
        let kind = resolve_sandbox(requirement)?;
        let scratch = ScratchDir::empty("my-agent-hooks-").map_err(|error| CommandError::Io {
            message: format!("cannot create the hooks' scratch directory: {error}"),
        })?;
        Ok(Self {
            workspace: workspace.to_path_buf(),
            kind,
            withheld,
            scratch,
        })
    }

    /// The confinement actually in force - not the configured requirement.
    pub fn sandbox(&self) -> SandboxKind {
        self.kind
    }

    /// Runs `command` in the workspace with `input` on its stdin.
    pub async fn run(
        &self,
        command: &str,
        input: Vec<u8>,
        timeout: Duration,
    ) -> Result<CommandOutput, CommandError> {
        let policy = ConfinementPolicy {
            kind: self.kind,
            writable: vec![self.workspace.clone(), self.scratch.path().to_path_buf()],
            scratch: self.scratch.path().to_path_buf(),
            proxy_port: None,
            withheld: self.withheld.clone(),
        };
        let confined = confine_command(shell_line(command, &self.workspace), &policy)?;
        run_capped_with_input(confined, Some(input), timeout, MAX_HOOK_OUTPUT_BYTES).await
    }
}

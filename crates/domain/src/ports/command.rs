//! Command-execution port.
//!
//! Running a command is the one capability that steps outside every guarantee
//! the file tools provide: `WorkspacePath` confines *our* file access at the
//! type level, but a child process is free of it. The confinement therefore
//! has to come from the operating system, and this port is where that fact is
//! made visible rather than assumed.
//!
//! [`CommandRunner::sandbox`] reports what is *actually* in force, not what
//! was configured. The composition root refuses to register the tool when the
//! answer is weaker than the operator asked for, and the reported kind travels
//! into the tool description and `agent doctor`, so nobody - human or model -
//! has to guess how much protection they have.

use std::time::Duration;

use async_trait::async_trait;

use crate::error::CommandError;
use crate::model::workspace::WorkspacePath;

/// The confinement a [`CommandRunner`] achieved for its children.
///
/// Deliberately *not* ordered. Seatbelt and Landlock are peers - one per
/// platform, neither weaker than the other - so any total order would make
/// "at least Landlock" refuse to start on macOS, which is the opposite of what
/// an operator asking for confinement meant. Strength comparisons go through
/// [`Self::confines`] and [`Self::isolates`] instead, which say what is
/// actually true of each mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxKind {
    /// No confinement. Only ever chosen when an operator asks for it.
    None,
    /// macOS Seatbelt: filesystem confinement and egress limited to the proxy.
    Seatbelt,
    /// Linux Landlock: filesystem confinement, plus TCP connect limited to the
    /// proxy's port. Port-scoped, so a determined child can still reach a
    /// different host on that same port - see `NetnsProxied` for the airtight
    /// version.
    LandlockConfined,
    /// Linux network namespace with only the proxy plumbed in. Nothing but the
    /// proxy is reachable at all.
    NetnsProxied,
}

impl SandboxKind {
    /// The mechanism confines the filesystem and limits egress to the proxy.
    pub fn confines(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Nothing outside the proxy is reachable *at all* - the port-scoped gap
    /// in [`Self::LandlockConfined`] is closed. Only the namespace tier can
    /// claim this.
    pub fn isolates(self) -> bool {
        matches!(self, Self::NetnsProxied)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Seatbelt => "seatbelt",
            Self::LandlockConfined => "landlock",
            Self::NetnsProxied => "netns",
        }
    }

    /// One line for the tool description and `agent doctor`. Written for a
    /// reader deciding whether to trust the tool with a command.
    pub fn describe(self) -> &'static str {
        match self {
            Self::None => "no sandbox: commands run with your full user privileges",
            Self::Seatbelt => {
                "macOS Seatbelt: writes confined to the workspace, network limited to the allowlist"
            }
            Self::LandlockConfined => {
                "Linux Landlock: writes confined to the workspace, network limited to the allowlist"
            }
            Self::NetnsProxied => {
                "Linux network namespace: writes confined to the workspace, network limited to the allowlist"
            }
        }
    }
}

/// One command to run.
///
/// The command is a shell line rather than an argv array: the model wants
/// pipes and redirection (`cargo test 2>&1 | tail -40`), and there is no
/// injection boundary to protect here - the model authored the whole string.
/// The sandbox, not the parsing, is what contains it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRequest {
    pub command: String,
    /// Working directory, relative to the workspace root. Confining the type
    /// keeps a command from starting outside the sandbox.
    pub cwd: WorkspacePath,
    pub timeout: Duration,
    /// Combined stdout+stderr bytes kept before truncating.
    pub max_output_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// `None` when the process was killed by a signal.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Output hit `max_output_bytes` and was cut.
    pub truncated: bool,
    pub duration: Duration,
}

impl CommandOutput {
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }
}

#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// The confinement actually in force. Callers surface this rather than
    /// their configuration, because the two can differ.
    fn sandbox(&self) -> SandboxKind;

    async fn run(&self, request: CommandRequest) -> Result<CommandOutput, CommandError>;
}

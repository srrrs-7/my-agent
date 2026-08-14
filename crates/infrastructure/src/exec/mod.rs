//! Sandboxed command execution.
//!
//! The [`agent_domain::ports::command::CommandRunner`] implementation, plus
//! the platform detection that decides how much confinement is actually
//! available here. Everything platform-specific lives in this module; the
//! tool and the loop above it never learn which mechanism was used, only
//! [`SandboxKind`].
//!
//! Design goal: **no external runtime dependency**. Landlock is a syscall and
//! Seatbelt ships with macOS, so a distributed binary works without asking the
//! user to install anything. That is the difference between a sandbox that is
//! present and one that is merely documented.

pub(crate) mod capture;
pub(crate) mod env;
#[cfg(target_os = "linux")]
pub(crate) mod linux;
pub(crate) mod proxy;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_domain::error::CommandError;
use agent_domain::model::workspace::WorkspaceRoot;
use agent_domain::ports::command::{CommandOutput, CommandRequest, CommandRunner, SandboxKind};
use async_trait::async_trait;
use tokio::process::Command;
use tracing::warn;

use crate::net::guard::HostPolicy;
use proxy::EgressProxy;

/// How much confinement the operator insists on.
///
/// Stated as a *property* rather than as a mechanism, because the mechanism
/// differs by platform and an operator asking for confinement does not care
/// which one delivers it. Naming Landlock here would refuse to start on macOS
/// despite Seatbelt being just as good.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxRequirement {
    /// Whatever real confinement this platform has. Startup fails where there
    /// is none. The default, and the right answer nearly everywhere.
    #[default]
    Confined,
    /// Only the tier where nothing but the proxy is reachable at all. Not
    /// implemented on any platform yet, so this currently always fails - which
    /// is the honest outcome for an operator who insists on it.
    Isolated,
    /// Explicit opt-out (`AGENT_SHELL_SANDBOX=none`). Its own variant, so
    /// turning the sandbox off is always a deliberate act and never the result
    /// of a comparison that happened to come out false.
    Disabled,
}

impl SandboxRequirement {
    fn is_satisfied_by(self, kind: SandboxKind) -> bool {
        match self {
            Self::Confined => kind.confines(),
            Self::Isolated => kind.isolates(),
            Self::Disabled => true,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Self::Confined => "any real confinement",
            Self::Isolated => "full network isolation",
            Self::Disabled => "no sandbox",
        }
    }
}

impl std::fmt::Display for SandboxRequirement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.describe())
    }
}

impl std::str::FromStr for SandboxRequirement {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "confined" | "auto" | "on" | "true" => Ok(Self::Confined),
            "isolated" => Ok(Self::Isolated),
            "none" | "off" | "false" => Ok(Self::Disabled),
            other => Err(format!(
                "`{other}` is not a sandbox requirement (confined, isolated, none)"
            )),
        }
    }
}

/// How the operator configured command execution.
#[derive(Debug, Clone)]
pub struct ExecConfig {
    pub sandbox: SandboxRequirement,
    /// Extra writable roots outside the workspace - build caches such as a
    /// `CARGO_TARGET_DIR` that points elsewhere.
    pub extra_writable: Vec<PathBuf>,
    /// Domain suffixes the child may reach. Empty means no network at all: no
    /// proxy is started, so the kernel refuses every outbound connection.
    ///
    /// There is no "everything" setting here on purpose. `web_fetch` opens
    /// onto the public internet because the model picks one URL at a time and
    /// the result comes back as text; a shell command that can reach anything
    /// can also post the workspace to it.
    pub allowed_domains: Vec<String>,
}

impl Default for ExecConfig {
    fn default() -> Self {
        Self {
            sandbox: SandboxRequirement::Confined,
            extra_writable: Vec::new(),
            allowed_domains: Vec::new(),
        }
    }
}

/// What this machine can enforce, and why not more.
#[derive(Debug, Clone)]
pub struct SandboxAvailability {
    pub kind: SandboxKind,
    /// Populated when the platform offers less than was asked for.
    pub shortfall: Option<String>,
}

/// Probes the platform. Never returns an error: an environment with no
/// sandbox is a fact to report, and the caller decides whether it is
/// acceptable.
pub fn detect_sandbox() -> SandboxAvailability {
    #[cfg(target_os = "linux")]
    {
        match linux::detect() {
            Ok(kind) => SandboxAvailability {
                kind,
                shortfall: None,
            },
            Err(reason) => SandboxAvailability {
                kind: SandboxKind::None,
                shortfall: Some(reason),
            },
        }
    }
    #[cfg(target_os = "macos")]
    {
        SandboxAvailability {
            kind: SandboxKind::None,
            shortfall: Some(
                "macOS Seatbelt support is not implemented yet, so commands would run \
                 unconfined"
                    .to_string(),
            ),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        SandboxAvailability {
            kind: SandboxKind::None,
            shortfall: Some(
                "this platform has no supported sandbox; on Windows, run the agent inside WSL2"
                    .to_string(),
            ),
        }
    }
}

/// Runs commands under whatever confinement [`detect_sandbox`] found.
pub struct SandboxedCommandRunner {
    workspace: std::sync::Arc<WorkspaceRoot>,
    config: ExecConfig,
    kind: SandboxKind,
    /// Scratch space handed to children instead of the shared `/tmp`.
    session_temp: PathBuf,
    /// The one destination the child may connect to. Absent when the allowlist
    /// is empty, which is also the only way the child gets no network at all.
    egress: Option<EgressProxy>,
}

/// Distinguishes concurrent runners in the same process.
static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

impl SandboxedCommandRunner {
    /// Fails when the platform cannot reach the required confinement, so a
    /// weaker environment can never silently downgrade the protection.
    ///
    /// Async because an allowlist with entries binds the egress proxy's port,
    /// and the proxy's lifetime has to be this runner's: a child holding a
    /// `HTTP_PROXY` that points at a closed port is worse than one that knows
    /// it has no network.
    pub async fn start(
        workspace: std::sync::Arc<WorkspaceRoot>,
        config: ExecConfig,
    ) -> Result<Self, CommandError> {
        let available = detect_sandbox();

        let kind = match config.sandbox {
            SandboxRequirement::Disabled => {
                warn!("running commands without a sandbox at the operator's request");
                SandboxKind::None
            }
            requirement => {
                if !requirement.is_satisfied_by(available.kind) {
                    return Err(CommandError::SandboxUnavailable {
                        reason: available.shortfall.unwrap_or_else(|| {
                            format!(
                                "this platform offers `{}`, which does not provide {}",
                                available.kind.as_str(),
                                requirement.describe()
                            )
                        }),
                    });
                }
                available.kind
            }
        };

        // Toolchains need scratch space, but handing them the shared `/tmp`
        // would let a command reach every other tenant of it. A directory per
        // runner keeps the grant as narrow as the workspace itself.
        let session_temp = std::env::temp_dir().join(format!(
            "my-agent-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&session_temp).map_err(|error| CommandError::Io {
            message: format!(
                "cannot create the sandbox scratch directory `{}`: {error}",
                session_temp.display()
            ),
        })?;

        // No allowlist, no proxy: there would be nothing for it to permit, and
        // not binding a port keeps the "no network" case exactly that.
        let egress = if config.allowed_domains.is_empty() {
            None
        } else {
            let policy = HostPolicy::allowing(config.allowed_domains.clone());
            Some(EgressProxy::start(policy).await.map_err(|error| {
                CommandError::SandboxUnavailable {
                    reason: format!("cannot start the egress proxy: {error}"),
                }
            })?)
        };

        Ok(Self {
            workspace,
            config,
            kind,
            session_temp,
            egress,
        })
    }

    /// Loopback port of the egress proxy, for `doctor` and for tests that need
    /// to prove the kernel rule matches it.
    pub fn egress_port(&self) -> Option<u16> {
        self.egress.as_ref().map(EgressProxy::port)
    }

    /// Roots the child may write to: the workspace, its own scratch directory,
    /// and whatever the operator added.
    fn writable_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![
            self.workspace.as_path().to_path_buf(),
            self.session_temp.clone(),
        ];
        roots.extend(self.config.extra_writable.iter().cloned());
        roots
    }
}

impl Drop for SandboxedCommandRunner {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.session_temp);
    }
}

#[async_trait]
impl CommandRunner for SandboxedCommandRunner {
    fn sandbox(&self) -> SandboxKind {
        self.kind
    }

    async fn run(&self, request: CommandRequest) -> Result<CommandOutput, CommandError> {
        if request.command.trim().is_empty() {
            return Err(CommandError::Refused {
                reason: "the command is empty".to_string(),
            });
        }

        let cwd = self.workspace.absolute(&request.cwd);
        if !cwd.is_dir() {
            return Err(CommandError::Refused {
                reason: format!("`{}` is not a directory in the workspace", request.cwd),
            });
        }

        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(&request.command).current_dir(&cwd);

        env::scrub(&mut command);

        // Point every temp convention at the scratch directory the sandbox
        // actually allows, or the first thing a toolchain does is fail.
        for name in ["TMPDIR", "TMP", "TEMP"] {
            command.env(name, &self.session_temp);
        }

        // Proxy-aware tools pick these up and route through the allowlist.
        // An inherited `NO_PROXY` would carve holes in it, so it goes either
        // way - with no proxy there is nothing for it to except.
        for name in ["NO_PROXY", "no_proxy"] {
            command.env_remove(name);
        }
        match self.egress_port() {
            Some(port) => {
                let url = format!("http://127.0.0.1:{port}");
                for name in [
                    "HTTP_PROXY",
                    "HTTPS_PROXY",
                    "ALL_PROXY",
                    "http_proxy",
                    "https_proxy",
                    "all_proxy",
                ] {
                    command.env(name, &url);
                }
            }
            None => {
                for name in [
                    "HTTP_PROXY",
                    "HTTPS_PROXY",
                    "ALL_PROXY",
                    "http_proxy",
                    "https_proxy",
                    "all_proxy",
                ] {
                    command.env_remove(name);
                }
            }
        }

        #[cfg(target_os = "linux")]
        if self.kind == SandboxKind::LandlockConfined {
            linux::confine(
                &mut command,
                &linux::LinuxSandboxPolicy {
                    writable: self.writable_roots(),
                    proxy_port: self.egress_port(),
                },
            )?;
        }

        capture::run_capped(command, request.timeout, request.max_output_bytes).await
    }
}

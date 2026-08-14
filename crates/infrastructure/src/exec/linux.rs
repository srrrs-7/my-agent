//! Linux confinement via Landlock.
//!
//! Landlock is the only mechanism that fits a distributable CLI: it is a
//! syscall, so there is nothing to `apt install`, and it is designed to be
//! applied by an *unprivileged* process to itself - which is why it keeps
//! working inside a `cap_drop: ALL` container where bubblewrap's user
//! namespaces are refused outright.
//!
//! ## How the restriction reaches the child
//!
//! The ruleset is built in the parent and only `landlock_restrict_self` runs
//! in the child, between `fork` and `exec`. That ordering is deliberate:
//! everything after `fork` in a multi-threaded process (we run on tokio) must
//! stick to async-signal-safe work, and a builder that allocates does not
//! qualify. Building first leaves two bare syscalls for the child, and the
//! restriction is inherited across `execve`.
//!
//! ## What it does and does not cover
//!
//! Writes are confined; reads are not. Toolchains read from all over the
//! filesystem, and a read allowlist that keeps `cargo`, `npm` and `go`
//! working ends up close to "everything" anyway. Credentials are defended by
//! scrubbing the environment (see [`super::env`]) and by the egress
//! allowlist instead.
//!
//! Landlock's network rules are scoped to *ports*, never addresses - the
//! kernel's `landlock_net_port_attr` has no host field. So "only the proxy is
//! reachable" is expressed as "only the proxy's port is connectable", which a
//! determined child can still abuse by reaching a different host on that same
//! port. Closing that needs a network namespace; it is tracked as
//! `SandboxKind::NetnsProxied`.

use std::path::{Path, PathBuf};

use agent_domain::error::CommandError;
use agent_domain::ports::command::SandboxKind;
use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, NetPort, PathBeneath, PathFd,
    RulesetAttr, RulesetCreatedAttr, RulesetStatus,
};
use tokio::process::Command;
use tracing::debug;

/// ABI 4 is the first with TCP rules, which is what lets us pin egress to the
/// proxy. Anything older confines the filesystem but leaves the network open,
/// so the detection below reports it as unusable rather than pretending.
const REQUIRED_ABI: ABI = ABI::V4;

/// Pseudo-devices a shell command cannot work without.
///
/// `>/dev/null` appears in almost every command line, and a read-only `/dev`
/// makes it fail before the real work starts. Granting all of `/dev` would
/// hand over raw disks, so the grant is limited to these nodes, and to writing
/// them rather than to creating anything alongside them.
///
/// `/dev/tty` is deliberately absent. It is the one device here that leads
/// back to the human rather than to a bit bucket, and the child is given no
/// controlling terminal precisely so that opening it fails - see
/// [`super::capture`].
const WRITABLE_DEVICES: [&str; 5] = [
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
];

/// What the child may touch.
///
/// There is no read-only carve-out inside a writable root, and there cannot
/// be: Landlock rules are a pure allow-list that the kernel *unions*, with no
/// deny and no most-specific-wins precedence. Granting write on the workspace
/// therefore grants it on `.git` too. Protecting a subtree needs a mechanism
/// that can express denial - a bind mount (netns tier) or Seatbelt's `deny`
/// (macOS) - so it is a per-tier capability rather than something this tier
/// can promise. Verified empirically, not assumed.
#[derive(Debug, Clone)]
pub(crate) struct LinuxSandboxPolicy {
    /// Writable roots. The workspace is always here; operators add build
    /// caches that live outside it (`CARGO_TARGET_DIR`, for instance).
    pub writable: Vec<PathBuf>,
    /// TCP port the child may connect to - the egress proxy. `None` blocks
    /// every outbound connection.
    pub proxy_port: Option<u16>,
}

/// Whether this kernel can enforce what we need.
///
/// The only honest probe is to ask the kernel to build the ruleset we actually
/// intend to use: `HardRequirement` turns "this ABI is not available" into an
/// error instead of quietly handing back a weaker ruleset that enforces less
/// than we would then claim.
pub(crate) fn detect() -> Result<SandboxKind, String> {
    empty_ruleset()
        .map(|_| SandboxKind::LandlockConfined)
        .map_err(|error| {
            format!(
                "Landlock ABI v{} (Linux 6.7+) is required to confine the network, but this \
                 kernel refused it: {error}",
                REQUIRED_ABI as i32
            )
        })
}

/// A ruleset handling everything we care about, with no rules added yet.
fn empty_ruleset() -> Result<landlock::RulesetCreated, landlock::RulesetError> {
    landlock::Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(REQUIRED_ABI))?
        .handle_access(AccessNet::from_all(REQUIRED_ABI))?
        .create()
}

/// Attaches the sandbox to `command`, to take effect between fork and exec.
pub(crate) fn confine(
    command: &mut Command,
    policy: &LinuxSandboxPolicy,
) -> Result<(), CommandError> {
    let ruleset = build_ruleset(policy)?;

    // Moved into the closure and taken on first use: `restrict_self` consumes
    // the ruleset, but `pre_exec` hands us `FnMut`.
    let mut ruleset = Some(ruleset);
    unsafe {
        command.pre_exec(move || {
            let Some(ruleset) = ruleset.take() else {
                return Err(std::io::Error::other("the sandbox was already consumed"));
            };
            let status = ruleset
                .restrict_self()
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            if status.ruleset == RulesetStatus::NotEnforced {
                // Fail the exec rather than run the command unconfined.
                return Err(std::io::Error::other(
                    "the kernel did not enforce the Landlock ruleset",
                ));
            }
            Ok(())
        });
    }
    Ok(())
}

/// Builds the ruleset in the parent, where allocation is safe.
fn build_ruleset(policy: &LinuxSandboxPolicy) -> Result<landlock::RulesetCreated, CommandError> {
    let sandbox_error = |error: landlock::RulesetError| CommandError::SandboxUnavailable {
        reason: error.to_string(),
    };

    let mut ruleset = empty_ruleset().map_err(sandbox_error)?;

    // Read everything (see the module docs on why), write only where allowed.
    ruleset = add_path(ruleset, Path::new("/"), AccessFs::from_read(REQUIRED_ABI))?;

    for path in &policy.writable {
        ruleset = add_path(ruleset, path, AccessFs::from_all(REQUIRED_ABI))?;
    }

    // File-level rights only: enough to redirect into them, not enough to
    // create new device nodes.
    let device_access = AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::Truncate;
    for device in WRITABLE_DEVICES {
        ruleset = add_path(ruleset, Path::new(device), device_access)?;
    }

    if let Some(port) = policy.proxy_port {
        ruleset = ruleset
            .add_rule(NetPort::new(port, AccessNet::ConnectTcp))
            .map_err(sandbox_error)?;
        debug!(port, "egress limited to the proxy port");
    }

    Ok(ruleset)
}

/// Adds one path rule, skipping paths that do not exist rather than failing:
/// a missing `/var/tmp` or an absent build cache is not a reason to refuse to
/// run anything.
fn add_path(
    ruleset: landlock::RulesetCreated,
    path: &Path,
    access: landlock::BitFlags<AccessFs>,
) -> Result<landlock::RulesetCreated, CommandError> {
    let Ok(fd) = PathFd::new(path) else {
        debug!(path = %path.display(), "skipping a sandbox path that does not exist");
        return Ok(ruleset);
    };
    ruleset
        .add_rule(PathBeneath::new(fd, access))
        .map_err(|error| CommandError::SandboxUnavailable {
            reason: format!("cannot add `{}` to the sandbox: {error}", path.display()),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_kernel_supports_what_we_require() {
        // The dev container is the reference environment; if this fails there,
        // the whole Linux path is unusable and we want to know loudly.
        match detect() {
            Ok(kind) => assert_eq!(kind, SandboxKind::LandlockConfined),
            Err(reason) => panic!("Landlock unavailable in the dev container: {reason}"),
        }
    }

    #[test]
    fn a_ruleset_builds_for_a_realistic_policy() {
        let policy = LinuxSandboxPolicy {
            writable: vec![PathBuf::from("/tmp")],
            proxy_port: Some(18080),
        };
        build_ruleset(&policy).expect("the ruleset must build");
    }

    #[test]
    fn missing_paths_are_skipped_rather_than_fatal() {
        let policy = LinuxSandboxPolicy {
            writable: vec![PathBuf::from("/definitely/not/here")],
            proxy_port: None,
        };
        build_ruleset(&policy).expect("a missing path must not break the sandbox");
    }
}

//! Spawning a child into its own session, with a guard that takes the whole
//! group down.
//!
//! Shared by every way this crate runs an untrusted process - a one-shot
//! command ([`super::capture`]) as much as a long-lived server - because the
//! guard is only meaningful for a child that leads its own group, so the two
//! are set up in the same place.
//!
//! The child is detached into its own *session*, which does two jobs at once
//! and closes a hole that a process group alone leaves open: a child sharing
//! our session keeps the controlling terminal, and `/dev/tty` reaches it
//! directly - past the pipes we capture. Writing there would let a command
//! print whatever it liked to the user's terminal, including an imitation of
//! the approval prompt; reading there would hand it the user's keystrokes.
//! Without a controlling terminal, opening `/dev/tty` simply fails.
//!
//! Dropping the handle kills the group, not only the direct child:
//! `kill_on_drop` alone would leave a shell's grandchildren running. The guard
//! fires first, while the child is still unreaped, so the group id cannot yet
//! belong to anyone else. It then sweeps for descendants that left the group
//! ([`super::reaper`]), which no group signal reaches.

use std::collections::HashSet;

use agent_domain::error::CommandError;
use tokio::process::{Child, Command};

use super::reaper;

/// A spawned child and the guard that kills its process group.
///
/// Field order is load-bearing: fields drop in declaration order, so `group`
/// goes before `child`. The group is therefore killed while the child is at
/// worst a zombie, which keeps its pid - and with it the group id - from being
/// handed to an unrelated process.
pub(crate) struct GuardedChild {
    pub(crate) group: GroupGuard,
    pub(crate) child: Child,
}

impl GuardedChild {
    /// Spawns `command` as the leader of a new session.
    ///
    /// Stdio is the caller's choice and must be set before this. Any
    /// `pre_exec` hooks already on `command` (the sandbox) run before the one
    /// added here.
    pub(crate) fn spawn(mut command: Command) -> Result<Self, CommandError> {
        command.kill_on_drop(true);

        // Its own session: a new process group (so the whole tree can be
        // taken down) *and* no controlling terminal. `setsid` rather than
        // `process_group(0)` because it delivers both, and because calling it
        // after the child is already a group leader would fail with EPERM.
        #[cfg(unix)]
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = reaper::register_spawn(|| command.spawn(), Child::id).map_err(|error| {
            CommandError::SpawnFailed {
                message: error.to_string(),
            }
        })?;
        Ok(Self {
            group: GroupGuard { pid: child.id() },
            child,
        })
    }
}

/// Kills the child's process group when dropped, unless disarmed first.
pub(crate) struct GroupGuard {
    pid: Option<u32>,
}

impl GroupGuard {
    /// Call once the child has been reaped: from then on its pid may be
    /// reused, and signalling its group could hit a stranger.
    pub(crate) fn disarm(&mut self) {
        if let Some(pid) = self.pid.take() {
            reaper::unregister(pid);
        }
    }

    /// Kills the group and every descendant that left it, and leaves the
    /// guard armed: the pid stays registered as tokio's, so the caller can
    /// reap the child and only then [`Self::disarm`]. Returns what
    /// [`reaper::settle`] should wait for.
    ///
    /// The child must still be unreaped.
    pub(crate) fn kill(&self) -> HashSet<i32> {
        let Some(pid) = self.pid else {
            return HashSet::new();
        };
        kill_group(Some(pid));
        reaper::sweep(&[pid])
    }

    /// Kills the group and every descendant that left it, and gives the pid
    /// back to tokio. Returns what [`reaper::settle`] should wait for.
    ///
    /// The child must still be unreaped; after this the guard is spent.
    pub(crate) fn stop(&mut self) -> HashSet<i32> {
        let Some(pid) = self.pid.take() else {
            return HashSet::new();
        };
        kill_group(Some(pid));
        // While the leader is unreaped, so its pid still names our child.
        let found = reaper::sweep(&[pid]);
        reaper::unregister(pid);
        found
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        let found = self.stop();
        if !found.is_empty() {
            reaper::settle_in_background(found);
        }
    }
}

#[cfg(unix)]
pub(crate) fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // Negative pid signals the whole process group.
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
}

#[cfg(not(unix))]
pub(crate) fn kill_group(_pid: Option<u32>) {}

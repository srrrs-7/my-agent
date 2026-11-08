//! Stopping the descendants that left their process group.
//!
//! Every untrusted child leads its own session, and stopping it means
//! signalling that group ([`super::process`]). A descendant that calls
//! `setsid` or `setpgid` is beyond that signal:
//! `setsid sh -c 'sleep 20; echo x >> Cargo.toml' &` would outlive the
//! command and act after the dependency check had already compared the
//! workspace. This module finds such processes and kills them.
//!
//! ## Why a subreaper, and why not cgroups
//!
//! A cgroup per command would attribute and kill in one write to
//! `cgroup.kill`, but the cgroup filesystem is mounted read-only in the dev
//! container (and in Docker's default setup generally), so it would be a
//! mechanism the reference environment cannot exercise - and a containment
//! claim that is never tested is not one this crate makes.
//!
//! Instead the agent marks itself `PR_SET_CHILD_SUBREAPER`. A process whose
//! parent dies is then handed to the agent rather than to init, so nothing a
//! child starts can leave the agent's subtree of `/proc`, whatever groups or
//! sessions it moves between. Both are unprivileged and need nothing beyond
//! the kernel.
//!
//! ## Who is a stray
//!
//! A direct child of the agent is one of three things:
//!
//! * a child tokio spawned for us and still owns - registered in [`OWNED`]
//!   while it runs, never touched here;
//! * a child spawned without [`super::process::GuardedChild`] (the test
//!   harness's own `curl`, say) - it shares the agent's session, which an
//!   untrusted child never does, because it was put into its own;
//! * an orphan of an untrusted child, adopted through the subreaper - a
//!   **stray**. It is in another session and nobody spawned it.
//!
//! Strays cannot be told apart by which command or server they came from:
//! the one link to their origin was the parent that died. So every stray is
//! killed whenever any untrusted child is stopped - which only errs towards
//! stopping something early, never towards letting it run on.
//!
//! ## Reaping without stealing
//!
//! Adopted orphans become the agent's zombies, and tokio only waits for the
//! pids it spawned. `waitpid(-1)` would reap them, but it would also take the
//! exit status of a command tokio is still waiting for. So reaping goes pid by
//! pid, and only for zombies that are strays by the rule above.

use std::time::Duration;

/// How long [`settle`] waits for signalled strays to disappear. SIGKILL is
/// delivered at once; this only covers the scheduler getting round to them.
pub(crate) const SETTLE_BUDGET: Duration = Duration::from_secs(5);

#[cfg(target_os = "linux")]
pub(crate) use linux::{enable, register_spawn, settle, settle_in_background, sweep, unregister};

#[cfg(not(target_os = "linux"))]
pub(crate) use fallback::{
    enable, register_spawn, settle, settle_in_background, sweep, unregister,
};

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::{BTreeSet, HashMap, HashSet};
    use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
    use std::time::{Duration, Instant};

    use tracing::warn;

    /// Pids tokio spawned and still owns. Held across spawning, so a sweep
    /// never sees a fresh child between its `fork` and its registration -
    /// which, being in another session and unregistered, would look exactly
    /// like a stray.
    static OWNED: Mutex<BTreeSet<i32>> = Mutex::new(BTreeSet::new());

    /// Whether this process is a subreaper with a readable `/proc`. Decided
    /// once: the attribute is process-wide and is never turned off.
    static ENABLED: OnceLock<Result<(), String>> = OnceLock::new();

    /// Rounds of "scan, signal what is new" before giving up on a tree that
    /// keeps forking faster than it is killed. A killed process cannot fork
    /// again, so a real tree settles in a few.
    const MAX_ROUNDS: usize = 64;

    fn owned() -> MutexGuard<'static, BTreeSet<i32>> {
        OWNED.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes this process a subreaper. `Err` says why descendants that leave
    /// their group cannot be stopped here; the caller reports it.
    pub(crate) fn enable() -> Result<(), String> {
        ENABLED
            .get_or_init(|| {
                // SAFETY: a plain prctl with integer arguments; it touches
                // only this process's own attributes.
                let set = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
                if set != 0 {
                    return Err(format!(
                        "this kernel refused PR_SET_CHILD_SUBREAPER: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                if read_stat(std::process::id() as i32).is_none() {
                    return Err("/proc is not readable here".to_string());
                }
                Ok(())
            })
            .clone()
    }

    /// Spawns through `spawn` and registers the pid, both under the lock.
    pub(crate) fn register_spawn<T>(
        spawn: impl FnOnce() -> std::io::Result<T>,
        pid: impl Fn(&T) -> Option<u32>,
    ) -> std::io::Result<T> {
        // Before the first child exists: an orphan that was handed to init
        // instead is out of reach for good.
        let _ = enable();
        let mut owned = owned();
        let child = spawn()?;
        if let Some(pid) = pid(&child) {
            owned.insert(pid as i32);
        }
        Ok(child)
    }

    /// Call once tokio no longer needs the pid: reaped, or handed over to its
    /// orphan queue on drop (which tolerates finding it already reaped).
    pub(crate) fn unregister(pid: u32) {
        owned().remove(&(pid as i32));
    }

    /// Signals every live stray and every live descendant of `roots`, until a
    /// scan finds nothing new. Returns every process found - signalled, or
    /// already dead and waiting to be reaped - for [`settle`] to wait on.
    ///
    /// Synchronous and non-blocking, so it can run from `Drop`. Once it
    /// returns, every process found has SIGKILL pending and cannot run
    /// another instruction of its own.
    ///
    /// `roots` must be unreaped, like the group leader in
    /// [`super::super::process::GroupGuard`]: a reaped pid may already belong
    /// to someone else.
    pub(crate) fn sweep(roots: &[u32]) -> HashSet<i32> {
        let mut found = HashSet::new();
        sweep_into(roots, &mut found);
        found
    }

    fn sweep_into(roots: &[u32], found: &mut HashSet<i32>) {
        if enable().is_err() {
            return;
        }
        for _ in 0..MAX_ROUNDS {
            let owned = owned();
            let table = snapshot();
            let mut fresh = false;
            for pid in targets(&table, roots, &owned) {
                // A zombie is tracked too: it still has to be reaped, which
                // happens here once its parent dies and hands it to us.
                let zombie = table.get(&pid).is_some_and(|entry| entry.zombie);
                if found.insert(pid) && !zombie {
                    // SAFETY: plain syscall. The pid was seen a moment ago in
                    // our subtree, where it cannot be recycled - see `targets`.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    fresh = true;
                }
            }
            // A process signalled in an earlier round can no longer fork, so
            // a round with nothing new means the tree is complete.
            if !fresh {
                return;
            }
        }
        warn!("stray processes kept appearing; stopped sweeping after {MAX_ROUNDS} rounds");
    }

    /// Sweeps and reaps until everything in `found` (from [`sweep`]) and
    /// everything found along the way has been reaped, or `budget` runs out.
    /// Blocks: call it off the async runtime.
    pub(crate) fn settle(mut found: HashSet<i32>, budget: Duration) -> bool {
        if enable().is_err() {
            return true;
        }
        let deadline = Instant::now() + budget;
        loop {
            sweep_into(&[], &mut found);
            reap();
            let table = snapshot();
            found.retain(|&pid| in_our_subtree(&table, pid));
            if found.is_empty() {
                return true;
            }
            if Instant::now() >= deadline {
                warn!(
                    pending = found.len(),
                    "stray processes were signalled but not reaped within {budget:?}"
                );
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// [`settle`] on a thread of its own, for `Drop`, which cannot wait.
    pub(crate) fn settle_in_background(found: HashSet<i32>) {
        if enable().is_err() {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("reap-strays".to_string())
            .spawn(move || settle(found, super::SETTLE_BUDGET));
        if let Err(error) = spawned {
            // Everything was signalled already; only zombies are left, and
            // the next settle reaps them.
            warn!("cannot start the stray reaper: {error}");
        }
    }

    /// Whether `pid` is still a process below us. Once reaped it either
    /// vanishes or, recycled, turns up outside our subtree - both mean done.
    fn in_our_subtree(table: &HashMap<i32, Entry>, pid: i32) -> bool {
        let me = std::process::id() as i32;
        let mut current = pid;
        // Bounded: a cycle cannot occur in a consistent table, but a snapshot
        // read process by process is not quite one.
        for _ in 0..table.len() + 1 {
            match table.get(&current) {
                Some(entry) if entry.ppid == me => return true,
                Some(entry) if entry.ppid > 1 => current = entry.ppid,
                _ => return false,
            }
        }
        false
    }

    /// Reaps zombie strays, pid by pid - never `waitpid(-1)`, which would
    /// take the status of a child tokio is waiting for.
    fn reap() {
        let owned = owned();
        let me = std::process::id() as i32;
        let my_session = own_session();
        for (pid, entry) in snapshot() {
            if entry.zombie && entry.ppid == me && entry.session != my_session {
                if owned.contains(&pid) {
                    continue;
                }
                let mut status = 0;
                // SAFETY: plain syscall on a pid that is our zombie child and
                // not tokio's; WNOHANG keeps it from blocking.
                unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            }
        }
    }

    /// Every process to stop: the strays and their subtrees, and the
    /// subtrees (not the roots themselves) of `roots`.
    ///
    /// A process in our subtree cannot have its pid recycled under us: a dead
    /// one stays a zombie until its parent - which is also in our subtree, or
    /// is us - reaps it, and only [`reap`] does that for strays.
    fn targets(table: &HashMap<i32, Entry>, roots: &[u32], owned: &BTreeSet<i32>) -> Vec<i32> {
        let me = std::process::id() as i32;
        let my_session = own_session();
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        for (&pid, entry) in table {
            children.entry(entry.ppid).or_default().push(pid);
        }

        let mut pending: Vec<i32> = Vec::new();
        for &root in roots {
            pending.extend(children.get(&(root as i32)).into_iter().flatten());
        }
        pending.extend(children.get(&me).into_iter().flatten().filter(|&&pid| {
            !owned.contains(&pid) && table.get(&pid).is_some_and(|e| e.session != my_session)
        }));

        let mut found = Vec::new();
        let mut seen = HashSet::new();
        while let Some(pid) = pending.pop() {
            if pid == me || owned.contains(&pid) || !seen.insert(pid) {
                continue;
            }
            found.push(pid);
            pending.extend(children.get(&pid).into_iter().flatten());
        }
        found
    }

    fn own_session() -> i32 {
        // SAFETY: getsid(0) asks about the calling process and cannot fail.
        unsafe { libc::getsid(0) }
    }

    #[derive(Debug, Clone, Copy)]
    struct Entry {
        ppid: i32,
        session: i32,
        zombie: bool,
    }

    /// Every process visible in `/proc`. Threads are not listed, and a
    /// process's parent is reported as the parent's thread-group id, so the
    /// tree is one of processes.
    fn snapshot() -> HashMap<i32, Entry> {
        let mut table = HashMap::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return table;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<i32>().ok())
            else {
                continue;
            };
            // Gone between listing and reading: simply not there any more.
            if let Some(stat) = read_stat(pid) {
                table.insert(pid, stat);
            }
        }
        table
    }

    fn read_stat(pid: i32) -> Option<Entry> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        parse_stat(&stat)
    }

    /// `pid (comm) state ppid pgrp session ...`. The command name may itself
    /// contain spaces and parentheses, so the fields are read after the
    /// *last* `)`.
    fn parse_stat(stat: &str) -> Option<Entry> {
        let (_, rest) = stat.rsplit_once(')')?;
        let mut fields = rest.split_whitespace();
        let state = fields.next()?;
        let ppid = fields.next()?.parse().ok()?;
        let _group: i32 = fields.next()?.parse().ok()?;
        let session = fields.next()?.parse().ok()?;
        Some(Entry {
            ppid,
            session,
            zombie: state == "Z",
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_command_name_with_parentheses_does_not_shift_the_fields() {
            let entry = parse_stat("4242 (a) (b) c) Z 17 4242 4242 0 -1 4194560").unwrap();
            assert_eq!(entry.ppid, 17);
            assert_eq!(entry.session, 4242);
            assert!(entry.zombie);
        }

        #[test]
        fn this_process_can_become_a_subreaper_and_read_proc() {
            // The dev container is the reference environment; if this fails
            // there, detached descendants are not being stopped.
            assert_eq!(enable(), Ok(()));
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod fallback {
    use std::collections::HashSet;
    use std::time::Duration;

    pub(crate) fn enable() -> Result<(), String> {
        Err("only implemented on Linux".to_string())
    }

    pub(crate) fn register_spawn<T>(
        spawn: impl FnOnce() -> std::io::Result<T>,
        _pid: impl Fn(&T) -> Option<u32>,
    ) -> std::io::Result<T> {
        spawn()
    }

    pub(crate) fn unregister(_pid: u32) {}

    pub(crate) fn sweep(_roots: &[u32]) -> HashSet<i32> {
        HashSet::new()
    }

    pub(crate) fn settle(_found: HashSet<i32>, _budget: Duration) -> bool {
        true
    }

    pub(crate) fn settle_in_background(_found: HashSet<i32>) {}
}

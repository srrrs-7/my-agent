//! Background jobs: commands that keep running after the call that started
//! them.
//!
//! The [`BackgroundCommands`] implementation. Each job is confined exactly
//! like a one-shot command - the same `/bin/sh -c` line built by
//! [`SandboxedCommandRunner::shell_command`], the same scratch directory and
//! egress proxy, spawned through [`GuardedChild`] so it leads its own session
//! and is registered with the reaper as ours (never mistaken for a stray).
//!
//! What a one-shot run guarantees by returning, a job has to be given by
//! limits, and every one of them is enforced here:
//!
//! * **Lifetime.** Every job has a deadline; reaching it kills the job and
//!   the read that observes the end says so.
//! * **Output.** Each job keeps a ring buffer of its newest output. Older
//!   bytes are discarded and a read that asked for them is told how many.
//! * **Count.** A start beyond the concurrency cap is refused, and finished
//!   jobs are forgotten oldest first once more than a few are kept.
//! * **Owner.** Dropping the supervisor kills every job at once, from
//!   `Drop`, so process exit takes them down too.
//!
//! ## Who kills, who reaps
//!
//! A group may only be signalled while its leader is unreaped (see
//! [`super::capture`]). Each job's [`GroupGuard`] lives in the job's shared
//! state; the task that owns the child takes it out under the lock, kills the
//! group with it, reaps, and only then disarms it. Whoever else wants the job
//! dead - `kill`, `/reset`, the supervisor's `Drop` - either asks that task to
//! do it or, from `Drop`, takes the guard first and drops it. The guard is
//! therefore used by exactly one party, before the reap.
//!
//! ## The end of a job
//!
//! As for a one-shot command, the end is noticed through the pipes: both
//! reach EOF once every process holding them has exited. A leader that exits
//! while a child of its own keeps the pipes open is still running as far as
//! this module is concerned - until the child exits, the job is killed, or
//! the lifetime runs out. Then the rest of the group is killed, so nothing a
//! job started outlives the moment its end is reported.
//!
//! ## Dependency files
//!
//! The workspace's dependency files are snapshotted when the job starts and
//! again once it has ended and everything it started has been reaped. The
//! difference is handed out once, with the first read that observes the end.
//! It covers the whole window, so a change made by something else while the
//! job ran is attributed to it as well; the note says "while", not "by".

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use agent_domain::error::{CommandError, JobError};
use agent_domain::model::dependency::DependencyReport;
use agent_domain::model::workspace::WorkspaceRoot;
use agent_domain::ports::background::{BackgroundCommands, JobId, JobLimits, JobOutput, JobStop};
use agent_domain::ports::command::{CommandRequest, SandboxKind};
use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;
use tokio::sync::{Notify, watch};

use super::SandboxedCommandRunner;
use super::process::{GroupGuard, GuardedChild};
use super::reaper;
use crate::tools::dependency_watch::{DEFAULT_MAX_ENTRIES, Snapshot};

/// Defaults for [`JobLimits`], used by the settings.
pub const DEFAULT_MAX_JOBS: usize = 4;
pub const DEFAULT_MAX_LIFETIME: Duration = Duration::from_secs(30 * 60);
pub const DEFAULT_BUFFER_BYTES: usize = 256 * 1024;

/// How long [`BackgroundSupervisor::kill`] waits for a job to be gone. The
/// kill itself is SIGKILL; this covers the reap and the second scan.
const KILL_WAIT: Duration = Duration::from_secs(30);

/// How long the pipes are drained after a job is killed. Everything holding
/// them has been signalled, so EOF follows at once; the bound only covers a
/// platform where a detached descendant could not be found.
const DRAIN_AFTER_KILL: Duration = Duration::from_secs(2);

/// Finished jobs kept, per allowed running job, before the oldest is
/// forgotten. Enough for the model to read a job it started a few calls ago.
const RETAINED_PER_SLOT: usize = 4;

/// Runs and tracks background jobs under the command runner's confinement.
pub struct BackgroundSupervisor {
    /// Held for the confinement and, through it, for the egress proxy and the
    /// scratch directory the jobs are pointed at: they must outlive every job.
    runner: Arc<SandboxedCommandRunner>,
    limits: JobLimits,
    next_id: AtomicU64,
    jobs: Mutex<BTreeMap<JobId, Arc<Job>>>,
}

/// One job, shared between the supervisor and the task that owns its child.
struct Job {
    state: Mutex<JobState>,
    /// Tells the owning task to kill the job. `notify_one` stores a permit,
    /// so a request made before the task waits is not lost.
    stop: Notify,
    /// Becomes `true` once the end is fully processed: reaped, compared,
    /// recorded.
    ended: watch::Sender<bool>,
}

struct JobState {
    buffer: RingBuffer,
    /// Present until someone uses it to kill the group - see the module docs.
    guard: Option<GroupGuard>,
    running: bool,
    exit_code: Option<i32>,
    stopped: Option<JobStop>,
    /// Why someone outside the owning task wanted the job stopped.
    requested: Option<JobStop>,
    /// Waiting to be handed out with the first read that observes the end.
    report: Option<DependencyReport>,
}

impl Job {
    fn lock(&self) -> MutexGuard<'_, JobState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Asks the owning task to stop the job. Does nothing once it has ended.
    fn request_stop(&self) {
        let mut state = self.lock();
        if state.running && state.requested.is_none() {
            state.requested = Some(JobStop::Killed);
            self.stop.notify_one();
        }
    }
}

impl BackgroundSupervisor {
    /// Refuses a runner without real confinement: a job runs unattended
    /// between calls, so it is only offered where the operating system
    /// contains it.
    pub fn new(
        runner: Arc<SandboxedCommandRunner>,
        limits: JobLimits,
    ) -> Result<Self, CommandError> {
        use agent_domain::ports::command::CommandRunner as _;
        if !runner.sandbox().confines() {
            return Err(CommandError::SandboxUnavailable {
                reason: "background jobs run only under a sandbox, and commands here run \
                         unconfined"
                    .to_string(),
            });
        }
        Ok(Self {
            runner,
            limits: JobLimits {
                max_jobs: limits.max_jobs.max(1),
                max_lifetime: limits.max_lifetime.max(Duration::from_secs(1)),
                buffer_bytes: limits.buffer_bytes.max(1),
            },
            next_id: AtomicU64::new(1),
            jobs: Mutex::new(BTreeMap::new()),
        })
    }

    fn jobs(&self) -> MutexGuard<'_, BTreeMap<JobId, Arc<Job>>> {
        self.jobs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn job(&self, id: JobId) -> Result<Arc<Job>, JobError> {
        self.jobs()
            .get(&id)
            .cloned()
            .ok_or(JobError::Unknown { id: id.get() })
    }

    /// Stops every job and forgets them all - for a conversation that starts
    /// over, which cannot refer to the old jobs anyway. Returns at once; each
    /// job's own task does the killing.
    pub fn stop_all(&self) {
        let jobs = std::mem::take(&mut *self.jobs());
        for job in jobs.values() {
            job.request_stop();
        }
    }

    /// Jobs still running.
    pub fn running(&self) -> usize {
        self.jobs()
            .values()
            .filter(|job| job.lock().running)
            .count()
    }

    /// Forgets the oldest finished jobs beyond what is kept. Running jobs are
    /// never forgotten - they are bounded by the cap instead.
    fn evict(jobs: &mut BTreeMap<JobId, Arc<Job>>, keep: usize) {
        let finished: Vec<JobId> = jobs
            .iter()
            .filter(|(_, job)| !job.lock().running)
            .map(|(id, _)| *id)
            .collect();
        let excess = finished.len().saturating_sub(keep);
        for id in finished.into_iter().take(excess) {
            jobs.remove(&id);
        }
    }

    fn running_in(jobs: &BTreeMap<JobId, Arc<Job>>) -> usize {
        jobs.values().filter(|job| job.lock().running).count()
    }
}

impl Drop for BackgroundSupervisor {
    /// Kills every job now. `Drop` cannot wait for the owning tasks - at
    /// process exit they may never run again - so the guards are taken and
    /// dropped here, which signals each group and sweeps synchronously.
    fn drop(&mut self) {
        let jobs = std::mem::take(&mut *self.jobs());
        for job in jobs.values() {
            let guard = {
                let mut state = job.lock();
                if state.running {
                    state.requested.get_or_insert(JobStop::Killed);
                }
                state.guard.take()
            };
            drop(guard);
            job.stop.notify_one();
        }
    }
}

#[async_trait]
impl BackgroundCommands for BackgroundSupervisor {
    fn sandbox(&self) -> SandboxKind {
        use agent_domain::ports::command::CommandRunner as _;
        self.runner.sandbox()
    }

    fn limits(&self) -> JobLimits {
        self.limits
    }

    async fn start(&self, request: CommandRequest) -> Result<JobId, JobError> {
        // Checked before the scan too, so a refused start costs nothing.
        if Self::running_in(&self.jobs()) >= self.limits.max_jobs {
            return Err(JobError::TooMany {
                limit: self.limits.max_jobs,
            });
        }
        let mut command = self.runner.shell_command(&request)?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let root = self.runner.workspace().clone();
        let before = scan(root.clone()).await;

        let lifetime = request.timeout.min(self.limits.max_lifetime);
        let capacity = request.max_output_bytes.clamp(1, self.limits.buffer_bytes);

        // From here to the insert nothing awaits: a start abandoned by an
        // interrupt either spawned nothing or left a job the table knows.
        let mut jobs = self.jobs();
        if Self::running_in(&jobs) >= self.limits.max_jobs {
            return Err(JobError::TooMany {
                limit: self.limits.max_jobs,
            });
        }
        let GuardedChild { group, child } = GuardedChild::spawn(command)?;
        let id = JobId::new(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (ended, _) = watch::channel(false);
        let job = Arc::new(Job {
            state: Mutex::new(JobState {
                buffer: RingBuffer::new(capacity),
                guard: Some(group),
                running: true,
                exit_code: None,
                stopped: None,
                requested: None,
                report: None,
            }),
            stop: Notify::new(),
            ended,
        });
        jobs.insert(id, job.clone());
        Self::evict(&mut jobs, self.limits.max_jobs * RETAINED_PER_SLOT);
        drop(jobs);

        tokio::spawn(supervise(job, child, lifetime, before, root));
        Ok(id)
    }

    async fn read(&self, id: JobId, since: u64, max: usize) -> Result<JobOutput, JobError> {
        let job = self.job(id)?;
        let mut state = job.lock();
        let (chunk, next, dropped) = state.buffer.read(since, max, !state.running);
        Ok(output(&mut state, chunk, next, dropped))
    }

    async fn kill(&self, id: JobId, max: usize) -> Result<JobOutput, JobError> {
        let job = self.job(id)?;
        job.request_stop();
        let mut ended = job.ended.subscribe();
        // A job that cannot be reaped in time is reported as still running;
        // the read that later observes its end carries the report.
        let _ = tokio::time::timeout(KILL_WAIT, ended.wait_for(|done| *done)).await;

        let mut state = job.lock();
        let (chunk, next) = state.buffer.tail(max, !state.running);
        Ok(output(&mut state, chunk, next, 0))
    }
}

/// Assembles a report, handing out the dependency comparison if the job has
/// ended and nobody has seen it yet.
fn output(state: &mut JobState, chunk: String, next: u64, dropped: u64) -> JobOutput {
    JobOutput {
        chunk,
        next,
        dropped,
        running: state.running,
        exit_code: state.exit_code,
        stopped: state.stopped,
        dependencies: if state.running {
            None
        } else {
            state.report.take()
        },
    }
}

/// Owns the child for the life of the job: pumps its output, waits for the
/// end, a stop request or the deadline, then kills, reaps and compares.
async fn supervise(
    job: Arc<Job>,
    mut child: Child,
    lifetime: Duration,
    before: Snapshot,
    root: Arc<WorkspaceRoot>,
) {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let pumps = async {
        tokio::join!(pump(stdout, &job), pump(stderr, &job));
    };
    tokio::pin!(pumps);

    let stopped = tokio::select! {
        () = &mut pumps => None,
        () = job.stop.notified() => Some(JobStop::Killed),
        () = tokio::time::sleep(lifetime) => Some(JobStop::LifetimeExceeded {
            seconds: lifetime.as_secs(),
        }),
    };

    // Taken under the lock, so nobody else can use it after this point - see
    // "Who kills, who reaps".
    let guard = job.lock().guard.take();
    let found = guard.as_ref().map(GroupGuard::kill).unwrap_or_default();
    if stopped.is_some() {
        // What the job wrote before it died is still in the pipes.
        let _ = tokio::time::timeout(DRAIN_AFTER_KILL, &mut pumps).await;
    }
    let status = child.wait().await;
    if let Some(mut guard) = guard {
        guard.disarm();
    }
    settle(found).await;

    let after = scan(root).await;
    let report = before.report_to(&after, DEFAULT_MAX_ENTRIES);

    {
        let mut state = job.lock();
        state.running = false;
        state.exit_code = status.ok().and_then(|status| status.code());
        state.stopped = stopped.or(state.requested);
        state.report = Some(report);
    }
    job.ended.send_replace(true);
}

/// Copies one stream into the job's buffer until EOF.
async fn pump<R: AsyncRead + Unpin>(reader: Option<R>, job: &Job) {
    let Some(mut reader) = reader else {
        return;
    };
    let mut chunk = [0_u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => job.lock().buffer.push(&chunk[..read]),
        }
    }
}

/// Waits, off the runtime, for the swept processes to be reaped.
async fn settle(found: HashSet<i32>) {
    if found.is_empty() {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || reaper::settle(found, reaper::SETTLE_BUDGET)).await;
}

async fn scan(root: Arc<WorkspaceRoot>) -> Snapshot {
    tokio::task::spawn_blocking(move || Snapshot::take(&root, DEFAULT_MAX_ENTRIES))
        .await
        // A scan that panicked saw nothing; marked incomplete so the report
        // says the check did not happen.
        .unwrap_or_else(|_| Snapshot::incomplete())
}

/// The newest `capacity` bytes of a stream, addressed by their offset in the
/// whole stream.
struct RingBuffer {
    bytes: VecDeque<u8>,
    capacity: usize,
    /// Bytes ever pushed: the offset one past the newest byte.
    total: u64,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            capacity,
            total: 0,
        }
    }

    fn push(&mut self, data: &[u8]) {
        self.total += data.len() as u64;
        let data = &data[data.len().saturating_sub(self.capacity)..];
        let overflow = (self.bytes.len() + data.len()).saturating_sub(self.capacity);
        self.bytes.drain(..overflow);
        self.bytes.extend(data);
    }

    /// Offset of the oldest byte still held.
    fn oldest(&self) -> u64 {
        self.total - self.bytes.len() as u64
    }

    /// Up to `max` bytes from `since`, as text, with the offset to continue
    /// from and how many bytes after `since` were already discarded.
    ///
    /// Cut on character boundaries while more may follow, so a reader that
    /// continues from `next` never sees a character split in two. `finished`
    /// lifts that for the very end of the stream, which nothing will
    /// complete.
    fn read(&self, since: u64, max: usize, finished: bool) -> (String, u64, u64) {
        let since = since.min(self.total);
        let mut from = since.max(self.oldest());
        if from > since {
            // Resuming inside the discarded part may land mid-character.
            from += self.continuation_bytes_at(from);
        }
        let to = (from + max as u64).min(self.total);
        let bytes = self.slice(from, to);
        let keep = if finished && to == self.total {
            bytes.len()
        } else {
            complete_prefix(&bytes)
        };
        let next = from + keep as u64;
        (
            String::from_utf8_lossy(&bytes[..keep]).into_owned(),
            next,
            from - since,
        )
    }

    /// The last `max` bytes, as text, with the end offset.
    fn tail(&self, max: usize, finished: bool) -> (String, u64) {
        let from = self.total.saturating_sub(max as u64).max(self.oldest());
        let from = from + self.continuation_bytes_at(from);
        let (text, next, _) = self.read(from, max, finished);
        (text, next)
    }

    /// UTF-8 continuation bytes at `offset` (at most 3), which belong to a
    /// character that started before it.
    fn continuation_bytes_at(&self, offset: u64) -> u64 {
        let start = (offset - self.oldest()) as usize;
        self.bytes
            .range(start..)
            .take(3)
            .take_while(|byte| (**byte & 0b1100_0000) == 0b1000_0000)
            .count() as u64
    }

    fn slice(&self, from: u64, to: u64) -> Vec<u8> {
        let oldest = self.oldest();
        let (start, end) = ((from - oldest) as usize, (to - oldest) as usize);
        self.bytes.range(start..end.max(start)).copied().collect()
    }
}

/// Length of the longest prefix of `bytes` that does not end inside a UTF-8
/// character. Bytes that are not UTF-8 at all are left for the lossy
/// conversion; only a sequence cut short at the end is held back.
fn complete_prefix(bytes: &[u8]) -> usize {
    let len = bytes.len();
    for back in 1..=len.min(4) {
        let byte = bytes[len - back];
        if (byte & 0b1100_0000) == 0b1000_0000 {
            continue;
        }
        let width = match byte {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => return len,
        };
        return if width > back { len - back } else { len };
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buffer_keeps_the_tail_and_counts_what_it_dropped() {
        let mut buffer = RingBuffer::new(4);
        buffer.push(b"abc");
        buffer.push(b"defg");

        let (chunk, next, dropped) = buffer.read(0, 100, false);
        assert_eq!(chunk, "defg");
        assert_eq!(next, 7);
        assert_eq!(dropped, 3);

        let (chunk, next, dropped) = buffer.read(5, 100, false);
        assert_eq!((chunk.as_str(), next, dropped), ("fg", 7, 0));
    }

    #[test]
    fn a_push_larger_than_the_buffer_keeps_its_own_tail() {
        let mut buffer = RingBuffer::new(3);
        buffer.push(b"0123456789");
        assert_eq!(buffer.read(0, 100, true), ("789".to_string(), 10, 7));
    }

    #[test]
    fn reads_resume_where_the_last_one_ended() {
        let mut buffer = RingBuffer::new(100);
        buffer.push(b"hello world");
        let (first, next, _) = buffer.read(0, 5, false);
        let (second, end, _) = buffer.read(next, 100, false);
        assert_eq!(format!("{first}{second}"), "hello world");
        assert_eq!(end, 11);
        assert_eq!(buffer.read(end, 100, false), (String::new(), 11, 0));
    }

    #[test]
    fn a_character_is_never_split_between_reads() {
        let mut buffer = RingBuffer::new(100);
        buffer.push("aé".as_bytes()); // `é` is two bytes
        let (first, next, _) = buffer.read(0, 2, false);
        assert_eq!((first.as_str(), next), ("a", 1));
        let (second, _, _) = buffer.read(next, 2, false);
        assert_eq!(second, "é");
    }

    #[test]
    fn a_character_half_written_is_held_back_until_it_completes() {
        let mut buffer = RingBuffer::new(100);
        let bytes = "é".as_bytes();
        buffer.push(&bytes[..1]);
        assert_eq!(buffer.read(0, 100, false), (String::new(), 0, 0));
        buffer.push(&bytes[1..]);
        assert_eq!(buffer.read(0, 100, false), ("é".to_string(), 2, 0));
    }

    #[test]
    fn a_discarded_half_character_is_skipped_rather_than_garbled() {
        let mut buffer = RingBuffer::new(3);
        // `a`, 0xC3 0xA9 (`é`), `b`, then `c`: the buffer ends up holding
        // 0xA9 `b` `c`, the first of which is half a character.
        buffer.push("aéb".as_bytes());
        buffer.push(b"c");
        // Held: [0xA9, b, c]. The continuation byte is not shown.
        let (chunk, next, dropped) = buffer.read(0, 100, true);
        assert_eq!(chunk, "bc");
        assert_eq!(next, 5);
        assert_eq!(dropped, 3);
    }

    #[test]
    fn the_tail_is_the_newest_output() {
        let mut buffer = RingBuffer::new(100);
        buffer.push(b"one\ntwo\nthree\n");
        assert_eq!(buffer.tail(6, true), ("three\n".to_string(), 14));
    }
}

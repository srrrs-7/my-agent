//! Background command port.
//!
//! [`super::command::CommandRunner`] runs a command to completion inside one
//! tool call; nothing it starts outlives the call. Some work does not fit in a
//! call - a dev server, a long test suite, a watcher - so this port runs a
//! command *past* the call that started it and lets later calls read its
//! output and stop it.
//!
//! It is a port of its own rather than more methods on `CommandRunner`
//! because the contract differs in the one way that matters: a running job is
//! state that outlives a tool call. Everything a one-shot run guarantees by
//! returning - output bounded, process tree gone - has to be guaranteed here
//! by limits instead, and an implementation must enforce all of
//! [`JobLimits`]: no job without a lifetime cap, no output kept without a
//! bound.
//!
//! Output is a single stream (stdout and stderr interleaved as they arrive)
//! addressed by byte offset. A reader passes the `next` it was last given as
//! `since` and receives only what came after; when the bounded buffer has
//! already discarded some of that, [`JobOutput::dropped`] says how much.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::JobError;
use crate::model::dependency::DependencyReport;
use crate::ports::command::{CommandRequest, SandboxKind};

/// Names one background job for the life of the process.
///
/// A plain counter, not a pid: a pid is reused once the job is reaped, and
/// the model may well ask about a job after that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(u64);

impl JobId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// The bounds every job runs under. Reported, like
/// [`BackgroundCommands::sandbox`], so the tool description tells the model
/// what is actually enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobLimits {
    /// Jobs that may run at once; the next start is refused.
    pub max_jobs: usize,
    /// How long a job may run before it is killed.
    pub max_lifetime: Duration,
    /// Output bytes kept per job. Older output is discarded first.
    pub buffer_bytes: usize,
}

/// Why a job ended other than by exiting on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStop {
    /// Someone asked: `kill`, a reset, or the supervisor going away.
    Killed,
    /// It reached [`JobLimits::max_lifetime`].
    LifetimeExceeded { seconds: u64 },
}

/// What one read of a job saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobOutput {
    /// Output from `since` (or from the oldest byte still kept) onwards, at
    /// most `max` bytes.
    pub chunk: String,
    /// The offset to pass as `since` next time.
    pub next: u64,
    /// Bytes after `since` that the buffer had already discarded, so the
    /// reader knows `chunk` does not continue where it left off.
    pub dropped: u64,
    pub running: bool,
    /// `None` while running, and when the job was killed by a signal.
    pub exit_code: Option<i32>,
    /// Set when the job did not end on its own.
    pub stopped: Option<JobStop>,
    /// What the job may have done to dependency files. Present exactly once:
    /// on the first read (or kill) that observes the job has ended.
    pub dependencies: Option<DependencyReport>,
}

#[async_trait]
pub trait BackgroundCommands: Send + Sync {
    /// The confinement actually in force for every job.
    fn sandbox(&self) -> SandboxKind;

    fn limits(&self) -> JobLimits;

    /// Starts `request` and returns at once.
    ///
    /// `request.timeout` asks for a lifetime, and `max_output_bytes` for a
    /// buffer; each is capped by [`Self::limits`].
    async fn start(&self, request: CommandRequest) -> Result<JobId, JobError>;

    /// Output after `since`, at most `max` bytes. Never waits for more.
    async fn read(&self, id: JobId, since: u64, max: usize) -> Result<JobOutput, JobError>;

    /// Stops the job and everything it started, waits until it is gone, and
    /// reports its last `max` bytes of output - the part a reader stopping a
    /// job usually wants. `dropped` is zero; the job stays readable with
    /// [`Self::read`] for anything earlier. Killing a job that already ended
    /// is not an error: it reports the same way.
    async fn kill(&self, id: JobId, max: usize) -> Result<JobOutput, JobError>;
}

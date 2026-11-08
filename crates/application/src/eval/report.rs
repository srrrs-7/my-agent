//! The evaluation report: one record per attempt, rolled up per task and for
//! the suite, under a fingerprint of what produced it.
//!
//! The report is the product. Two runs are compared by diffing two of these,
//! so the shape is flat numbers wherever possible, durations are whole
//! milliseconds, and the fingerprint says what differed between the runs
//! before anyone reads the numbers.

use std::time::Duration;

use agent_domain::model::llm::TokenUsage;
use agent_domain::ports::events::FinishReason;
use serde::Serialize;

use super::metrics::RunMetrics;
use super::task::TaskId;
use super::verify::CheckOutcome;

/// Bumped whenever a field changes meaning or disappears. Adding a field
/// does not bump it.
pub const REPORT_SCHEMA: u32 = 1;

/// 64-bit FNV-1a.
///
/// Spelled out rather than taken from `DefaultHasher`, whose algorithm the
/// standard library is free to change between releases: a fingerprint that
/// changes when the toolchain does would report a prompt change that never
/// happened.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

/// What produced a report. Equal fingerprints mean the numbers are
/// comparable; a difference is the first thing to look at when they move.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Fingerprint {
    pub agent_version: String,
    pub provider: String,
    pub model: String,
    /// The tools advertised to the model, in the order it sees them.
    pub tools: Vec<String>,
    /// The confinement `run_command` ran under; `None` when the model was not
    /// given the tool.
    pub run_command_sandbox: Option<String>,
    /// The confinement command checks ran under; `None` when no task had one.
    pub check_sandbox: Option<String>,
    /// The loop settings that change behaviour, as the caller resolved them.
    pub settings: serde_json::Value,
    /// [`fnv1a64`] of the system prompt, as 16 hex digits.
    pub system_prompt_fnv1a: String,
}

impl Fingerprint {
    pub fn prompt_hash(prompt: &str) -> String {
        format!("{:016x}", fnv1a64(prompt.as_bytes()))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
}

impl From<TokenUsage> for Tokens {
    fn from(usage: TokenUsage) -> Self {
        Self {
            input: usage.input_tokens,
            output: usage.output_tokens,
            cache_read: usage.cache_read_tokens,
            cache_write: usage.cache_write_tokens,
            total: usage.total(),
        }
    }
}

impl Tokens {
    fn add(&mut self, other: Self) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.total += other.total;
    }
}

/// One run of one task.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AttemptReport {
    /// 1-based, counting `--repeat`.
    pub attempt: u32,
    /// No run error and every check passed. How the loop finished does not
    /// enter into it: a model that wrote the right file and then ran out of
    /// iterations did the work.
    pub passed: bool,
    /// `completed`, `max_iterations`, `stopped`, `interrupted` or `error`.
    pub finish_reason: &'static str,
    pub iterations: u32,
    /// Everything the run cost, the summary requests of compaction included.
    pub tokens: Tokens,
    pub model_requests: u32,
    pub model_latency_ms: u64,
    pub wall_ms: u64,
    pub compactions: u32,
    pub trims: u32,
    pub tool_calls: u32,
    pub tool_errors: u32,
    pub denials: u32,
    pub session_id: String,
    /// Where the scratch workspace was left, with `--keep`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scratch_dir: Option<String>,
    /// Why the run itself failed - the provider broke. Checks still ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub checks: Vec<CheckOutcome>,
}

/// The parts of a run the caller gathered, in one place.
pub struct AttemptInput {
    pub attempt: u32,
    /// The loop's finish reason, or the error that ended the run.
    pub finish: Result<FinishReason, String>,
    pub usage: TokenUsage,
    pub metrics: RunMetrics,
    pub wall: Duration,
    pub session_id: String,
    pub scratch_dir: Option<String>,
    pub checks: Vec<CheckOutcome>,
}

impl AttemptReport {
    pub fn new(input: AttemptInput) -> Self {
        let AttemptInput {
            attempt,
            finish,
            usage,
            metrics,
            wall,
            session_id,
            scratch_dir,
            checks,
        } = input;
        let (finish_reason, error) = match finish {
            Ok(reason) => (finish_label(&reason), None),
            Err(error) => ("error", Some(error)),
        };
        Self {
            attempt,
            passed: error.is_none() && checks.iter().all(|check| check.passed),
            finish_reason,
            iterations: metrics.iterations,
            tokens: usage.into(),
            model_requests: metrics.model_requests,
            model_latency_ms: millis(metrics.model_latency),
            wall_ms: millis(wall),
            compactions: metrics.compactions,
            trims: metrics.trims,
            tool_calls: metrics.tool_calls,
            tool_errors: metrics.tool_errors,
            denials: metrics.denials,
            session_id,
            scratch_dir,
            error,
            checks,
        }
    }
}

fn finish_label(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::Completed => "completed",
        FinishReason::MaxIterations { .. } => "max_iterations",
        FinishReason::Stopped { .. } => "stopped",
        FinishReason::Interrupted => "interrupted",
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Sums over a set of attempts - one task's, or the whole suite's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Totals {
    pub attempts: u32,
    pub passed: u32,
    /// `passed / attempts`, 0 when there were none.
    pub pass_rate: f64,
    pub iterations: u32,
    pub mean_iterations: f64,
    pub tokens: Tokens,
    pub mean_tokens: f64,
    pub model_latency_ms: u64,
    pub wall_ms: u64,
    pub compactions: u32,
    pub trims: u32,
    pub tool_calls: u32,
    pub tool_errors: u32,
    pub denials: u32,
}

impl Totals {
    fn over<'a>(attempts: impl IntoIterator<Item = &'a AttemptReport>) -> Self {
        let mut totals = Self::default();
        for attempt in attempts {
            totals.attempts += 1;
            totals.passed += u32::from(attempt.passed);
            totals.iterations += attempt.iterations;
            totals.tokens.add(attempt.tokens);
            totals.model_latency_ms += attempt.model_latency_ms;
            totals.wall_ms += attempt.wall_ms;
            totals.compactions += attempt.compactions;
            totals.trims += attempt.trims;
            totals.tool_calls += attempt.tool_calls;
            totals.tool_errors += attempt.tool_errors;
            totals.denials += attempt.denials;
        }
        if totals.attempts > 0 {
            let count = f64::from(totals.attempts);
            totals.pass_rate = f64::from(totals.passed) / count;
            totals.mean_iterations = f64::from(totals.iterations) / count;
            // Token totals stay far below 2^53, where the conversion is exact.
            totals.mean_tokens = totals.tokens.total as f64 / count;
        }
        totals
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaskReport {
    pub id: TaskId,
    #[serde(flatten)]
    pub totals: Totals,
    #[serde(rename = "runs")]
    pub attempts: Vec<AttemptReport>,
}

impl TaskReport {
    pub fn new(id: TaskId, attempts: Vec<AttemptReport>) -> Self {
        Self {
            id,
            totals: Totals::over(&attempts),
            attempts,
        }
    }

    pub fn all_passed(&self) -> bool {
        self.attempts.iter().all(|attempt| attempt.passed)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub tasks: usize,
    pub repeat: u32,
    #[serde(flatten)]
    pub totals: Totals,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SuiteReport {
    pub schema: u32,
    pub fingerprint: Fingerprint,
    pub summary: Summary,
    pub tasks: Vec<TaskReport>,
}

impl SuiteReport {
    pub fn new(fingerprint: Fingerprint, repeat: u32, tasks: Vec<TaskReport>) -> Self {
        let totals = Totals::over(tasks.iter().flat_map(|task| &task.attempts));
        Self {
            schema: REPORT_SCHEMA,
            fingerprint,
            summary: Summary {
                tasks: tasks.len(),
                repeat,
                totals,
            },
            tasks,
        }
    }

    /// Every attempt of every task passed.
    pub fn all_passed(&self) -> bool {
        self.tasks.iter().all(TaskReport::all_passed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_matches_the_published_test_vectors() {
        // From the FNV reference (Landon Curt Noll's test suite).
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
        assert_eq!(Fingerprint::prompt_hash("a"), "af63dc4c8601ec8c");
    }

    fn attempt(attempt: u32, passed: bool, iterations: u32, tokens: u64) -> AttemptReport {
        AttemptReport::new(AttemptInput {
            attempt,
            finish: Ok(FinishReason::Completed),
            usage: TokenUsage {
                input_tokens: tokens,
                output_tokens: 0,
                ..TokenUsage::default()
            },
            metrics: RunMetrics {
                iterations,
                ..RunMetrics::default()
            },
            wall: Duration::from_millis(10),
            session_id: format!("s{attempt}"),
            scratch_dir: None,
            checks: vec![CheckOutcome {
                kind: "file_contains",
                target: "x".into(),
                passed,
                detail: String::new(),
            }],
        })
    }

    #[test]
    fn a_run_error_fails_the_attempt_even_when_its_checks_pass() {
        let report = AttemptReport::new(AttemptInput {
            finish: Err("provider down".into()),
            ..AttemptInput {
                attempt: 1,
                finish: Ok(FinishReason::Completed),
                usage: TokenUsage::default(),
                metrics: RunMetrics::default(),
                wall: Duration::ZERO,
                session_id: "s".into(),
                scratch_dir: None,
                checks: Vec::new(),
            }
        });
        assert!(!report.passed);
        assert_eq!(report.finish_reason, "error");
        assert_eq!(report.error.as_deref(), Some("provider down"));
    }

    #[test]
    fn totals_roll_up_per_task_and_for_the_suite() {
        let id = |raw: &str| TaskId::new(raw).unwrap();
        let report = SuiteReport::new(
            Fingerprint {
                agent_version: "0".into(),
                provider: "p".into(),
                model: "m".into(),
                tools: Vec::new(),
                run_command_sandbox: None,
                check_sandbox: None,
                settings: serde_json::Value::Null,
                system_prompt_fnv1a: String::new(),
            },
            2,
            vec![
                TaskReport::new(
                    id("a"),
                    vec![attempt(1, true, 2, 100), attempt(2, true, 4, 300)],
                ),
                TaskReport::new(
                    id("b"),
                    vec![attempt(1, false, 6, 200), attempt(2, true, 4, 200)],
                ),
            ],
        );

        assert_eq!(report.tasks[0].totals.pass_rate, 1.0);
        assert_eq!(report.tasks[1].totals.pass_rate, 0.5);
        assert_eq!(report.summary.totals.attempts, 4);
        assert_eq!(report.summary.totals.passed, 3);
        assert_eq!(report.summary.totals.pass_rate, 0.75);
        assert_eq!(report.summary.totals.iterations, 16);
        assert_eq!(report.summary.totals.mean_iterations, 4.0);
        assert_eq!(report.summary.totals.tokens.total, 800);
        assert_eq!(report.summary.totals.wall_ms, 40);
        assert!(!report.all_passed());

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["summary"]["pass_rate"], 0.75, "{json}");
        assert_eq!(json["tasks"][1]["id"], "b");
        assert_eq!(
            json["tasks"][1]["runs"][0]["checks"][0]["type"],
            "file_contains"
        );
    }
}

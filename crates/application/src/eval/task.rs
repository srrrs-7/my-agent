//! The task suite: what to run, and how to tell whether it worked.
//!
//! One JSON document, because `serde_json` is already a dependency and a YAML
//! or TOML parser would be new supply-chain surface for a file a person writes
//! a few times a year.
//!
//! Strict on purpose. A misspelt check field that was silently ignored would
//! turn a check into "always passes", and a suite whose numbers cannot be
//! trusted is worse than none: unknown fields are rejected, a task without
//! checks is rejected, and ids must be unique so two results can never be
//! attributed to the same name.
//!
//! Paths the *model's* work is checked against (`file_contains.path`) are
//! [`WorkspacePath`]s, so a check cannot read outside the scratch copy. Paths
//! the *operator* names (`fixture`, `prompt_file`) are plain paths, resolved
//! by the caller against the suite file's directory - they are the operator's
//! own files, read by the operator's process, like `AGENT_SYSTEM_PROMPT_FILE`.

use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;

use agent_domain::model::workspace::WorkspacePath;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The only suite format there is so far.
pub const SUITE_VERSION: u32 = 1;

/// Longest task id accepted. Ids end up in session file names and in report
/// keys, so they are meant to be short handles, not descriptions.
const MAX_ID_LEN: usize = 64;

/// A task's name: unique within its suite, stable across runs, and safe to
/// put in a file name.
///
/// The alphabet is the one the session log keeps verbatim in its file names,
/// so `write-hello` in the report is `write-hello-...jsonl` on disk rather
/// than something mangled into it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TaskId(String);

impl TaskId {
    pub fn new(raw: impl Into<String>) -> Result<Self, String> {
        let raw = raw.into();
        if raw.is_empty() || raw.len() > MAX_ID_LEN {
            return Err(format!(
                "task id `{raw}` must be 1 to {MAX_ID_LEN} characters long"
            ));
        }
        if !raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!(
                "task id `{raw}` may only contain ASCII letters, digits, `-` and `_`"
            ));
        }
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TaskId {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::new(raw)
    }
}

impl From<TaskId> for String {
    fn from(id: TaskId) -> Self {
        id.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One way of deciding that a task succeeded. Every check of a task must pass.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Check {
    /// The file exists in the workspace afterwards and contains `contains`
    /// as a substring.
    FileContains {
        path: WorkspacePath,
        contains: String,
    },
    /// A shell line, run in the workspace afterwards under the command
    /// sandbox, exits with status 0.
    Command {
        run: String,
        /// Defaults to [`Check::DEFAULT_COMMAND_TIMEOUT_SECS`].
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
}

impl Check {
    pub const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 120;

    pub fn is_command(&self) -> bool {
        matches!(self, Self::Command { .. })
    }
}

/// Where a task's prompt comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptSource {
    Inline(String),
    /// Relative paths are the caller's to resolve, against the suite file.
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: TaskId,
    /// The directory copied into a fresh scratch workspace for every attempt.
    /// Relative paths are the caller's to resolve, against the suite file.
    pub fixture: PathBuf,
    pub prompt: PromptSource,
    /// Overrides the configured iteration budget for this task only.
    pub max_iterations: Option<u32>,
    pub checks: Vec<Check>,
}

impl Task {
    pub fn has_command_checks(&self) -> bool {
        self.checks.iter().any(Check::is_command)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSuite {
    pub tasks: Vec<Task>,
}

/// Why a suite document was rejected. Read by the operator who wrote it, so
/// every message names the task and what to change.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SuiteError {
    #[error("the task file is not a valid suite: {0}")]
    Malformed(String),
    #[error(
        "the task file declares version {found}; this agent reads version {SUITE_VERSION}. \
         Set \"version\": {SUITE_VERSION}"
    )]
    UnsupportedVersion { found: u32 },
    #[error("the task file has no tasks. Add at least one entry to \"tasks\"")]
    Empty,
    #[error(
        "task `{id}` has no checks, so it would pass whatever the model did. Add at least one \
         \"file_contains\" or \"command\" check"
    )]
    NoChecks { id: TaskId },
    #[error("task id `{id}` appears more than once. Give every task its own id")]
    DuplicateId { id: TaskId },
    #[error("task `{id}` must have exactly one of \"prompt\" and \"prompt_file\"; it has {found}")]
    Prompt { id: TaskId, found: &'static str },
    #[error("task `{id}`: \"max_iterations\" must be at least 1")]
    ZeroIterations { id: TaskId },
}

// --- the document as written --------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSuite {
    version: u32,
    tasks: Vec<RawTask>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTask {
    id: TaskId,
    fixture: PathBuf,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    prompt_file: Option<PathBuf>,
    #[serde(default)]
    max_iterations: Option<u32>,
    checks: Vec<Check>,
}

impl TaskSuite {
    /// Parses and validates a suite document. Nothing is read from disk: the
    /// fixture and prompt paths come back as written.
    pub fn parse(json: &str) -> Result<Self, SuiteError> {
        let raw: RawSuite =
            serde_json::from_str(json).map_err(|error| SuiteError::Malformed(error.to_string()))?;
        if raw.version != SUITE_VERSION {
            return Err(SuiteError::UnsupportedVersion { found: raw.version });
        }
        if raw.tasks.is_empty() {
            return Err(SuiteError::Empty);
        }

        let mut seen = HashSet::new();
        let mut tasks = Vec::with_capacity(raw.tasks.len());
        for task in raw.tasks {
            if !seen.insert(task.id.clone()) {
                return Err(SuiteError::DuplicateId { id: task.id });
            }
            tasks.push(task.validate()?);
        }
        Ok(Self { tasks })
    }

    pub fn has_command_checks(&self) -> bool {
        self.tasks.iter().any(Task::has_command_checks)
    }
}

impl RawTask {
    fn validate(self) -> Result<Task, SuiteError> {
        let id = self.id;
        if self.checks.is_empty() {
            return Err(SuiteError::NoChecks { id });
        }
        if self.max_iterations == Some(0) {
            return Err(SuiteError::ZeroIterations { id });
        }
        let prompt = match (self.prompt, self.prompt_file) {
            (Some(text), None) => PromptSource::Inline(text),
            (None, Some(path)) => PromptSource::File(path),
            (None, None) => {
                return Err(SuiteError::Prompt {
                    id,
                    found: "neither",
                });
            }
            (Some(_), Some(_)) => return Err(SuiteError::Prompt { id, found: "both" }),
        };
        Ok(Task {
            id,
            fixture: self.fixture,
            prompt,
            max_iterations: self.max_iterations,
            checks: self.checks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suite(tasks: &str) -> String {
        format!(r#"{{"version": 1, "tasks": [{tasks}]}}"#)
    }

    const CHECK: &str = r#"{"type": "file_contains", "path": "out.txt", "contains": "hi"}"#;

    #[test]
    fn a_well_formed_suite_parses() {
        let parsed = TaskSuite::parse(&suite(&format!(
            r#"{{"id": "write-hello", "fixture": "fixtures/empty", "prompt": "say hi",
                "max_iterations": 5,
                "checks": [{CHECK}, {{"type": "command", "run": "test -f out.txt"}}]}}"#
        )))
        .unwrap();

        let task = &parsed.tasks[0];
        assert_eq!(task.id.as_str(), "write-hello");
        assert_eq!(task.prompt, PromptSource::Inline("say hi".into()));
        assert_eq!(task.max_iterations, Some(5));
        assert_eq!(
            task.checks[0],
            Check::FileContains {
                path: WorkspacePath::parse("out.txt").unwrap(),
                contains: "hi".into(),
            }
        );
        assert!(parsed.has_command_checks());
    }

    #[test]
    fn unknown_fields_are_rejected_at_every_level() {
        // A typo that was ignored would quietly change what is measured.
        for document in [
            r#"{"version": 1, "tasks": [], "extra": true}"#.to_string(),
            suite(&format!(
                r#"{{"id": "a", "fixture": "f", "prompt": "p", "checks": [{CHECK}], "promt": "x"}}"#
            )),
            suite(
                r#"{"id": "a", "fixture": "f", "prompt": "p",
                    "checks": [{"type": "file_contains", "path": "x", "contains": "y", "contians": "z"}]}"#,
            ),
        ] {
            let error = TaskSuite::parse(&document).unwrap_err();
            assert!(
                matches!(&error, SuiteError::Malformed(message) if message.contains("unknown field")),
                "{document} -> {error:?}"
            );
        }
    }

    #[test]
    fn a_task_without_checks_is_rejected() {
        let error = TaskSuite::parse(&suite(
            r#"{"id": "vacuous", "fixture": "f", "prompt": "p", "checks": []}"#,
        ))
        .unwrap_err();
        assert_eq!(
            error,
            SuiteError::NoChecks {
                id: TaskId::new("vacuous").unwrap()
            }
        );
        assert!(error.to_string().contains("Add at least one"), "{error}");
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let task =
            format!(r#"{{"id": "twin", "fixture": "f", "prompt": "p", "checks": [{CHECK}]}}"#);
        let error = TaskSuite::parse(&suite(&format!("{task}, {task}"))).unwrap_err();
        assert_eq!(
            error,
            SuiteError::DuplicateId {
                id: TaskId::new("twin").unwrap()
            }
        );
    }

    #[test]
    fn a_check_path_that_leaves_the_workspace_is_rejected() {
        for path in ["../outside.txt", "/etc/passwd", "a/../../b"] {
            let error = TaskSuite::parse(&suite(&format!(
                r#"{{"id": "a", "fixture": "f", "prompt": "p",
                    "checks": [{{"type": "file_contains", "path": "{path}", "contains": "x"}}]}}"#
            )))
            .unwrap_err();
            assert!(
                matches!(error, SuiteError::Malformed(_)),
                "{path}: {error:?}"
            );
        }
    }

    #[test]
    fn ids_are_restricted_to_a_file_name_safe_alphabet() {
        for bad in ["", "has space", "../x", "a.b"] {
            assert!(TaskId::new(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(TaskId::new("Fix_bug-12").is_ok());
    }

    #[test]
    fn exactly_one_prompt_source_is_required() {
        let neither = TaskSuite::parse(&suite(&format!(
            r#"{{"id": "a", "fixture": "f", "checks": [{CHECK}]}}"#
        )))
        .unwrap_err();
        assert!(matches!(
            neither,
            SuiteError::Prompt {
                found: "neither",
                ..
            }
        ));

        let both = TaskSuite::parse(&suite(&format!(
            r#"{{"id": "a", "fixture": "f", "prompt": "p", "prompt_file": "p.md", "checks": [{CHECK}]}}"#
        )))
        .unwrap_err();
        assert!(matches!(both, SuiteError::Prompt { found: "both", .. }));
    }

    #[test]
    fn version_and_emptiness_are_checked() {
        assert_eq!(
            TaskSuite::parse(r#"{"version": 2, "tasks": []}"#).unwrap_err(),
            SuiteError::UnsupportedVersion { found: 2 }
        );
        assert_eq!(
            TaskSuite::parse(r#"{"version": 1, "tasks": []}"#).unwrap_err(),
            SuiteError::Empty
        );
    }
}

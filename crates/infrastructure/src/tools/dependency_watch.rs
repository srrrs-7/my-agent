//! Notices when a tool call changed dependency files behind the file tools'
//! back.
//!
//! `write_file` and `edit_file` say what they will write, so a change to
//! `Cargo.toml` through them is approved before it happens (see the
//! dispatcher). A command cannot: `cargo add`, `npm install` and `cargo
//! update` rewrite manifests and lockfiles, and nothing about the command line
//! says which. So this decorator compares the workspace's dependency files
//! before and after the call and reports what changed - at the head of the
//! tool result, where the model reads first and truncation never reaches,
//! and as an [`AgentEvent::DependenciesChanged`] for the terminal.
//!
//! **This detects; it does not prevent.** By the time the comparison runs the
//! command has finished, and a build script it triggered has already run.
//! What it buys is that a change no one approved does not go unnoticed.
//!
//! **Which tools are watched is decided by a property, not by a name:** every
//! tool whose safety is [`ToolSafety::Destructive`] - the class for calls
//! whose effects cannot be read off their arguments. Today that is
//! `run_command`; a future tool that reaches the workspace through something
//! else (an MCP server) is covered by being classified honestly, without
//! anyone remembering to add it to a list. See [`DependencyWatchTool::watches`].
//!
//! It wraps the tool *outside* [`super::TimeoutTool`], so that a command cut
//! off by the timeout is still compared afterwards: a half-finished install
//! is exactly the case worth reporting. An interrupt (Ctrl-C) drops the call
//! before the second scan, and nothing is reported - the dispatcher already
//! tells the model to check what an interrupted call left behind.
//!
//! ## What the scan walks
//!
//! It skips the directories package managers and tools fill with installed
//! copies (`node_modules`, `target`, `.venv`, ...), and directories the
//! workspace's ignore files (`.gitignore`, `.ignore`, the repository's
//! `info/exclude`) exclude. Without the second, a single `venv/` or `build/`
//! uses up the entry limit on every call, and changes past it go unreported.
//!
//! The ignore rules prune **directories only**. A file is compared whatever
//! the rules say about it: `Cargo.lock` is gitignored in many library crates,
//! and a lockfile rewritten behind the user's back matters all the same.
//!
//! `vendor/` is walked on purpose. Unlike `node_modules`, it is committed
//! source the build trusts as-is, so a rewritten vendored manifest is a
//! supply-chain change and exactly what this exists to notice.
//!
//! The scan stops after a fixed number of entries. When it does, the result
//! says so and an [`AgentEvent::DependencyCheckIncomplete`] warns on the
//! terminal - a check that silently covered half the workspace would read as
//! "nothing changed".

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use agent_domain::error::ToolError;
use agent_domain::model::dependency::{DependencyFileChange, FileChange, ManifestKind};
use agent_domain::model::tool::{PlannedWrite, ToolDefinition, ToolOutcome, ToolSafety};
use agent_domain::model::workspace::WorkspaceRoot;
use agent_domain::ports::events::{AgentEvent, EventSink};
use agent_domain::ports::tool::Tool;
use agent_domain::text::printable;
use async_trait::async_trait;
use ignore::WalkBuilder;
use serde_json::Value;

/// Directories whose manifests belong to installed packages or tool state,
/// not to the project, skipped even where no ignore file says so. Compared
/// case-insensitively, like the file names. Not `vendor`: see the module docs.
const SKIPPED_DIRS: [&str; 7] = [
    "node_modules",
    "target",
    ".git",
    ".venv",
    "venv",
    "__pycache__",
    ".tox",
];

/// Directory entries visited per scan before giving up. Two scans per call,
/// so this bounds the cost a large workspace adds to every command.
pub const DEFAULT_MAX_ENTRIES: usize = 20_000;

pub struct DependencyWatchTool {
    inner: Arc<dyn Tool>,
    root: Arc<WorkspaceRoot>,
    events: Arc<dyn EventSink>,
    max_entries: usize,
}

impl DependencyWatchTool {
    pub fn new(inner: Arc<dyn Tool>, root: Arc<WorkspaceRoot>, events: Arc<dyn EventSink>) -> Self {
        Self {
            inner,
            root,
            events,
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }

    pub fn with_max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries;
        self
    }

    /// Whether a tool with `definition` should be wrapped: every destructive
    /// one. Decided by property so that a new tool of that class is watched
    /// without being named here.
    pub fn watches(definition: &ToolDefinition) -> bool {
        definition.safety == ToolSafety::Destructive
    }

    /// Convenience for the composition root.
    pub fn wrap(
        inner: Arc<dyn Tool>,
        root: Arc<WorkspaceRoot>,
        events: Arc<dyn EventSink>,
    ) -> Arc<dyn Tool> {
        Arc::new(Self::new(inner, root, events))
    }

    async fn scan(&self) -> Snapshot {
        let root = self.root.as_path().to_path_buf();
        let limit = self.max_entries;
        tokio::task::spawn_blocking(move || Snapshot::take(&root, limit))
            .await
            // A scan that panicked saw nothing. Marked incomplete, so the
            // result says the check did not happen rather than that nothing
            // changed.
            .unwrap_or_else(|_| Snapshot {
                files: BTreeMap::new(),
                complete: false,
            })
    }
}

#[async_trait]
impl Tool for DependencyWatchTool {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }

    async fn preview(&self, arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
        self.inner.preview(arguments).await
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        let before = self.scan().await;
        let result = self.inner.execute(arguments).await;
        let after = self.scan().await;

        let changes = before.changes_to(&after);
        let complete = before.complete && after.complete;
        if !changes.is_empty() {
            self.events.emit(AgentEvent::DependenciesChanged {
                name: self.inner.definition().name,
                files: changes.clone(),
            });
        }
        if !complete {
            self.events.emit(AgentEvent::DependencyCheckIncomplete {
                name: self.inner.definition().name,
                limit: self.max_entries,
            });
        }

        let Some(note) = note(&changes, complete, self.max_entries) else {
            return result;
        };
        match result {
            Ok(outcome) => Ok(ToolOutcome {
                content: format!("{note}\n\n{}", outcome.content),
                summary: outcome.summary,
            }),
            // The note has to reach the model whichever way the call ended,
            // and only `Execution` carries free text.
            Err(error) => Err(ToolError::execution(
                &self.inner.definition().name,
                format!("{note}\n\n{error}"),
            )),
        }
    }
}

/// What the model is told. `None` when there is nothing to say.
fn note(changes: &[DependencyFileChange], complete: bool, limit: usize) -> Option<String> {
    let mut lines = Vec::new();
    if !changes.is_empty() {
        let files = changes
            .iter()
            .map(|change| {
                format!(
                    "{} ({} {})",
                    printable(&change.path),
                    change.kind.label(),
                    change.change.label()
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!(
            "[dependency files changed by this call: {files}. Nobody reviewed this change \
             before it happened. Tell the user in your reply which dependencies were added, \
             removed or upgraded and why; do not hide it among other work.]"
        ));
    }
    if !complete {
        lines.push(format!(
            "[dependency check incomplete: the workspace scan stopped after {limit} entries, so \
             changes to dependency files beyond that point are not reported.]"
        ));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The dependency files of a workspace at one moment, by content hash.
///
/// The hash only has to notice a change between two scans in one process,
/// so std's SipHash is enough; nothing here needs to resist a collision
/// crafted in advance.
struct Snapshot {
    files: BTreeMap<String, (ManifestKind, Option<u64>)>,
    /// Every entry was visited.
    complete: bool,
}

impl Snapshot {
    /// Directories come from an ignore-aware walk, which prunes them; the
    /// files in each are then listed directly, so no ignore rule ever hides a
    /// file (see the module docs).
    fn take(root: &Path, limit: usize) -> Self {
        let mut files = BTreeMap::new();
        let mut visited = 0usize;

        let directories = WalkBuilder::new(root)
            .hidden(false)
            .git_ignore(true)
            .git_exclude(true)
            // The user's global excludes describe their editor, not this
            // project; letting them prune would make the check differ by
            // machine.
            .git_global(false)
            .ignore(true)
            // Apply ignore rules even when the workspace is not a checkout.
            .require_git(false)
            // A link out of the workspace is never walked into.
            .follow_links(false)
            .filter_entry(|entry| {
                let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
                let skipped = || {
                    let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                    SKIPPED_DIRS.contains(&name.as_str())
                };
                // The root is walked whatever it is called.
                entry.depth() == 0 || (is_dir && !skipped())
            })
            .build();

        for directory in directories.flatten() {
            let Ok(entries) = std::fs::read_dir(directory.path()) else {
                continue;
            };
            let prefix = relative_prefix(root, directory.path());
            for entry in entries.flatten() {
                visited += 1;
                if visited > limit {
                    return Self {
                        files,
                        complete: false,
                    };
                }
                // `file_type` does not follow symlinks; a link named
                // `Cargo.toml` is not a file this directory owns.
                if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(kind) = ManifestKind::classify_name(&name) {
                    files.insert(format!("{prefix}{name}"), (kind, hash_file(&entry.path())));
                }
            }
        }
        Self {
            files,
            complete: true,
        }
    }

    fn changes_to(&self, after: &Self) -> Vec<DependencyFileChange> {
        // When either scan stopped early, a file missing from one of them may
        // simply not have been reached. Only files seen both times can be
        // compared honestly; the note says the rest was not checked.
        let complete = self.complete && after.complete;
        let mut changes = Vec::new();
        for (path, (kind, hash)) in &self.files {
            match after.files.get(path) {
                Some((_, new)) if new != hash => {
                    changes.push(change(path, *kind, FileChange::Modified))
                }
                Some(_) => {}
                None if complete => changes.push(change(path, *kind, FileChange::Deleted)),
                None => {}
            }
        }
        if complete {
            for (path, (kind, _)) in &after.files {
                if !self.files.contains_key(path) {
                    changes.push(change(path, *kind, FileChange::Created));
                }
            }
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes
    }
}

/// `directory` relative to `root`, `/`-separated with a trailing `/`, or
/// empty for the root itself.
fn relative_prefix(root: &Path, directory: &Path) -> String {
    directory
        .strip_prefix(root)
        .unwrap_or(directory)
        .components()
        .map(|component| format!("{}/", component.as_os_str().to_string_lossy()))
        .collect()
}

fn change(path: &str, kind: ManifestKind, change: FileChange) -> DependencyFileChange {
    DependencyFileChange {
        path: path.to_string(),
        kind,
        change,
    }
}

/// `None` for a file that could not be read; two unreadable scans compare
/// equal, a file that became unreadable (or readable) does not.
fn hash_file(path: &Path) -> Option<u64> {
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use agent_domain::model::tool::ToolName;
    use serde_json::json;

    /// Stands in for `run_command`: writes `contents` to `file` under the
    /// workspace, then returns or fails.
    struct FakeCommand {
        root: PathBuf,
        file: &'static str,
        contents: &'static str,
        fail: bool,
    }

    #[async_trait]
    impl Tool for FakeCommand {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: ToolName::new("run_command").unwrap(),
                description: "runs".into(),
                input_schema: json!({"type": "object"}),
                safety: ToolSafety::Destructive,
            }
        }

        async fn preview(&self, _arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
            Ok(Vec::new())
        }

        async fn execute(&self, _arguments: Value) -> Result<ToolOutcome, ToolError> {
            let path = self.root.join(self.file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, self.contents).unwrap();
            if self.fail {
                return Err(ToolError::Timeout {
                    tool: self.definition().name,
                    seconds: 1,
                });
            }
            Ok(ToolOutcome::new("added 1 package"))
        }
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<AgentEvent>>);

    impl EventSink for Recorder {
        fn emit(&self, event: AgentEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        events: Arc<Recorder>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("package.json"), "{\"dependencies\": {}}").unwrap();
            std::fs::write(dir.path().join("index.js"), "").unwrap();
            Self {
                dir,
                events: Arc::new(Recorder::default()),
            }
        }

        fn watch(
            &self,
            file: &'static str,
            contents: &'static str,
            fail: bool,
        ) -> DependencyWatchTool {
            let root = std::fs::canonicalize(self.dir.path()).unwrap();
            let command = FakeCommand {
                root: root.clone(),
                file,
                contents,
                fail,
            };
            DependencyWatchTool::new(
                Arc::new(command),
                Arc::new(WorkspaceRoot::new(root).unwrap()),
                self.events.clone(),
            )
        }

        fn reported(&self) -> Vec<DependencyFileChange> {
            self.events
                .0
                .lock()
                .unwrap()
                .iter()
                .flat_map(|event| match event {
                    AgentEvent::DependenciesChanged { files, .. } => files.clone(),
                    _ => Vec::new(),
                })
                .collect()
        }

        /// The limits of every incomplete-check warning, in order.
        fn incomplete(&self) -> Vec<usize> {
            self.events
                .0
                .lock()
                .unwrap()
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::DependencyCheckIncomplete { limit, .. } => Some(*limit),
                    _ => None,
                })
                .collect()
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.dir.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
    }

    #[tokio::test]
    async fn a_command_that_edits_a_manifest_is_reported_at_the_head_of_its_result() {
        let fixture = Fixture::new();
        let tool = fixture.watch(
            "package.json",
            "{\"dependencies\": {\"left-pad\": \"1\"}}",
            false,
        );

        let outcome = tool.execute(json!({})).await.unwrap();

        assert!(
            outcome
                .content
                .starts_with("[dependency files changed by this call: package.json"),
            "the note comes first, where truncation cannot reach it: {}",
            outcome.content
        );
        assert!(
            outcome.content.ends_with("added 1 package"),
            "{}",
            outcome.content
        );
        assert_eq!(
            fixture.reported(),
            [change(
                "package.json",
                ManifestKind::PackageJson,
                FileChange::Modified
            )]
        );
    }

    #[tokio::test]
    async fn a_created_lockfile_is_reported_as_created() {
        let fixture = Fixture::new();
        let tool = fixture.watch("sub/Cargo.lock", "version = 4\n", false);

        tool.execute(json!({})).await.unwrap();

        assert_eq!(
            fixture.reported(),
            [change(
                "sub/Cargo.lock",
                ManifestKind::CargoLock,
                FileChange::Created
            )]
        );
    }

    #[tokio::test]
    async fn a_call_that_timed_out_is_still_compared() {
        let fixture = Fixture::new();
        let tool = fixture.watch("package.json", "{\"dependencies\": {\"x\": \"1\"}}", true);

        let error = tool.execute(json!({})).await.unwrap_err().to_string();

        assert!(
            error.contains("[dependency files changed by this call"),
            "{error}"
        );
        assert!(
            error.contains("timed out after 1s"),
            "the original error survives: {error}"
        );
        assert_eq!(fixture.reported().len(), 1);
    }

    #[tokio::test]
    async fn manifests_of_installed_packages_are_not_watched() {
        let fixture = Fixture::new();
        let tool = fixture.watch("node_modules/left-pad/package.json", "{}", false);

        let outcome = tool.execute(json!({})).await.unwrap();

        assert_eq!(outcome.content, "added 1 package", "nothing to report");
        assert!(fixture.reported().is_empty());
    }

    #[tokio::test]
    async fn a_command_that_touches_no_dependency_file_is_left_alone() {
        let fixture = Fixture::new();
        let tool = fixture.watch("index.js", "console.log(1)", false);

        let outcome = tool.execute(json!({})).await.unwrap();

        assert_eq!(outcome.content, "added 1 package");
        assert!(fixture.reported().is_empty());
    }

    #[tokio::test]
    async fn a_scan_that_hits_its_limit_says_so() {
        let fixture = Fixture::new();
        let tool = fixture
            .watch("index.js", "console.log(1)", false)
            .with_max_entries(1);

        let outcome = tool.execute(json!({})).await.unwrap();

        assert!(
            outcome.content.starts_with("[dependency check incomplete"),
            "{}",
            outcome.content
        );
        assert_eq!(
            fixture.incomplete(),
            [1],
            "the terminal is warned too, not only the model"
        );
    }

    #[tokio::test]
    async fn a_complete_scan_raises_no_incomplete_warning() {
        let fixture = Fixture::new();
        fixture
            .watch("index.js", "console.log(1)", false)
            .execute(json!({}))
            .await
            .unwrap();
        assert!(fixture.incomplete().is_empty());
    }

    #[tokio::test]
    async fn a_gitignored_directory_is_not_walked() {
        let fixture = Fixture::new();
        fixture.write(".gitignore", "build/\n");
        // Far more entries than the limit allows, all inside the ignored
        // directory: walking into it would leave the scan incomplete.
        for index in 0..50 {
            fixture.write(&format!("build/out/{index}.o"), "");
        }
        let tool = fixture
            .watch("build/out/package.json", "{}", false)
            .with_max_entries(20);

        let outcome = tool.execute(json!({})).await.unwrap();

        assert_eq!(outcome.content, "added 1 package", "nothing to report");
        assert!(fixture.reported().is_empty());
        assert!(fixture.incomplete().is_empty());
    }

    #[tokio::test]
    async fn a_gitignored_lockfile_is_still_compared() {
        let fixture = Fixture::new();
        fixture.write(".gitignore", "Cargo.lock\n");
        fixture.write("Cargo.lock", "version = 3\n");
        let tool = fixture.watch("Cargo.lock", "version = 4\n", false);

        tool.execute(json!({})).await.unwrap();

        assert_eq!(
            fixture.reported(),
            [change(
                "Cargo.lock",
                ManifestKind::CargoLock,
                FileChange::Modified
            )],
            "ignore rules prune directories, never files"
        );
    }

    #[tokio::test]
    async fn a_vendored_manifest_is_watched() {
        let fixture = Fixture::new();
        fixture.write("vendor/x/Cargo.toml", "[package]\nname = \"x\"\n");
        let tool = fixture.watch(
            "vendor/x/Cargo.toml",
            "[package]\nname = \"x\"\nbuild = \"evil.rs\"\n",
            false,
        );

        tool.execute(json!({})).await.unwrap();

        assert_eq!(
            fixture.reported(),
            [change(
                "vendor/x/Cargo.toml",
                ManifestKind::CargoToml,
                FileChange::Modified
            )]
        );
    }

    #[tokio::test]
    async fn tool_state_directories_are_skipped_without_an_ignore_file() {
        for file in [
            "venv/lib/pyproject.toml",
            "__pycache__/lib/pyproject.toml",
            ".tox/lib/pyproject.toml",
        ] {
            let fixture = Fixture::new();
            let outcome = fixture
                .watch(file, "[project]", false)
                .execute(json!({}))
                .await
                .unwrap();
            assert_eq!(outcome.content, "added 1 package", "{file}");
        }
    }

    #[test]
    fn destructive_tools_are_watched_and_others_are_not() {
        let definition = |safety| ToolDefinition {
            name: ToolName::new("any").unwrap(),
            description: String::new(),
            input_schema: json!({}),
            safety,
        };
        assert!(DependencyWatchTool::watches(&definition(
            ToolSafety::Destructive
        )));
        for safety in [
            ToolSafety::ReadOnly,
            ToolSafety::Mutating,
            ToolSafety::Network,
        ] {
            assert!(
                !DependencyWatchTool::watches(&definition(safety)),
                "{safety:?}"
            );
        }
    }

    #[test]
    fn control_characters_in_file_names_do_not_reach_the_model() {
        let shown = note(
            &[change(
                "a\n\u{1b}[2Jb/package.json",
                ManifestKind::PackageJson,
                FileChange::Created,
            )],
            true,
            10,
        )
        .unwrap();
        assert!(
            !shown.chars().any(|c| c.is_control() && c != '\n'),
            "{shown:?}"
        );
        assert_eq!(shown.lines().count(), 1, "{shown:?}");
    }
}

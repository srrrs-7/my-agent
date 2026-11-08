//! Builds the ambient context handed to the model at the start of a run.
//!
//! ## Instruction files
//!
//! Instructions are merged from the general to the specific, so that the
//! more specific text comes later in the prompt and wins a disagreement:
//!
//! 1. the operator's global file (`AGENT_GLOBAL_INSTRUCTIONS`), if any;
//! 2. the workspace root's file - the first of [`INSTRUCTION_FILES`] found.
//!
//! Instruction files in subdirectories are **listed, not loaded**: their paths
//! go into the prompt and the model reads one with `read_file` when it starts
//! working in that directory. Loading every nested `AGENTS.md` up front would
//! spend the prompt on parts of a monorepo the task never touches, and the
//! prompt is built once per session (it is a prompt-cache prefix), so it could
//! not follow the model into a directory later anyway. The search is bounded
//! by [`NESTED_MAX_DEPTH`] and [`NESTED_MAX_FILES`] for the same reason.
//!
//! Every workspace file - root or nested - passes the checks `read_file`
//! makes, by name and by where a symlink leads. The global file does not: the
//! operator named it, it is read by the operator's process, and it usually
//! lives outside the workspace on purpose.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_domain::error::FsError;
use agent_domain::model::access::FileAccess;
use agent_domain::model::context::ContextSnapshot;
use agent_domain::model::workspace::{WorkspacePath, WorkspaceRoot};
use agent_domain::ports::context::ContextProvider;
use agent_domain::text;
use async_trait::async_trait;
use ignore::WalkBuilder;
use tracing::warn;

use super::resolve::ResolvedWorkspace;

/// Instruction files, in precedence order. The first one found in a directory
/// wins - this is the convention Claude Code and Codex established, and
/// following it means an existing repository needs no new file to steer this
/// agent.
const INSTRUCTION_FILES: [&str; 4] = [
    "AGENTS.md",
    "CLAUDE.md",
    ".agent/instructions.md",
    ".github/copilot-instructions.md",
];

/// Bytes of instructions loaded into the prompt, all sources together.
const MAX_INSTRUCTION_BYTES: usize = 16 * 1024;
/// The global file's share when a workspace file is present too: the
/// workspace's own rules are the ones most specific to the task.
const MAX_GLOBAL_SHARE_BYTES: usize = MAX_INSTRUCTION_BYTES / 2;

/// How many directory levels below the root are searched for nested
/// instruction files (`crates/domain/AGENTS.md` is two).
const NESTED_MAX_DEPTH: usize = 3;
/// How many nested instruction files are listed.
const NESTED_MAX_FILES: usize = 20;

/// Depth and size of the overview. Big enough to orient, small enough that it
/// never dominates the prompt.
const OVERVIEW_MAX_DEPTH: usize = 2;
const OVERVIEW_MAX_ENTRIES: usize = 60;

pub struct WorkspaceContextProvider {
    root: Arc<WorkspaceRoot>,
    global_instructions: Option<PathBuf>,
}

impl WorkspaceContextProvider {
    pub fn new(root: Arc<WorkspaceRoot>) -> Self {
        Self {
            root,
            global_instructions: None,
        }
    }

    /// Merges the operator's own instruction file in front of the
    /// workspace's. `path` is read as is - outside the workspace and without
    /// its access policy - because the operator, not the repository, chose it.
    #[must_use]
    pub fn with_global_instructions(mut self, path: Option<PathBuf>) -> Self {
        self.global_instructions = path;
        self
    }
}

#[async_trait]
impl ContextProvider for WorkspaceContextProvider {
    async fn snapshot(&self) -> Result<ContextSnapshot, FsError> {
        let root = self.root.clone();
        let global = self.global_instructions.clone();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        tokio::task::spawn_blocking(move || {
            let base = root.as_path();
            Ok(ContextSnapshot {
                workspace_root: root.display(),
                os: std::env::consts::OS.to_string(),
                today,
                is_git_repository: base.join(".git").exists(),
                project_instructions: read_instructions(&root, global.as_deref()),
                nested_instructions: nested_instructions(&root),
                directory_overview: overview(base),
            })
        })
        .await
        .map_err(|error| FsError::Io {
            path: ".".to_string(),
            message: format!("context task failed: {error}"),
        })?
    }
}

/// The global and root instruction files, labelled and merged general-first,
/// within [`MAX_INSTRUCTION_BYTES`].
fn read_instructions(root: &WorkspaceRoot, global: Option<&Path>) -> Option<String> {
    let global = global.and_then(|path| {
        match std::fs::read_to_string(path) {
            Ok(contents) => Some((path.display().to_string(), contents)),
            Err(error) => {
                warn!(file = %path.display(), %error, "global instructions were not loaded");
                None
            }
        }
        .filter(|(_, contents)| !contents.trim().is_empty())
    });
    let workspace = instruction_file_in(root, &WorkspacePath::root()).and_then(|path| {
        std::fs::read_to_string(root.absolute(&path))
            .ok()
            .filter(|contents| !contents.trim().is_empty())
            .map(|contents| (path.display(), contents))
    });

    let global_budget = if workspace.is_some() {
        MAX_GLOBAL_SHARE_BYTES
    } else {
        MAX_INSTRUCTION_BYTES
    };

    let mut merged = String::new();
    let mut used = 0;
    if let Some((path, contents)) = global {
        let section = section(
            &format!("Global instructions (`{path}`)"),
            &contents,
            global_budget,
            "it is outside the workspace and cannot be read with the tools",
        );
        used += section.len();
        merged.push_str(&section);
    }
    if let Some((path, contents)) = workspace {
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str(&section(
            &format!("`{path}`"),
            &contents,
            MAX_INSTRUCTION_BYTES.saturating_sub(used),
            "read the file for the rest",
        ));
    }

    (!merged.is_empty()).then_some(merged)
}

/// One labelled source, clipped to `budget` with a note saying it was.
fn section(label: &str, contents: &str, budget: usize, rest: &str) -> String {
    let contents = contents.trim();
    let clipped = text::truncate(contents, budget);
    let mut section = format!("## From {label}\n\n{clipped}\n");
    if clipped.len() < contents.len() {
        section.push_str(&format!(
            "\n[truncated: the first {} of {} bytes are shown; {rest}]\n",
            clipped.len(),
            contents.len()
        ));
    }
    section
}

/// The instruction file that governs `dir`, if it may be read: the first of
/// [`INSTRUCTION_FILES`] that exists there.
///
/// When that first file is refused, the directory has none - falling through
/// to the next name would let a repository pick which file is loaded by
/// planting a refused one in front of it.
fn instruction_file_in(root: &WorkspaceRoot, dir: &WorkspacePath) -> Option<WorkspacePath> {
    let path = INSTRUCTION_FILES
        .iter()
        .filter_map(|name| dir.join(name).ok())
        .find(|path| root.absolute(path).is_file())?;

    // The operator's process reads this file, but the repository chooses
    // what it is. `AGENTS.md -> .env` or `-> ~/.aws/credentials` would put
    // the secret in the system prompt without any tool being called.
    match instruction_file_permitted(root, &path) {
        Ok(()) => Some(path),
        Err(error) => {
            warn!(file = %path, %error, "instruction file was not loaded");
            None
        }
    }
}

/// The same two checks `read_file` makes: the name, and where it leads.
fn instruction_file_permitted(
    root: &WorkspaceRoot,
    requested: &WorkspacePath,
) -> Result<(), FsError> {
    let workspace = ResolvedWorkspace::new(root)?;
    let real = std::fs::canonicalize(root.absolute(requested)).map_err(|error| FsError::Io {
        path: requested.display(),
        message: error.to_string(),
    })?;
    workspace.check(requested, &real, FileAccess::Read)
}

/// Paths of the instruction files in subdirectories, sorted, at most
/// [`NESTED_MAX_FILES`] plus a line saying how many more there were.
///
/// The walk honours `.gitignore` and skips hidden directories, so vendored
/// trees (`node_modules/`, `target/`) and `.git/` contribute nothing.
fn nested_instructions(root: &WorkspaceRoot) -> Vec<String> {
    let base = root.as_path();
    let mut found: Vec<String> = WalkBuilder::new(base)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .max_depth(Some(NESTED_MAX_DEPTH))
        .follow_links(false)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.depth() > 0 && entry.file_type().is_some_and(|kind| kind.is_dir()))
        .filter_map(|entry| root.relativize(entry.path()).ok())
        .filter(|dir| root.permits(dir, FileAccess::Read))
        .filter_map(|dir| instruction_file_in(root, &dir))
        .map(|path| path.display())
        .collect();
    found.sort();

    if found.len() > NESTED_MAX_FILES {
        let more = found.len() - NESTED_MAX_FILES;
        found.truncate(NESTED_MAX_FILES);
        found.push(format!(
            "… and {more} more (use `glob` with `**/AGENTS.md` to find them)"
        ));
    }
    found
}

fn overview(base: &Path) -> Vec<String> {
    let mut entries: Vec<String> = WalkBuilder::new(base)
        .hidden(true)
        .git_ignore(true)
        .require_git(false)
        .max_depth(Some(OVERVIEW_MAX_DEPTH))
        .follow_links(false)
        .build()
        .filter_map(Result::ok)
        // depth 0 is the root itself
        .filter(|entry| entry.depth() > 0)
        .filter_map(|entry| {
            let relative = entry.path().strip_prefix(base).ok()?;
            let display = relative.to_string_lossy().replace('\\', "/");
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            Some(if is_dir {
                format!("{display}/")
            } else {
                display
            })
        })
        .take(OVERVIEW_MAX_ENTRIES + 1)
        .collect();

    entries.sort();

    if entries.len() > OVERVIEW_MAX_ENTRIES {
        entries.truncate(OVERVIEW_MAX_ENTRIES);
        entries.push("… (truncated; use list_directory or search_files to explore)".to_string());
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn collects_environment_instructions_and_overview() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "run `make check`").unwrap();
        std::fs::create_dir_all(dir.path().join("crates/domain")).unwrap();
        std::fs::write(dir.path().join("crates/domain/Cargo.toml"), "").unwrap();

        let root = Arc::new(WorkspaceRoot::new(dir.path().to_path_buf()).unwrap());
        let snapshot = WorkspaceContextProvider::new(root)
            .snapshot()
            .await
            .unwrap();

        assert_eq!(
            snapshot.project_instructions.as_deref(),
            Some("## From `AGENTS.md`\n\nrun `make check`\n")
        );
        assert!(!snapshot.is_git_repository);
        assert!(snapshot.directory_overview.contains(&"crates/".to_string()));
        assert!(
            snapshot
                .directory_overview
                .contains(&"crates/domain/".to_string())
        );
        assert_eq!(snapshot.today.len(), 10, "ISO-8601 date");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_instruction_file_that_links_to_a_secret_is_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "API_KEY=sk-from-env").unwrap();
        std::fs::write(outside.path().join("credentials"), "aws_secret=sk-outside").unwrap();
        let root = Arc::new(WorkspaceRoot::new(dir.path().to_path_buf()).unwrap());

        std::os::unix::fs::symlink(".env", dir.path().join("AGENTS.md")).unwrap();
        let snapshot = WorkspaceContextProvider::new(root.clone())
            .snapshot()
            .await
            .unwrap();
        assert_eq!(snapshot.project_instructions, None);

        std::fs::remove_file(dir.path().join("AGENTS.md")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("credentials"),
            dir.path().join("AGENTS.md"),
        )
        .unwrap();
        let snapshot = WorkspaceContextProvider::new(root)
            .snapshot()
            .await
            .unwrap();
        assert_eq!(
            snapshot.project_instructions, None,
            "a link out of the workspace is refused as well"
        );
    }

    #[tokio::test]
    async fn tolerates_a_workspace_without_instructions() {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(WorkspaceRoot::new(dir.path().to_path_buf()).unwrap());
        let snapshot = WorkspaceContextProvider::new(root)
            .snapshot()
            .await
            .unwrap();
        assert!(snapshot.project_instructions.is_none());
        assert!(snapshot.nested_instructions.is_empty());
        assert!(snapshot.directory_overview.is_empty());
    }

    // --- hierarchical instructions --------------------------------------------

    fn write(dir: &Path, relative: &str, contents: &str) {
        let path = dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    async fn snapshot_of(dir: &Path, global: Option<PathBuf>) -> ContextSnapshot {
        let root = Arc::new(WorkspaceRoot::new(dir.to_path_buf()).unwrap());
        WorkspaceContextProvider::new(root)
            .with_global_instructions(global)
            .snapshot()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_global_file_comes_first_and_the_workspace_file_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let global = home.path().join("AGENTS.md");
        std::fs::write(&global, "Always answer in English.").unwrap();
        write(dir.path(), "AGENTS.md", "Run `make check`.");

        let snapshot = snapshot_of(dir.path(), Some(global.clone())).await;
        let merged = snapshot.project_instructions.unwrap();

        let general = merged.find("Always answer in English.").expect("global");
        let specific = merged.find("Run `make check`.").expect("workspace");
        assert!(general < specific, "general before specific: {merged}");
        assert!(
            merged.contains(&format!(
                "## From Global instructions (`{}`)",
                global.display()
            )),
            "{merged}"
        );
        assert!(merged.contains("## From `AGENTS.md`"), "{merged}");
    }

    #[tokio::test]
    async fn the_global_file_is_loaded_on_its_own_and_a_missing_one_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let global = home.path().join("AGENTS.md");
        std::fs::write(&global, "Prefer small diffs.").unwrap();

        let alone = snapshot_of(dir.path(), Some(global)).await;
        assert!(
            alone
                .project_instructions
                .is_some_and(|text| text.contains("Prefer small diffs.")),
            "a workspace without its own file still gets the global one"
        );

        let missing = snapshot_of(dir.path(), Some(home.path().join("gone.md"))).await;
        assert_eq!(missing.project_instructions, None);
    }

    #[tokio::test]
    async fn the_first_instruction_file_in_a_directory_wins() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "CLAUDE.md", "from claude");
        write(dir.path(), ".agent/instructions.md", "from agent dir");

        let snapshot = snapshot_of(dir.path(), None).await;
        let merged = snapshot.project_instructions.unwrap();
        assert!(merged.contains("from claude"), "{merged}");
        assert!(
            !merged.contains("from agent dir"),
            "only one file per directory"
        );

        write(dir.path(), "AGENTS.md", "from agents");
        let merged = snapshot_of(dir.path(), None)
            .await
            .project_instructions
            .unwrap();
        assert!(merged.contains("from agents") && !merged.contains("from claude"));
    }

    #[tokio::test]
    async fn nested_instruction_files_are_listed_not_loaded_one_per_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "AGENTS.md", "root rules");
        write(dir.path(), "crates/domain/AGENTS.md", "domain rules");
        write(dir.path(), "crates/domain/CLAUDE.md", "shadowed");
        write(dir.path(), "web/CLAUDE.md", "web rules");
        write(dir.path(), "a/b/c/AGENTS.md", "three levels down");
        write(dir.path(), "a/b/c/d/AGENTS.md", "too deep");
        write(dir.path(), ".gitignore", "vendor/\n");
        write(dir.path(), "vendor/lib/AGENTS.md", "ignored");

        let snapshot = snapshot_of(dir.path(), None).await;

        assert_eq!(
            snapshot.nested_instructions,
            vec![
                "a/b/c/AGENTS.md",
                "crates/domain/AGENTS.md",
                "web/CLAUDE.md"
            ]
        );
        let merged = snapshot.project_instructions.unwrap();
        assert!(merged.contains("root rules"));
        assert!(
            !merged.contains("domain rules"),
            "nested files are listed for the model to read, not loaded: {merged}"
        );
    }

    #[tokio::test]
    async fn the_nested_list_is_capped_and_says_how_many_more_there_are() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..NESTED_MAX_FILES + 3 {
            write(dir.path(), &format!("pkg{index:02}/AGENTS.md"), "rules");
        }

        let listed = snapshot_of(dir.path(), None).await.nested_instructions;

        assert_eq!(listed.len(), NESTED_MAX_FILES + 1);
        assert!(listed.last().unwrap().contains("and 3 more"), "{listed:?}");
    }

    #[tokio::test]
    async fn oversized_instructions_are_cut_within_the_budget_with_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let global = home.path().join("AGENTS.md");
        std::fs::write(&global, "g".repeat(MAX_INSTRUCTION_BYTES)).unwrap();
        write(dir.path(), "AGENTS.md", &"w".repeat(MAX_INSTRUCTION_BYTES));

        let merged = snapshot_of(dir.path(), Some(global))
            .await
            .project_instructions
            .unwrap();

        // Labels and notes are the only overhead on top of the budget.
        assert!(
            merged.len() < MAX_INSTRUCTION_BYTES + 1024,
            "{}",
            merged.len()
        );
        assert!(
            merged.contains(&format!(
                "[truncated: the first {MAX_GLOBAL_SHARE_BYTES} of {MAX_INSTRUCTION_BYTES} bytes \
                 are shown; it is outside the workspace"
            )),
            "the global file yields half the budget to the workspace's"
        );
        assert!(
            merged.contains("bytes are shown; read the file for the rest]"),
            "the workspace file is cut too, and the model is told it can read the rest"
        );
        assert!(merged.contains("wwww"), "the workspace file is not starved");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn protected_or_escaping_nested_files_are_neither_listed_nor_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(dir.path(), ".env", "API_KEY=sk-from-env");
        write(outside.path(), "credentials", "aws_secret=sk-outside");
        write(dir.path(), "ok/AGENTS.md", "fine");
        std::fs::create_dir_all(dir.path().join("leak")).unwrap();
        std::os::unix::fs::symlink(dir.path().join(".env"), dir.path().join("leak/AGENTS.md"))
            .unwrap();
        std::fs::create_dir_all(dir.path().join("escape")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("credentials"),
            dir.path().join("escape/AGENTS.md"),
        )
        .unwrap();
        // A refused first file does not hand the directory to the next name.
        write(dir.path(), "leak/CLAUDE.md", "planted fallback");
        write(
            dir.path(),
            "secrets.key/AGENTS.md",
            "inside a protected directory",
        );

        let snapshot = snapshot_of(dir.path(), None).await;

        assert_eq!(snapshot.nested_instructions, vec!["ok/AGENTS.md"]);
    }
}

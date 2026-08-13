//! Builds the ambient context handed to the model at the start of a run.

use std::path::Path;
use std::sync::Arc;

use agent_domain::error::FsError;
use agent_domain::model::context::ContextSnapshot;
use agent_domain::model::workspace::WorkspaceRoot;
use agent_domain::ports::context::ContextProvider;
use async_trait::async_trait;
use ignore::WalkBuilder;

/// Instruction files, in precedence order. The first one found wins - this is
/// the convention Claude Code and Codex established, and following it means an
/// existing repository needs no new file to steer this agent.
const INSTRUCTION_FILES: [&str; 4] = [
    "AGENTS.md",
    "CLAUDE.md",
    ".agent/instructions.md",
    ".github/copilot-instructions.md",
];

/// Depth and size of the overview. Big enough to orient, small enough that it
/// never dominates the prompt.
const OVERVIEW_MAX_DEPTH: usize = 2;
const OVERVIEW_MAX_ENTRIES: usize = 60;

pub struct WorkspaceContextProvider {
    root: Arc<WorkspaceRoot>,
}

impl WorkspaceContextProvider {
    pub fn new(root: Arc<WorkspaceRoot>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl ContextProvider for WorkspaceContextProvider {
    async fn snapshot(&self) -> Result<ContextSnapshot, FsError> {
        let root = self.root.clone();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        tokio::task::spawn_blocking(move || {
            let base = root.as_path();
            Ok(ContextSnapshot {
                workspace_root: root.display(),
                os: std::env::consts::OS.to_string(),
                today,
                is_git_repository: base.join(".git").exists(),
                project_instructions: read_instructions(base),
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

fn read_instructions(base: &Path) -> Option<String> {
    INSTRUCTION_FILES
        .iter()
        .map(|name| base.join(name))
        .find(|path| path.is_file())
        .and_then(|path| std::fs::read_to_string(path).ok())
        .filter(|contents| !contents.trim().is_empty())
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
            Some("run `make check`")
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

    #[tokio::test]
    async fn tolerates_a_workspace_without_instructions() {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(WorkspaceRoot::new(dir.path().to_path_buf()).unwrap());
        let snapshot = WorkspaceContextProvider::new(root)
            .snapshot()
            .await
            .unwrap();
        assert!(snapshot.project_instructions.is_none());
        assert!(snapshot.directory_overview.is_empty());
    }
}

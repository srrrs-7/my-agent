//! Content search over the workspace.
//!
//! Built on the `ignore` crate (the walker behind ripgrep) so that `.gitignore`
//! is honoured for free: without it, the first search in any Rust project would
//! drown the model in `target/`.
//!
//! `.gitignore` is a convenience, not a protection - an include glob such as
//! `.env*` overrides it. Files the workspace's access policy refuses to read
//! are skipped on their own account, checked by name and by where they really
//! are, just as `read_file` checks them.

use std::path::Path;
use std::sync::Arc;

use agent_domain::error::FsError;
use agent_domain::model::access::FileAccess;
use agent_domain::model::workspace::{WorkspacePath, WorkspaceRoot};
use agent_domain::ports::search::{FileSearcher, SearchHit, SearchQuery};
use async_trait::async_trait;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use regex::RegexBuilder;

/// Files larger than this are skipped: a match inside a multi-megabyte blob is
/// not useful to the model and reading it is pure latency.
const MAX_SEARCHED_FILE_BYTES: u64 = 1024 * 1024;
/// Bytes inspected when deciding whether a file is binary.
const BINARY_SNIFF_BYTES: usize = 8192;

pub struct IgnoreAwareSearcher {
    root: Arc<WorkspaceRoot>,
}

impl IgnoreAwareSearcher {
    pub fn new(root: Arc<WorkspaceRoot>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl FileSearcher for IgnoreAwareSearcher {
    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchHit>, FsError> {
        let root = self.root.clone();
        // Walking a tree is blocking work; keep it off the async executor.
        tokio::task::spawn_blocking(move || search_blocking(&root, query))
            .await
            .map_err(|error| FsError::Io {
                path: ".".to_string(),
                message: format!("search task failed: {error}"),
            })?
    }
}

fn search_blocking(root: &WorkspaceRoot, query: SearchQuery) -> Result<Vec<SearchHit>, FsError> {
    let regex = RegexBuilder::new(&query.pattern)
        .case_insensitive(query.case_insensitive)
        .build()
        .map_err(|error| FsError::InvalidPattern(error.to_string()))?;

    let base = root.absolute(&query.root);
    if !base.exists() {
        return Err(FsError::NotFound {
            path: query.root.display(),
        });
    }

    // The walker follows a symlink given as its starting point, so the root
    // of the search is resolved and checked like any other path: a link to a
    // directory outside the workspace, or to a protected one, would otherwise
    // be searched in full.
    let real_root = super::canonical_root(root)?;
    let real_base = std::fs::canonicalize(&base).map_err(|error| FsError::Io {
        path: query.root.display(),
        message: error.to_string(),
    })?;
    let real_query_root =
        real_root
            .relativize(&real_base)
            .map_err(|_| FsError::OutsideWorkspace {
                path: query.root.display(),
            })?;
    if !root.permits(&query.root, FileAccess::Read)
        || !real_root.permits(&real_query_root, FileAccess::Read)
    {
        return Err(FsError::AccessDenied {
            path: query.root.display(),
            access: FileAccess::Read,
        });
    }

    let mut builder = WalkBuilder::new(&base);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        // Apply ignore rules even when the workspace is not a git checkout.
        .require_git(false)
        .follow_links(false);

    if let Some(glob) = &query.include_glob {
        let mut overrides = OverrideBuilder::new(&base);
        overrides
            .add(glob)
            .map_err(|error| FsError::InvalidPattern(format!("include glob: {error}")))?;
        builder.overrides(
            overrides
                .build()
                .map_err(|error| FsError::InvalidPattern(format!("include glob: {error}")))?,
        );
    }

    let mut hits = Vec::new();

    for entry in builder.build() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if entry.metadata().map(|meta| meta.len()).unwrap_or(0) > MAX_SEARCHED_FILE_BYTES {
            continue;
        }

        let Ok(path) = root.relativize(entry.path()) else {
            continue;
        };
        // Decided before the file is opened: a refused file is never read.
        if !permitted(root, &path, &real_root, &base, &real_base, entry.path()) {
            continue;
        }

        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        if bytes.iter().take(BINARY_SNIFF_BYTES).any(|byte| *byte == 0) {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };

        for (index, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                hits.push(SearchHit {
                    path: path.clone(),
                    line_number: index as u64 + 1,
                    line: line.to_string(),
                });
                if hits.len() >= query.max_results {
                    return Ok(hits);
                }
            }
        }
    }

    Ok(hits)
}

/// Whether a file the walk found may be read, by its name and by where it
/// really is.
///
/// Below its starting point the walk follows no links, so the real location
/// is the resolved start plus the same tail - no syscall per file. Anything
/// that does not fit that shape is refused rather than guessed at.
fn permitted(
    root: &WorkspaceRoot,
    path: &WorkspacePath,
    real_root: &WorkspaceRoot,
    base: &Path,
    real_base: &Path,
    found: &Path,
) -> bool {
    if !root.permits(path, FileAccess::Read) {
        return false;
    }
    let Ok(tail) = found.strip_prefix(base) else {
        return false;
    };
    let real = if tail.as_os_str().is_empty() {
        real_base.to_path_buf()
    } else {
        real_base.join(tail)
    };
    real_root
        .relativize(&real)
        .is_ok_and(|real| real_root.permits(&real, FileAccess::Read))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::workspace::WorkspacePath;

    fn write(dir: &std::path::Path, relative: &str, contents: &str) {
        let path = dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn query(pattern: &str) -> SearchQuery {
        SearchQuery {
            pattern: pattern.to_string(),
            root: WorkspacePath::root(),
            include_glob: None,
            case_insensitive: false,
            max_results: 100,
        }
    }

    #[tokio::test]
    async fn finds_matches_and_skips_ignored_directories() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "target/\n");
        write(
            dir.path(),
            "src/main.rs",
            "fn main() {\n    let needle = 1;\n}\n",
        );
        write(dir.path(), "target/debug/build.rs", "let needle = 2;\n");

        let root = Arc::new(WorkspaceRoot::new(dir.path().to_path_buf()).unwrap());
        let hits = IgnoreAwareSearcher::new(root)
            .search(query("needle"))
            .await
            .unwrap();

        assert_eq!(hits.len(), 1, "target/ must be skipped, got {hits:?}");
        assert_eq!(hits[0].path.display(), "src/main.rs");
        assert_eq!(hits[0].line_number, 2);
    }

    #[tokio::test]
    async fn honours_the_include_glob_and_the_result_cap() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "needle\nneedle\n");
        write(dir.path(), "b.txt", "needle\n");

        let root = Arc::new(WorkspaceRoot::new(dir.path().to_path_buf()).unwrap());
        let searcher = IgnoreAwareSearcher::new(root);

        let mut scoped = query("needle");
        scoped.include_glob = Some("**/*.rs".into());
        let hits = searcher.search(scoped).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|hit| hit.path.display() == "a.rs"));

        let mut capped = query("needle");
        capped.max_results = 1;
        assert_eq!(searcher.search(capped).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rejects_an_invalid_regex() {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(WorkspaceRoot::new(dir.path().to_path_buf()).unwrap());
        let error = IgnoreAwareSearcher::new(root)
            .search(query("[unclosed"))
            .await
            .unwrap_err();
        assert!(matches!(error, FsError::InvalidPattern(_)));
    }

    #[tokio::test]
    async fn skips_binary_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blob.bin"), b"needle\0\0binary").unwrap();

        let root = Arc::new(WorkspaceRoot::new(dir.path().to_path_buf()).unwrap());
        assert!(
            IgnoreAwareSearcher::new(root)
                .search(query("needle"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    // --- access policy -------------------------------------------------------

    fn searcher(dir: &std::path::Path) -> IgnoreAwareSearcher {
        IgnoreAwareSearcher::new(Arc::new(WorkspaceRoot::new(dir.to_path_buf()).unwrap()))
    }

    #[tokio::test]
    async fn protected_files_are_not_searched_even_when_nothing_ignores_them() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".env", "API_KEY=needle-in-env\n");
        write(dir.path(), "certs/server.pem", "needle-in-pem\n");
        write(dir.path(), "src/lib.rs", "// needle-in-source\n");

        // An include glob overrides `.gitignore` and hidden-file skipping, so
        // this is the search that would reach `.env` if anything could.
        let mut everything = query("needle");
        everything.include_glob = Some("{.env,*.pem,*.rs}".into());
        let hits = searcher(dir.path()).search(everything).await.unwrap();

        let shown: Vec<String> = hits.iter().map(|hit| hit.line.clone()).collect();
        assert_eq!(shown, vec!["// needle-in-source"], "got {hits:?}");
    }

    #[tokio::test]
    async fn searching_inside_a_protected_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".env.d/database", "needle\n");

        let mut scoped = query("needle");
        scoped.root = WorkspacePath::parse(".env.d").unwrap();
        let error = searcher(dir.path()).search(scoped).await.unwrap_err();

        assert!(
            matches!(error, FsError::AccessDenied { .. }),
            "got {error:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_linked_search_root_is_checked_where_it_leads() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "passwd", "needle:x:0:0\n");
        write(dir.path(), "secrets.key/token", "needle\n");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("secrets.key"), dir.path().join("vault"))
            .unwrap();
        let searcher = searcher(dir.path());

        let mut escape = query("needle");
        escape.root = WorkspacePath::parse("escape").unwrap();
        let error = searcher.search(escape).await.unwrap_err();
        assert!(
            matches!(error, FsError::OutsideWorkspace { .. }),
            "got {error:?}"
        );

        let mut vault = query("needle");
        vault.root = WorkspacePath::parse("vault").unwrap();
        let error = searcher.search(vault).await.unwrap_err();
        assert!(
            matches!(error, FsError::AccessDenied { .. }),
            "got {error:?}"
        );
    }
}

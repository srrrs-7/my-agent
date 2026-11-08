//! Content and path search over the workspace.
//!
//! Built on the `ignore` crate (the walker behind ripgrep) so that `.gitignore`
//! is honoured for free: without it, the first search in any Rust project would
//! drown the model in `target/`.
//!
//! `.gitignore` is a convenience, not a protection - an include glob such as
//! `.env*` overrides it. Files the workspace's access policy refuses to read
//! are skipped on their own account, checked by name and by where they really
//! are, just as `read_file` checks them. That holds for [`FileSearcher::glob`]
//! too: a path is not a secret's contents, but listing `.env` invites the next
//! call to try reading it, and the session log's file names are conversation
//! ids.
//!
//! Neither search follows symlinks below its starting point, so a link is
//! never a match: it is neither read nor listed, wherever it leads.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use agent_domain::error::FsError;
use agent_domain::model::access::FileAccess;
use agent_domain::model::workspace::{WorkspacePath, WorkspaceRoot};
use agent_domain::ports::search::{FileSearcher, GlobMatches, GlobQuery, SearchHit, SearchQuery};
use async_trait::async_trait;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use regex::RegexBuilder;

use super::resolve::ResolvedWorkspace;

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

    async fn glob(&self, query: GlobQuery) -> Result<GlobMatches, FsError> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || glob_blocking(&root, query))
            .await
            .map_err(|error| FsError::Io {
                path: ".".to_string(),
                message: format!("glob task failed: {error}"),
            })?
    }
}

fn search_blocking(root: &WorkspaceRoot, query: SearchQuery) -> Result<Vec<SearchHit>, FsError> {
    let regex = RegexBuilder::new(&query.pattern)
        .case_insensitive(query.case_insensitive)
        .build()
        .map_err(|error| FsError::InvalidPattern(error.to_string()))?;

    let WalkStart {
        base,
        workspace,
        real_base,
    } = walk_start(root, &query.root)?;

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
        if !permitted(&workspace, &path, &base, &real_base, entry.path()) {
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

/// Where a walk begins, checked: the subtree as configured and as the
/// filesystem really resolves it.
struct WalkStart {
    base: PathBuf,
    workspace: ResolvedWorkspace,
    real_base: PathBuf,
}

fn walk_start(root: &WorkspaceRoot, subtree: &WorkspacePath) -> Result<WalkStart, FsError> {
    let base = root.absolute(subtree);
    if !base.exists() {
        return Err(FsError::NotFound {
            path: subtree.display(),
        });
    }

    // The walker follows a symlink given as its starting point, so the root
    // of the search is resolved and checked like any other path: a link to a
    // directory outside the workspace, or to a protected one, would otherwise
    // be searched in full.
    let workspace = ResolvedWorkspace::new(root)?;
    let real_base = std::fs::canonicalize(&base).map_err(|error| FsError::Io {
        path: subtree.display(),
        message: error.to_string(),
    })?;
    workspace.check(subtree, &real_base, FileAccess::Read)?;

    Ok(WalkStart {
        base,
        workspace,
        real_base,
    })
}

fn glob_blocking(root: &WorkspaceRoot, query: GlobQuery) -> Result<GlobMatches, FsError> {
    let pattern = query.pattern.trim();
    check_glob(pattern)?;

    let WalkStart {
        base,
        workspace,
        real_base,
    } = walk_start(root, &query.root)?;

    // `Override` is the ignore crate's gitignore-syntax matcher, used here on
    // its own rather than handed to the walker: as a walker override a match
    // would also *un-ignore* the file, and `glob` must honour `.gitignore`
    // exactly as an unfiltered walk does.
    let matcher = {
        let mut builder = OverrideBuilder::new(&base);
        builder
            .add(pattern)
            .map_err(|error| FsError::InvalidPattern(format!("glob: {error}")))?;
        builder
            .build()
            .map_err(|error| FsError::InvalidPattern(format!("glob: {error}")))?
    };

    let mut builder = WalkBuilder::new(&base);
    builder
        // Hidden entries are walked only when the pattern names one: `*.yml`
        // should not surface every file under `.github/` and `.cargo/`, but
        // `.github/**/*.yml` has to be able to match at all.
        .hidden(!names_a_hidden_entry(pattern))
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        // Git's object store is never what a path search is looking for, and
        // walking it is most of the cost once hidden entries are included.
        .filter_entry(|entry| entry.file_name() != ".git");

    let mut found: Vec<(SystemTime, WorkspacePath)> = Vec::new();
    for entry in builder.build() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if !matcher.matched(entry.path(), false).is_whitelist() {
            continue;
        }
        let Ok(path) = root.relativize(entry.path()) else {
            continue;
        };
        if !permitted(&workspace, &path, &base, &real_base, entry.path()) {
            continue;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        found.push((modified, path));
    }

    found.sort_by(|(a_time, a_path), (b_time, b_path)| {
        b_time.cmp(a_time).then_with(|| a_path.cmp(b_path))
    });
    let total = found.len();
    found.truncate(query.max_results);

    Ok(GlobMatches {
        paths: found.into_iter().map(|(_, path)| path).collect(),
        total,
    })
}

/// Refuses a pattern that reaches outside the subtree it is matched in.
///
/// Such a pattern could never match anything the walk visits, so accepting it
/// would answer "no files" - which reads as "nothing like that exists" rather
/// than "you asked the wrong way".
fn check_glob(pattern: &str) -> Result<(), FsError> {
    let refuse = |reason: &str| Err(FsError::InvalidPattern(reason.to_string()));
    if pattern.is_empty() {
        return refuse("the glob is empty. Pass a pattern such as `**/*.rs`");
    }
    if pattern.contains('\0') {
        return refuse("the glob contains a NUL byte");
    }
    let bytes = pattern.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if pattern.starts_with(['/', '\\']) || drive {
        return refuse(
            "the glob must be relative to `path`, not absolute. Drop the leading `/` and pass \
             the directory to search as `path` instead",
        );
    }
    if pattern.split(['/', '\\']).any(|segment| segment == "..") {
        return refuse(
            "the glob must not contain `..`: it only matches inside the workspace. Pass the \
             directory to search as `path` instead",
        );
    }
    Ok(())
}

/// Whether some segment of the pattern starts with a dot (`.github/*`,
/// `**/.env*`), i.e. names an entry the walk would otherwise skip as hidden.
fn names_a_hidden_entry(pattern: &str) -> bool {
    pattern
        .split('/')
        .any(|segment| segment.starts_with('.') && segment != "." && segment != "..")
}

/// Whether a file the walk found may be read, by its name and by where it
/// really is.
///
/// Below its starting point the walk follows no links, so the real location
/// is the resolved start plus the same tail - no syscall per file. Anything
/// that does not fit that shape is refused rather than guessed at.
fn permitted(
    workspace: &ResolvedWorkspace,
    path: &WorkspacePath,
    base: &Path,
    real_base: &Path,
    found: &Path,
) -> bool {
    let Ok(tail) = found.strip_prefix(base) else {
        return false;
    };
    let real = if tail.as_os_str().is_empty() {
        real_base.to_path_buf()
    } else {
        real_base.join(tail)
    };
    workspace.check(path, &real, FileAccess::Read).is_ok()
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

    // --- glob ----------------------------------------------------------------

    fn glob(pattern: &str) -> GlobQuery {
        GlobQuery {
            pattern: pattern.to_string(),
            root: WorkspacePath::root(),
            max_results: 100,
        }
    }

    fn shown(matches: &GlobMatches) -> Vec<String> {
        matches.paths.iter().map(WorkspacePath::display).collect()
    }

    /// Sets a file's modification time without waiting for the clock.
    fn touch(dir: &std::path::Path, relative: &str, seconds_after_epoch: u64) {
        let file = std::fs::File::options()
            .write(true)
            .open(dir.join(relative))
            .unwrap();
        file.set_modified(
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds_after_epoch),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn glob_lists_matching_files_newest_first_then_by_path() {
        let dir = tempfile::tempdir().unwrap();
        for file in ["src/a.rs", "src/b.rs", "src/nested/c.rs", "README.md"] {
            write(dir.path(), file, "");
        }
        touch(dir.path(), "src/a.rs", 1_000);
        touch(dir.path(), "src/b.rs", 1_000);
        touch(dir.path(), "src/nested/c.rs", 2_000);

        let found = searcher(dir.path())
            .glob(glob("src/**/*.rs"))
            .await
            .unwrap();

        assert_eq!(
            shown(&found),
            vec!["src/nested/c.rs", "src/a.rs", "src/b.rs"],
            "newest first, equal times by path"
        );
        assert!(!found.truncated());

        let anywhere = searcher(dir.path()).glob(glob("*.md")).await.unwrap();
        assert_eq!(
            shown(&anywhere),
            vec!["README.md"],
            "a pattern without `/` matches at any depth, and only files"
        );
    }

    #[tokio::test]
    async fn glob_matches_inside_the_given_subtree_only() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "crates/a/lib.rs", "");
        write(dir.path(), "tools/lib.rs", "");

        let mut scoped = glob("*.rs");
        scoped.root = WorkspacePath::parse("crates").unwrap();
        let found = searcher(dir.path()).glob(scoped).await.unwrap();

        assert_eq!(shown(&found), vec!["crates/a/lib.rs"]);
    }

    #[tokio::test]
    async fn glob_honours_gitignore_even_for_a_pattern_naming_the_ignored_files() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "target/\n*.log\n");
        write(dir.path(), "src/main.rs", "");
        write(dir.path(), "target/debug/build.rs", "");
        write(dir.path(), "debug.log", "");

        let searcher = searcher(dir.path());
        assert_eq!(
            shown(&searcher.glob(glob("**/*.rs")).await.unwrap()),
            vec!["src/main.rs"]
        );
        assert!(
            searcher.glob(glob("*.log")).await.unwrap().paths.is_empty(),
            "unlike an include glob, a glob pattern does not un-ignore a file"
        );
    }

    #[tokio::test]
    async fn glob_never_lists_protected_files_or_the_session_log() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".env", "API_KEY=sk-1");
        write(dir.path(), "apps/web/.env.local", "API_KEY=sk-2");
        write(dir.path(), ".env.example", "API_KEY=");
        write(dir.path(), "certs/server.pem", "");
        write(dir.path(), ".agent/sessions/0193-abc.jsonl", "{}");
        write(dir.path(), ".agent/instructions.md", "");
        let policy = agent_domain::model::access::AccessPolicy::standard().with_read_denied(
            agent_domain::model::access::PathPattern::directory(
                &WorkspacePath::parse(".agent/sessions").unwrap(),
            )
            .unwrap(),
        );
        let root = WorkspaceRoot::new(dir.path().to_path_buf())
            .unwrap()
            .with_policy(policy);
        let searcher = IgnoreAwareSearcher::new(Arc::new(root));

        let env = searcher.glob(glob("**/.env*")).await.unwrap();
        assert_eq!(shown(&env), vec![".env.example"], "got {env:?}");

        let agent = searcher.glob(glob(".agent/**/*")).await.unwrap();
        assert_eq!(
            shown(&agent),
            vec![".agent/instructions.md"],
            "got {agent:?}"
        );

        let pem = searcher.glob(glob("**/*.pem")).await.unwrap();
        assert!(pem.paths.is_empty(), "got {pem:?}");
        assert_eq!(pem.total, 0, "a refused file is not even counted");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn glob_does_not_list_a_symlink_that_leads_to_a_protected_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".env", "API_KEY=sk-1");
        write(dir.path(), "secrets.key/token.txt", "sk-2");
        write(dir.path(), "plain.txt", "");
        std::os::unix::fs::symlink(".env", dir.path().join("notes.txt")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("secrets.key"), dir.path().join("vault"))
            .unwrap();
        let searcher = searcher(dir.path());

        let found = searcher.glob(glob("**/*.txt")).await.unwrap();
        assert_eq!(shown(&found), vec!["plain.txt"], "got {found:?}");

        let mut through_link = glob("*.txt");
        through_link.root = WorkspacePath::parse("vault").unwrap();
        let error = searcher.glob(through_link).await.unwrap_err();
        assert!(
            matches!(error, FsError::AccessDenied { .. }),
            "a linked subtree is checked where it leads, got {error:?}"
        );
    }

    #[tokio::test]
    async fn glob_caps_the_list_and_reports_how_many_matched() {
        let dir = tempfile::tempdir().unwrap();
        for file in ["a.rs", "b.rs", "c.rs"] {
            write(dir.path(), file, "");
        }

        let mut capped = glob("*.rs");
        capped.max_results = 2;
        let found = searcher(dir.path()).glob(capped).await.unwrap();

        assert_eq!(found.paths.len(), 2);
        assert_eq!(found.total, 3);
        assert!(found.truncated());
    }

    #[tokio::test]
    async fn glob_refuses_patterns_that_reach_outside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let searcher = searcher(dir.path());

        for pattern in [
            "../*.rs",
            "src/../../etc/*",
            "/etc/passwd",
            "\\\\server\\share",
            "C:/x",
        ] {
            let error = searcher.glob(glob(pattern)).await.unwrap_err();
            assert!(
                matches!(error, FsError::InvalidPattern(_)),
                "`{pattern}` must be refused, got {error:?}"
            );
        }
        let error = searcher.glob(glob("  ")).await.unwrap_err();
        assert!(matches!(error, FsError::InvalidPattern(_)), "got {error:?}");
    }
}

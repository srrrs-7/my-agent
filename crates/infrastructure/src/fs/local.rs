//! Sandboxed local filesystem.
//!
//! [`agent_domain::model::workspace::WorkspacePath`] already guarantees that no
//! `..` component can climb out of the workspace, but that check is *lexical*.
//! A symlink inside the workspace pointing at `/etc` would defeat it, so every
//! access is additionally canonicalised and re-checked against the canonical
//! root here - the one place that can actually touch the filesystem.
//!
//! The workspace's access policy gets the same two passes, for the same
//! reason: once for the name the caller used, once for the file it really
//! reaches. Every read and write passes through [`LocalFileSystem::guard`],
//! so no way of obtaining a `WorkspacePath` gets around either. Listing and
//! existence checks skip the policy - they reveal names, which the model can
//! see in any directory listing, and never contents.

use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;

use agent_domain::error::FsError;
use agent_domain::model::access::FileAccess;
use agent_domain::model::workspace::{WorkspacePath, WorkspaceRoot};
use agent_domain::ports::file_system::{DirEntry, EntryKind, FileSystem};
use async_trait::async_trait;

use super::resolve::ResolvedWorkspace;

pub struct LocalFileSystem {
    workspace: ResolvedWorkspace,
    max_file_bytes: u64,
}

impl LocalFileSystem {
    pub fn new(root: Arc<WorkspaceRoot>, max_file_bytes: u64) -> Result<Self, FsError> {
        Ok(Self {
            workspace: ResolvedWorkspace::new(&root)?,
            max_file_bytes,
        })
    }

    /// Absolute, symlink-resolved location of `path`, checked against the
    /// policy for `access` when one is given (see
    /// [`ResolvedWorkspace::resolve`]).
    fn guard(&self, path: &WorkspacePath, access: Option<FileAccess>) -> Result<PathBuf, FsError> {
        self.workspace.resolve(path, access)
    }

    /// Every read, text or not: guarded for [`FileAccess::Read`], a regular
    /// file, and no longer than `limit`.
    async fn read_capped(&self, path: &WorkspacePath, limit: u64) -> Result<Vec<u8>, FsError> {
        let target = self.guard(path, Some(FileAccess::Read))?;

        let metadata = tokio::fs::metadata(&target)
            .await
            .map_err(|error| io_error(path, error))?;
        if metadata.is_dir() {
            return Err(FsError::NotAFile {
                path: path.display(),
            });
        }
        if metadata.len() > limit {
            return Err(FsError::TooLarge {
                path: path.display(),
                actual: metadata.len(),
                limit,
            });
        }

        tokio::fs::read(&target)
            .await
            .map_err(|error| io_error(path, error))
    }
}

#[async_trait]
impl FileSystem for LocalFileSystem {
    async fn read_to_string(&self, path: &WorkspacePath) -> Result<String, FsError> {
        let bytes = self.read_capped(path, self.max_file_bytes).await?;
        String::from_utf8(bytes).map_err(|_| FsError::NotUtf8 {
            path: path.display(),
        })
    }

    async fn read_bytes(&self, path: &WorkspacePath, limit: u64) -> Result<Vec<u8>, FsError> {
        // The operator's cap (`AGENT_MAX_FILE_BYTES`) bounds every read, not
        // only the text ones.
        self.read_capped(path, limit.min(self.max_file_bytes)).await
    }

    async fn write(&self, path: &WorkspacePath, contents: &str) -> Result<(), FsError> {
        if path.is_root() {
            return Err(FsError::NotAFile {
                path: path.display(),
            });
        }
        let target = self.guard(path, Some(FileAccess::Write))?;

        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| io_error(path, error))?;
        }
        tokio::fs::write(&target, contents)
            .await
            .map_err(|error| io_error(path, error))
    }

    async fn list_dir(&self, path: &WorkspacePath) -> Result<Vec<DirEntry>, FsError> {
        let target = self.guard(path, None)?;

        let metadata = tokio::fs::metadata(&target)
            .await
            .map_err(|error| io_error(path, error))?;
        if !metadata.is_dir() {
            return Err(FsError::NotADirectory {
                path: path.display(),
            });
        }

        let mut reader = tokio::fs::read_dir(&target)
            .await
            .map_err(|error| io_error(path, error))?;
        let mut entries = Vec::new();

        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|error| io_error(path, error))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Build the relative path by extending the parent rather than by
            // stripping the canonical root: the two can differ when the root
            // itself is reached through a symlink.
            let child = path.join(&name).map_err(FsError::from)?;

            let file_type = entry
                .file_type()
                .await
                .map_err(|error| io_error(&child, error))?;
            let kind = if file_type.is_symlink() {
                EntryKind::Symlink
            } else if file_type.is_dir() {
                EntryKind::Directory
            } else {
                EntryKind::File
            };

            let size_bytes = match entry.metadata().await {
                Ok(metadata) => metadata.len(),
                Err(_) => 0,
            };

            entries.push(DirEntry {
                path: child,
                kind,
                size_bytes,
            });
        }

        // Directories first, then alphabetically - the order a human expects.
        entries.sort_by(|a, b| {
            let rank = |kind: EntryKind| match kind {
                EntryKind::Directory => 0,
                EntryKind::File => 1,
                EntryKind::Symlink => 2,
            };
            rank(a.kind)
                .cmp(&rank(b.kind))
                .then_with(|| a.path.display().cmp(&b.path.display()))
        });

        Ok(entries)
    }

    async fn exists(&self, path: &WorkspacePath) -> Result<bool, FsError> {
        let target = self.guard(path, None)?;
        Ok(tokio::fs::try_exists(&target).await.unwrap_or(false))
    }

    async fn remove_file(&self, path: &WorkspacePath) -> Result<(), FsError> {
        if path.is_root() {
            return Err(FsError::NotAFile {
                path: path.display(),
            });
        }
        // Write access, for both the name and what it reaches: deleting a
        // protected file is at least as much a change to it as writing it.
        let target = self.guard(path, Some(FileAccess::Write))?;

        // Asked of the name, not of `target`, which has every link resolved
        // and so can never be one.
        let named = tokio::fs::symlink_metadata(self.workspace.named(path))
            .await
            .map_err(|error| io_error(path, error))?;
        if named.file_type().is_symlink() || named.is_dir() {
            return Err(FsError::NotAFile {
                path: path.display(),
            });
        }
        let metadata = tokio::fs::symlink_metadata(&target)
            .await
            .map_err(|error| io_error(path, error))?;
        if !metadata.is_file() {
            return Err(FsError::NotAFile {
                path: path.display(),
            });
        }

        tokio::fs::remove_file(&target)
            .await
            .map_err(|error| io_error(path, error))
    }

    async fn resolve_real(&self, path: &WorkspacePath) -> Result<WorkspacePath, FsError> {
        // The same resolution a write performs, so the answer names the file
        // the write would reach - not a second, possibly different, opinion.
        let target = self.guard(path, None)?;
        Ok(self.workspace.real_root().relativize(&target)?)
    }
}

fn io_error(path: &WorkspacePath, error: std::io::Error) -> FsError {
    match error.kind() {
        ErrorKind::NotFound => FsError::NotFound {
            path: path.display(),
        },
        ErrorKind::PermissionDenied => FsError::PermissionDenied {
            path: path.display(),
        },
        ErrorKind::IsADirectory => FsError::NotAFile {
            path: path.display(),
        },
        _ => FsError::Io {
            path: path.display(),
            message: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: Arc<WorkspaceRoot>,
        fs: LocalFileSystem,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(WorkspaceRoot::new(dir.path().to_path_buf()).unwrap());
        let fs = LocalFileSystem::new(root.clone(), 1024 * 1024).unwrap();
        Fixture {
            _dir: dir,
            root,
            fs,
        }
    }

    #[tokio::test]
    async fn writes_and_reads_back() {
        let fixture = fixture();
        let path = fixture.root.resolve("src/main.rs").unwrap();

        assert!(!fixture.fs.exists(&path).await.unwrap());
        fixture.fs.write(&path, "fn main() {}").await.unwrap();
        assert!(fixture.fs.exists(&path).await.unwrap());
        assert_eq!(
            fixture.fs.read_to_string(&path).await.unwrap(),
            "fn main() {}"
        );
    }

    #[tokio::test]
    async fn creates_missing_parent_directories() {
        let fixture = fixture();
        let path = fixture.root.resolve("a/b/c/deep.txt").unwrap();
        fixture.fs.write(&path, "x").await.unwrap();
        assert_eq!(fixture.fs.read_to_string(&path).await.unwrap(), "x");
    }

    #[tokio::test]
    async fn lists_directories_first() {
        let fixture = fixture();
        fixture
            .fs
            .write(&fixture.root.resolve("z.txt").unwrap(), "z")
            .await
            .unwrap();
        fixture
            .fs
            .write(&fixture.root.resolve("sub/a.txt").unwrap(), "a")
            .await
            .unwrap();

        let entries = fixture.fs.list_dir(&WorkspacePath::root()).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, EntryKind::Directory);
        assert_eq!(entries[0].path.display(), "sub");
        assert_eq!(entries[1].path.display(), "z.txt");
    }

    #[tokio::test]
    async fn refuses_a_symlink_that_points_outside_the_workspace() {
        let fixture = fixture();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "top secret").unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), fixture.root.as_path().join("escape")).unwrap();

        let path = fixture.root.resolve("escape/secret.txt").unwrap();
        let error = fixture.fs.read_to_string(&path).await.unwrap_err();
        assert!(
            matches!(error, FsError::OutsideWorkspace { .. }),
            "symlink escape must be refused, got {error:?}"
        );
    }

    #[tokio::test]
    async fn reports_a_missing_file_as_not_found() {
        let fixture = fixture();
        let path = fixture.root.resolve("nope.txt").unwrap();
        assert!(matches!(
            fixture.fs.read_to_string(&path).await.unwrap_err(),
            FsError::NotFound { .. }
        ));
    }

    #[tokio::test]
    async fn enforces_the_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(WorkspaceRoot::new(dir.path().to_path_buf()).unwrap());
        let fs = LocalFileSystem::new(root.clone(), 8).unwrap();

        let path = root.resolve("big.txt").unwrap();
        fs.write(&path, "0123456789").await.unwrap();
        assert!(matches!(
            fs.read_to_string(&path).await.unwrap_err(),
            FsError::TooLarge { .. }
        ));
    }

    #[tokio::test]
    async fn reading_a_directory_is_an_error() {
        let fixture = fixture();
        fixture
            .fs
            .write(&fixture.root.resolve("sub/a.txt").unwrap(), "a")
            .await
            .unwrap();
        let path = fixture.root.resolve("sub").unwrap();
        assert!(matches!(
            fixture.fs.read_to_string(&path).await.unwrap_err(),
            FsError::NotAFile { .. }
        ));
    }

    // --- access policy -------------------------------------------------------

    fn put(fixture: &Fixture, relative: &str, contents: &str) {
        let path = fixture.root.as_path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[cfg(unix)]
    fn link(fixture: &Fixture, target: &str, name: &str) {
        std::os::unix::fs::symlink(target, fixture.root.as_path().join(name)).unwrap();
    }

    fn denied(error: &FsError, access: FileAccess) -> bool {
        matches!(error, FsError::AccessDenied { access: refused, .. } if *refused == access)
    }

    #[tokio::test]
    async fn a_protected_file_cannot_be_read_and_the_model_is_told_what_to_do() {
        let fixture = fixture();
        put(&fixture, ".env", "AGENT_API_KEY=sk-secret");

        let path = fixture.root.resolve(".env").unwrap();
        let error = fixture.fs.read_to_string(&path).await.unwrap_err();

        assert!(denied(&error, FileAccess::Read), "got {error:?}");
        let message = error.to_string();
        assert!(message.contains("ask the user"), "{message}");
        assert!(!message.contains("sk-secret"), "{message}");
    }

    #[tokio::test]
    async fn read_bytes_returns_binary_content_under_the_callers_limit() {
        let fixture = fixture();
        let path = fixture.root.resolve("pic.bin").unwrap();
        std::fs::write(
            fixture.root.as_path().join("pic.bin"),
            [0x89, 0x00, 0xFF, 0x10],
        )
        .unwrap();

        assert_eq!(
            fixture.fs.read_bytes(&path, 4).await.unwrap(),
            [0x89, 0x00, 0xFF, 0x10]
        );
        assert!(matches!(
            fixture.fs.read_bytes(&path, 3).await,
            Err(FsError::TooLarge { limit: 3, .. })
        ));
    }

    #[tokio::test]
    async fn read_bytes_never_reads_past_the_operators_file_limit() {
        let fixture = fixture();
        let small = LocalFileSystem::new(fixture.root.clone(), 3).unwrap();
        let path = fixture.root.resolve("pic.bin").unwrap();
        std::fs::write(fixture.root.as_path().join("pic.bin"), [1, 2, 3, 4]).unwrap();

        assert!(
            matches!(
                small.read_bytes(&path, 1024).await,
                Err(FsError::TooLarge { limit: 3, .. })
            ),
            "the lower of the two caps applies, and is the one reported"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_bytes_obeys_the_access_policy_through_a_link() {
        let fixture = fixture();
        put(&fixture, ".env", "SECRET=1");
        link(&fixture, ".env", "innocent.png");

        for name in [".env", "innocent.png"] {
            let path = fixture.root.resolve(name).unwrap();
            let error = fixture.fs.read_bytes(&path, 1024).await.unwrap_err();
            assert!(denied(&error, FileAccess::Read), "{name}: {error:?}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_innocent_name_that_links_to_a_protected_file_is_refused() {
        let fixture = fixture();
        put(&fixture, ".env", "AGENT_API_KEY=sk-secret");
        link(&fixture, ".env", "notes.txt");

        let path = fixture.root.resolve("notes.txt").unwrap();
        let error = fixture.fs.read_to_string(&path).await.unwrap_err();
        assert!(denied(&error, FileAccess::Read), "got {error:?}");
        assert!(
            error.to_string().contains("`notes.txt`") && !error.to_string().contains(".env"),
            "the error names the requested path, not the link target: {error}"
        );

        let error = fixture.fs.write(&path, "overwritten").await.unwrap_err();
        assert!(denied(&error, FileAccess::Write), "got {error:?}");
        assert_eq!(
            std::fs::read_to_string(fixture.root.as_path().join(".env")).unwrap(),
            "AGENT_API_KEY=sk-secret"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_protected_name_is_refused_even_when_it_links_to_an_innocent_file() {
        let fixture = fixture();
        put(&fixture, "config/app.env", "TOKEN=abc");
        link(&fixture, "config/app.env", ".env");

        let path = fixture.root.resolve(".env").unwrap();
        let error = fixture.fs.read_to_string(&path).await.unwrap_err();
        assert!(denied(&error, FileAccess::Read), "got {error:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_linked_directory_does_not_open_a_protected_one() {
        let fixture = fixture();
        put(&fixture, ".git/config", "[core]\n");
        link(&fixture, ".git", "repo");

        let path = fixture.root.resolve("repo/config").unwrap();
        assert_eq!(
            fixture.fs.read_to_string(&path).await.unwrap(),
            "[core]\n",
            "git metadata stays readable"
        );
        let error = fixture
            .fs
            .write(&path, "[core]\n\tfsmonitor = x\n")
            .await
            .unwrap_err();
        assert!(denied(&error, FileAccess::Write), "got {error:?}");

        let hook = fixture.root.resolve("repo/hooks/pre-commit").unwrap();
        let error = fixture.fs.write(&hook, "#!/bin/sh").await.unwrap_err();
        assert!(
            denied(&error, FileAccess::Write),
            "a new file below it too: {error:?}"
        );
        assert!(!fixture.root.as_path().join(".git/hooks").exists());
    }

    #[tokio::test]
    async fn protected_files_cannot_be_created_or_overwritten() {
        let fixture = fixture();
        put(&fixture, ".git/config", "[core]\n");

        for target in [
            ".env",
            "deploy/.env.production",
            ".git/config",
            "server.pem",
        ] {
            let path = fixture.root.resolve(target).unwrap();
            let error = fixture.fs.write(&path, "x").await.unwrap_err();
            assert!(denied(&error, FileAccess::Write), "`{target}`: {error:?}");
        }
        assert!(!fixture.root.as_path().join(".env").exists());
        assert!(!fixture.root.as_path().join("deploy").exists());
        assert_eq!(
            std::fs::read_to_string(fixture.root.as_path().join(".git/config")).unwrap(),
            "[core]\n"
        );
    }

    /// A spelling a case-insensitive filesystem opens as `x.key` is refused
    /// before the filesystem is asked.
    ///
    /// The directory sits next to the crate's sources rather than in the
    /// system temp directory: in the dev container that is the bind-mounted
    /// macOS directory, the filesystem where the aliasing is real, while
    /// `/tmp` is case-sensitive. Where the filesystem does not alias the
    /// names, the refusal is still asserted and only the demonstration that
    /// it matters is skipped.
    #[tokio::test]
    async fn a_unicode_spelling_of_a_protected_name_is_refused_on_a_real_filesystem() {
        let dir = tempfile::Builder::new()
            .prefix(".fold-test-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        std::fs::write(dir.path().join("x.key"), "sk-secret").unwrap();
        let root = Arc::new(WorkspaceRoot::new(dir.path().to_path_buf()).unwrap());
        let fs = LocalFileSystem::new(root.clone(), 1024).unwrap();

        let kelvin = "x.\u{212A}ey";
        match std::fs::read_to_string(dir.path().join(kelvin)) {
            Ok(contents) => assert_eq!(contents, "sk-secret", "the alias reaches `x.key`"),
            Err(error) => {
                assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
                eprintln!("skipped: this filesystem keeps `{kelvin}` and `x.key` apart");
            }
        }

        let path = root.resolve(kelvin).unwrap();
        let error = fs.read_to_string(&path).await.unwrap_err();
        assert!(denied(&error, FileAccess::Read), "got {error:?}");
        assert!(!error.to_string().contains("sk-secret"), "{error}");

        let error = fs.write(&path, "overwritten").await.unwrap_err();
        assert!(denied(&error, FileAccess::Write), "got {error:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("x.key")).unwrap(),
            "sk-secret"
        );
    }

    #[tokio::test]
    async fn the_example_env_file_stays_readable_and_writable() {
        let fixture = fixture();
        let path = fixture.root.resolve(".env.example").unwrap();
        fixture.fs.write(&path, "AGENT_MODEL=").await.unwrap();
        assert_eq!(
            fixture.fs.read_to_string(&path).await.unwrap(),
            "AGENT_MODEL="
        );
    }

    #[tokio::test]
    async fn listing_still_shows_protected_names() {
        let fixture = fixture();
        put(&fixture, ".env", "SECRET=1");

        let entries = fixture.fs.list_dir(&WorkspacePath::root()).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path.display(), ".env");
        assert!(
            fixture
                .fs
                .exists(&fixture.root.resolve(".env").unwrap())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn an_operator_rule_is_enforced_like_a_built_in_one() {
        use agent_domain::model::access::{AccessPolicy, PathPattern};

        let dir = tempfile::tempdir().unwrap();
        let sessions = WorkspacePath::parse(".agent/sessions").unwrap();
        let root = Arc::new(
            WorkspaceRoot::new(dir.path().to_path_buf())
                .unwrap()
                .with_policy(
                    AccessPolicy::standard()
                        .with_read_denied(PathPattern::directory(&sessions).unwrap()),
                ),
        );
        let fs = LocalFileSystem::new(root.clone(), 1024).unwrap();
        std::fs::create_dir_all(dir.path().join(".agent/sessions")).unwrap();
        std::fs::write(dir.path().join(".agent/sessions/s.jsonl"), "{}").unwrap();

        let path = root.resolve(".agent/sessions/s.jsonl").unwrap();
        let error = fs.read_to_string(&path).await.unwrap_err();
        assert!(denied(&error, FileAccess::Read), "got {error:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_dangling_symlink_is_not_written_through() {
        let fixture = fixture();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("created-outside.txt");
        std::os::unix::fs::symlink(&target, fixture.root.as_path().join("notes.md")).unwrap();

        let path = fixture.root.resolve("notes.md").unwrap();
        let error = fixture.fs.write(&path, "escaped").await.unwrap_err();

        assert!(
            !target.exists(),
            "the write must not create the link target outside the workspace"
        );
        assert!(error.to_string().contains("symbolic link"), "{error}");
    }

    #[tokio::test]
    async fn remove_file_deletes_a_file_and_leaves_its_directory() {
        let fixture = fixture();
        put(&fixture, "sub/a.txt", "a");

        let path = fixture.root.resolve("sub/a.txt").unwrap();
        fixture.fs.remove_file(&path).await.unwrap();

        assert!(!fixture.root.as_path().join("sub/a.txt").exists());
        assert!(fixture.root.as_path().join("sub").is_dir());
        assert!(matches!(
            fixture.fs.remove_file(&path).await.unwrap_err(),
            FsError::NotFound { .. }
        ));
    }

    #[tokio::test]
    async fn remove_file_refuses_a_directory() {
        let fixture = fixture();
        put(&fixture, "sub/a.txt", "a");

        let path = fixture.root.resolve("sub").unwrap();
        let error = fixture.fs.remove_file(&path).await.unwrap_err();
        assert!(matches!(error, FsError::NotAFile { .. }), "got {error:?}");
        assert!(fixture.root.as_path().join("sub/a.txt").exists());
    }

    #[tokio::test]
    async fn remove_file_refuses_a_protected_file() {
        let fixture = fixture();
        put(&fixture, ".env", "AGENT_API_KEY=sk-secret");
        put(&fixture, ".git/config", "[core]\n");

        for target in [".env", ".git/config"] {
            let path = fixture.root.resolve(target).unwrap();
            let error = fixture.fs.remove_file(&path).await.unwrap_err();
            assert!(denied(&error, FileAccess::Write), "`{target}`: {error:?}");
            assert!(fixture.root.as_path().join(target).exists(), "{target}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn remove_file_refuses_a_link_and_leaves_what_it_points_at() {
        let fixture = fixture();
        put(&fixture, "real.txt", "keep me");
        link(&fixture, "real.txt", "alias.txt");
        put(&fixture, ".env", "AGENT_API_KEY=sk-secret");
        link(&fixture, ".env", "notes.txt");

        let alias = fixture.root.resolve("alias.txt").unwrap();
        let error = fixture.fs.remove_file(&alias).await.unwrap_err();
        assert!(matches!(error, FsError::NotAFile { .. }), "got {error:?}");
        assert!(fixture.root.as_path().join("alias.txt").is_symlink());
        assert_eq!(
            std::fs::read_to_string(fixture.root.as_path().join("real.txt")).unwrap(),
            "keep me"
        );

        let notes = fixture.root.resolve("notes.txt").unwrap();
        let error = fixture.fs.remove_file(&notes).await.unwrap_err();
        assert!(
            denied(&error, FileAccess::Write),
            "a link to a protected file is refused by the policy first: {error:?}"
        );
        assert!(fixture.root.as_path().join(".env").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn remove_file_refuses_a_file_reached_through_a_link_outside_the_workspace() {
        let fixture = fixture();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("victim.txt"), "outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), fixture.root.as_path().join("escape")).unwrap();

        let path = fixture.root.resolve("escape/victim.txt").unwrap();
        let error = fixture.fs.remove_file(&path).await.unwrap_err();
        assert!(
            matches!(error, FsError::OutsideWorkspace { .. }),
            "got {error:?}"
        );
        assert!(outside.path().join("victim.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_real_names_the_file_a_link_reaches() {
        let fixture = fixture();
        let dir = fixture.root.as_path();
        std::fs::create_dir_all(dir.join("crates/x")).unwrap();
        std::fs::write(dir.join("crates/x/Cargo.toml"), "[package]\n").unwrap();
        std::os::unix::fs::symlink("crates/x/Cargo.toml", dir.join("deps.txt")).unwrap();
        std::os::unix::fs::symlink("crates/x", dir.join("linked")).unwrap();

        let real = |raw: &str| {
            let path = fixture.root.resolve(raw).unwrap();
            let fs = &fixture.fs;
            async move { fs.resolve_real(&path).await.unwrap().display() }
        };
        assert_eq!(real("deps.txt").await, "crates/x/Cargo.toml");
        // A link in the directory part counts too, also for a file to be created.
        assert_eq!(real("linked/Cargo.lock").await, "crates/x/Cargo.lock");
        assert_eq!(real("plain/new.rs").await, "plain/new.rs");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_real_refuses_what_a_write_would_refuse() {
        let fixture = fixture();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), fixture.root.as_path().join("escape")).unwrap();

        let path = fixture.root.resolve("escape/Cargo.toml").unwrap();
        let error = fixture.fs.resolve_real(&path).await.unwrap_err();
        assert!(
            matches!(error, FsError::OutsideWorkspace { .. }),
            "got {error:?}"
        );
    }
}

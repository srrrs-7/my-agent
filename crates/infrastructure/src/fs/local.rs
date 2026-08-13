//! Sandboxed local filesystem.
//!
//! [`agent_domain::model::workspace::WorkspacePath`] already guarantees that no
//! `..` component can climb out of the workspace, but that check is *lexical*.
//! A symlink inside the workspace pointing at `/etc` would defeat it, so every
//! access is additionally canonicalised and re-checked against the canonical
//! root here - the one place that can actually touch the filesystem.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_domain::error::FsError;
use agent_domain::model::workspace::{WorkspacePath, WorkspaceRoot};
use agent_domain::ports::file_system::{DirEntry, EntryKind, FileSystem};
use async_trait::async_trait;

pub struct LocalFileSystem {
    root: Arc<WorkspaceRoot>,
    canonical_root: PathBuf,
    max_file_bytes: u64,
}

impl LocalFileSystem {
    pub fn new(root: Arc<WorkspaceRoot>, max_file_bytes: u64) -> Result<Self, FsError> {
        let canonical_root =
            std::fs::canonicalize(root.as_path()).map_err(|error| FsError::Io {
                path: root.display(),
                message: format!("cannot open the workspace root: {error}"),
            })?;
        Ok(Self {
            root,
            canonical_root,
            max_file_bytes,
        })
    }

    /// Absolute, symlink-resolved location of `path`, or
    /// [`FsError::OutsideWorkspace`] if it leaves the sandbox.
    ///
    /// Works for paths that do not exist yet (needed for `write_file`): the
    /// deepest existing ancestor is canonicalised and the remaining, purely
    /// lexical tail - which contains no `..` - is appended.
    fn guard(&self, path: &WorkspacePath) -> Result<PathBuf, FsError> {
        let absolute = self.root.absolute(path);
        let existing = nearest_existing_ancestor(&absolute);

        let canonical = std::fs::canonicalize(&existing).map_err(|error| FsError::Io {
            path: path.display(),
            message: error.to_string(),
        })?;

        if !canonical.starts_with(&self.canonical_root) {
            return Err(FsError::OutsideWorkspace {
                path: path.display(),
            });
        }

        let tail = absolute.strip_prefix(&existing).unwrap_or(Path::new(""));
        if tail.as_os_str().is_empty() {
            // Joining an empty path appends a trailing separator, and a
            // trailing slash on a regular file makes every syscall fail with
            // ENOTDIR. The path already exists, so there is nothing to append.
            return Ok(canonical);
        }
        Ok(canonical.join(tail))
    }
}

#[async_trait]
impl FileSystem for LocalFileSystem {
    async fn read_to_string(&self, path: &WorkspacePath) -> Result<String, FsError> {
        let target = self.guard(path)?;

        let metadata = tokio::fs::metadata(&target)
            .await
            .map_err(|error| io_error(path, error))?;
        if metadata.is_dir() {
            return Err(FsError::NotAFile {
                path: path.display(),
            });
        }
        if metadata.len() > self.max_file_bytes {
            return Err(FsError::TooLarge {
                path: path.display(),
                actual: metadata.len(),
                limit: self.max_file_bytes,
            });
        }

        let bytes = tokio::fs::read(&target)
            .await
            .map_err(|error| io_error(path, error))?;
        String::from_utf8(bytes).map_err(|_| FsError::NotUtf8 {
            path: path.display(),
        })
    }

    async fn write(&self, path: &WorkspacePath, contents: &str) -> Result<(), FsError> {
        if path.is_root() {
            return Err(FsError::NotAFile {
                path: path.display(),
            });
        }
        let target = self.guard(path)?;

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
        let target = self.guard(path)?;

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
        let target = self.guard(path)?;
        Ok(tokio::fs::try_exists(&target).await.unwrap_or(false))
    }
}

fn nearest_existing_ancestor(path: &Path) -> PathBuf {
    let mut current = path;
    loop {
        if current.exists() {
            return current.to_path_buf();
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return PathBuf::from("/"),
        }
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
}

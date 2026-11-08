//! Throwaway workspaces: a fixture copied into a fresh directory that is
//! removed again when the copy is dropped.
//!
//! What `agent eval` runs each attempt in, so that the model's edits land in
//! a copy and the fixture - and the operator's real workspace - stay as they
//! were.
//!
//! Symbolic links in the fixture are refused rather than copied or followed.
//! Copied as links they would point back out of the scratch directory, and
//! the workspace sandbox (invariant §2) would then be the only thing standing
//! between the model and whatever they name; followed, they would copy that
//! target - possibly a whole home directory - into a place the model can
//! read. Neither is something a fixture needs.
//!
//! `std` only: this runs once per attempt, before anything is asynchronous.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

/// Distinguishes scratch directories made by the same process.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Read by the operator who set up the fixture.
#[derive(Debug, Error)]
pub enum ScratchError {
    #[error("the fixture `{0}` is not a directory")]
    NotADirectory(PathBuf),
    #[error(
        "the fixture contains a symbolic link at `{0}`. Fixtures are copied without links, \
         so a link cannot lead out of the scratch workspace - replace it with a real file or \
         directory"
    )]
    Symlink(PathBuf),
    #[error("the fixture contains `{0}`, which is neither a file nor a directory")]
    Unsupported(PathBuf),
    #[error("cannot prepare the scratch workspace at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> ScratchError + '_ {
    move |source| ScratchError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// A copy of a fixture in a directory of its own. Removed on drop unless
/// [`Self::keep`] was asked for.
#[derive(Debug)]
pub struct ScratchDir {
    path: PathBuf,
    keep: bool,
}

impl ScratchDir {
    /// Copies `fixture` into a new directory under the system temp dir, named
    /// `<prefix><pid>-<n>`.
    pub fn copy_of(fixture: &Path, prefix: &str) -> Result<Self, ScratchError> {
        Self::copy_into(&std::env::temp_dir(), fixture, prefix)
    }

    /// An empty scratch directory under the system temp dir - somewhere to
    /// build things against before any fixture is involved.
    pub fn empty(prefix: &str) -> Result<Self, ScratchError> {
        Self::create(&std::env::temp_dir(), prefix)
    }

    /// Fails the way [`Self::copy_of`] would on what `fixture` holds - a
    /// link, a socket, not a directory at all - without copying anything.
    ///
    /// For checking every fixture of a run up front, before anything is
    /// spent on it. The copy still checks again: the fixture can change in
    /// between.
    pub fn check_fixture(fixture: &Path) -> Result<(), ScratchError> {
        fixture_root(fixture)?;
        check_tree(fixture)
    }

    /// [`Self::copy_of`], under `parent` instead of the system temp dir.
    pub fn copy_into(parent: &Path, fixture: &Path, prefix: &str) -> Result<Self, ScratchError> {
        fixture_root(fixture)?;

        let scratch = Self::create(parent, prefix)?;
        // From here on `scratch` owns the directory, so a copy that fails
        // halfway is cleaned up by the drop on the way out.
        copy_tree(fixture, &scratch.path)?;
        Ok(scratch)
    }

    /// Makes a fresh, empty directory. `create_dir` rather than
    /// `create_dir_all` so that a name already taken - by an earlier run, or
    /// by anything else in a shared temp dir, a planted link included - is
    /// skipped instead of reused.
    fn create(parent: &Path, prefix: &str) -> Result<Self, ScratchError> {
        let pid = std::process::id();
        loop {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!("{prefix}{pid}-{n}"));
            match make_private_dir(&candidate) {
                Ok(()) => {
                    // Canonical, so that it matches what the workspace root
                    // becomes once resolved (`/var` is `/private/var` on
                    // macOS) and the reported path is the real one.
                    let path = std::fs::canonicalize(&candidate).map_err(io_error(&candidate))?;
                    return Ok(Self { path, keep: false });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(ScratchError::Io {
                        path: candidate,
                        source,
                    });
                }
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Leaves the directory in place when this is dropped, for inspecting
    /// what the model did.
    pub fn keep(mut self) -> Self {
        self.keep = true;
        self
    }

    pub fn is_kept(&self) -> bool {
        self.keep
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if !self.keep {
            // Nothing useful to do with a failure here: the directory is in
            // the temp dir, which the system clears eventually.
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Owner-only, so another user on the machine cannot read what the model
/// wrote into it.
fn make_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Refuses a fixture root that is not a directory.
fn fixture_root(fixture: &Path) -> Result<(), ScratchError> {
    // Followed on purpose: the fixture root is the operator's own path and
    // may well sit behind a link. Links *inside* it are the concern.
    let meta = std::fs::metadata(fixture).map_err(io_error(fixture))?;
    if meta.is_dir() {
        Ok(())
    } else {
        Err(ScratchError::NotADirectory(fixture.to_path_buf()))
    }
}

/// [`copy_tree`]'s refusals, without the copy.
fn check_tree(dir: &Path) -> Result<(), ScratchError> {
    for entry in std::fs::read_dir(dir).map_err(io_error(dir))? {
        let entry = entry.map_err(io_error(dir))?;
        let path = entry.path();
        let kind = entry.file_type().map_err(io_error(&path))?;
        if kind.is_symlink() {
            return Err(ScratchError::Symlink(path));
        } else if kind.is_dir() {
            check_tree(&path)?;
        } else if !kind.is_file() {
            return Err(ScratchError::Unsupported(path));
        }
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), ScratchError> {
    for entry in std::fs::read_dir(from).map_err(io_error(from))? {
        let entry = entry.map_err(io_error(from))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        // `file_type` does not follow links, which is the point.
        let kind = entry.file_type().map_err(io_error(&source))?;

        if kind.is_symlink() {
            return Err(ScratchError::Symlink(source));
        } else if kind.is_dir() {
            std::fs::create_dir(&target).map_err(io_error(&target))?;
            copy_tree(&source, &target)?;
        } else if kind.is_file() {
            std::fs::copy(&source, &target).map_err(io_error(&source))?;
        } else {
            return Err(ScratchError::Unsupported(source));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "# fixture\n").unwrap();
        std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        std::fs::write(dir.path().join("src/nested/lib.rs"), "fn f() {}\n").unwrap();
        dir
    }

    #[test]
    fn copies_the_fixture_tree() {
        let fixture = fixture();
        let parent = tempfile::tempdir().unwrap();

        let scratch = ScratchDir::copy_into(parent.path(), fixture.path(), "eval-").unwrap();

        assert!(
            scratch
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("eval-"),
            "{}",
            scratch.path().display()
        );
        assert_eq!(
            std::fs::read_to_string(scratch.path().join("README.md")).unwrap(),
            "# fixture\n"
        );
        assert_eq!(
            std::fs::read_to_string(scratch.path().join("src/nested/lib.rs")).unwrap(),
            "fn f() {}\n"
        );
        // A write into the copy leaves the fixture alone.
        std::fs::write(scratch.path().join("README.md"), "changed").unwrap();
        assert_eq!(
            std::fs::read_to_string(fixture.path().join("README.md")).unwrap(),
            "# fixture\n"
        );
    }

    #[test]
    fn two_copies_of_one_fixture_get_separate_directories() {
        let fixture = fixture();
        let parent = tempfile::tempdir().unwrap();
        let a = ScratchDir::copy_into(parent.path(), fixture.path(), "eval-").unwrap();
        let b = ScratchDir::copy_into(parent.path(), fixture.path(), "eval-").unwrap();
        assert_ne!(a.path(), b.path());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_fixture_containing_a_symlink_and_leaves_nothing_behind() {
        let fixture = fixture();
        std::os::unix::fs::symlink("/etc", fixture.path().join("src/escape")).unwrap();
        let parent = tempfile::tempdir().unwrap();

        let error = ScratchDir::copy_into(parent.path(), fixture.path(), "eval-").unwrap_err();

        assert!(
            matches!(&error, ScratchError::Symlink(path) if path.ends_with("src/escape")),
            "{error:?}"
        );
        assert_eq!(
            std::fs::read_dir(parent.path()).unwrap().count(),
            0,
            "the half-made copy must be removed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn checking_a_fixture_finds_a_nested_symlink_without_copying_anything() {
        let fixture = fixture();
        std::os::unix::fs::symlink("/etc", fixture.path().join("src/nested/escape")).unwrap();

        let error = ScratchDir::check_fixture(fixture.path()).unwrap_err();

        assert!(
            matches!(&error, ScratchError::Symlink(path) if path.ends_with("src/nested/escape")),
            "{error:?}"
        );
        assert!(ScratchDir::check_fixture(self::fixture().path()).is_ok());
    }

    #[test]
    fn a_fixture_that_is_not_a_directory_is_refused() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let error = ScratchDir::copy_into(parent.path(), file.path(), "eval-").unwrap_err();
        assert!(matches!(error, ScratchError::NotADirectory(_)), "{error:?}");
    }

    #[test]
    fn the_copy_is_removed_on_drop() {
        let fixture = fixture();
        let parent = tempfile::tempdir().unwrap();
        let scratch = ScratchDir::copy_into(parent.path(), fixture.path(), "eval-").unwrap();
        let path = scratch.path().to_path_buf();
        assert!(path.is_dir());

        drop(scratch);

        assert!(!path.exists(), "{} survived the drop", path.display());
    }

    #[test]
    fn keep_leaves_the_copy_in_place() {
        let fixture = fixture();
        let parent = tempfile::tempdir().unwrap();
        let scratch = ScratchDir::copy_into(parent.path(), fixture.path(), "eval-")
            .unwrap()
            .keep();
        let path = scratch.path().to_path_buf();
        assert!(scratch.is_kept());

        drop(scratch);

        assert!(path.join("README.md").is_file(), "--keep must keep it");
    }
}

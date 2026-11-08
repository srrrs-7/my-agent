//! Dependency manifests and lockfiles, and what a change to one means.
//!
//! A write to `Cargo.toml` or `package-lock.json` is not like other edits: it
//! decides which third-party code the next build fetches and runs. The rule
//! that such a write needs its own approval is a business rule, so the
//! question "is this file one of those?" is answered here, once, for every
//! layer that asks it.
//!
//! Classification looks at the file name only, compared case-insensitively:
//! a case-insensitive filesystem (macOS, Windows) opens `cargo.toml` as
//! `Cargo.toml`, so a spelling that slips past the comparison would slip past
//! the gate while still changing the real file. The directory does not
//! matter either - a nested crate or a vendored package has a manifest too.
//!
//! What a change *contains* is described by parsers in the application layer.
//! The description is for the human; whether to ask at all never depends on
//! it (see [`DependencyChangeDescription`]).

use std::collections::BTreeMap;
use std::fmt;

use crate::model::workspace::WorkspacePath;

/// A file that declares or pins dependencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ManifestKind {
    CargoToml,
    CargoLock,
    PackageJson,
    PackageLock,
    NpmShrinkwrap,
    YarnLock,
    PnpmLock,
    BunLock,
    Requirements,
    Pyproject,
    Pipfile,
    PipfileLock,
    PoetryLock,
    UvLock,
    GoMod,
    GoSum,
    Gemfile,
    GemfileLock,
    ComposerJson,
    ComposerLock,
}

impl ManifestKind {
    /// The kind of dependency file `path` names, if it names one.
    pub fn classify(path: &WorkspacePath) -> Option<Self> {
        let name = path.as_path().file_name()?.to_string_lossy();
        Self::classify_name(&name)
    }

    /// Like [`Self::classify`], for a bare file name.
    pub fn classify_name(file_name: &str) -> Option<Self> {
        let name = file_name.to_ascii_lowercase();
        let kind = match name.as_str() {
            "cargo.toml" => Self::CargoToml,
            "cargo.lock" => Self::CargoLock,
            "package.json" => Self::PackageJson,
            "package-lock.json" => Self::PackageLock,
            "npm-shrinkwrap.json" => Self::NpmShrinkwrap,
            "yarn.lock" => Self::YarnLock,
            "pnpm-lock.yaml" => Self::PnpmLock,
            "bun.lock" | "bun.lockb" => Self::BunLock,
            "pyproject.toml" => Self::Pyproject,
            "pipfile" => Self::Pipfile,
            "pipfile.lock" => Self::PipfileLock,
            "poetry.lock" => Self::PoetryLock,
            "uv.lock" => Self::UvLock,
            "go.mod" => Self::GoMod,
            "go.sum" => Self::GoSum,
            "gemfile" => Self::Gemfile,
            "gemfile.lock" => Self::GemfileLock,
            "composer.json" => Self::ComposerJson,
            "composer.lock" => Self::ComposerLock,
            // `requirements.txt`, `requirements-dev.txt`, ... - pip takes any
            // name with `-r`, but these are the ones a project commits.
            other if other.starts_with("requirements") && other.ends_with(".txt") => {
                Self::Requirements
            }
            _ => return None,
        };
        Some(kind)
    }

    /// Whether the file pins resolved versions rather than declaring wishes.
    pub fn is_lockfile(self) -> bool {
        matches!(
            self,
            Self::CargoLock
                | Self::PackageLock
                | Self::NpmShrinkwrap
                | Self::YarnLock
                | Self::PnpmLock
                | Self::BunLock
                | Self::PipfileLock
                | Self::PoetryLock
                | Self::UvLock
                | Self::GoSum
                | Self::GemfileLock
                | Self::ComposerLock
        )
    }

    pub fn ecosystem(self) -> &'static str {
        match self {
            Self::CargoToml | Self::CargoLock => "cargo",
            Self::PackageJson
            | Self::PackageLock
            | Self::NpmShrinkwrap
            | Self::YarnLock
            | Self::PnpmLock
            | Self::BunLock => "npm",
            Self::Requirements
            | Self::Pyproject
            | Self::Pipfile
            | Self::PipfileLock
            | Self::PoetryLock
            | Self::UvLock => "python",
            Self::GoMod | Self::GoSum => "go",
            Self::Gemfile | Self::GemfileLock => "ruby",
            Self::ComposerJson | Self::ComposerLock => "php",
        }
    }

    pub fn label(self) -> &'static str {
        if self.is_lockfile() {
            "lockfile"
        } else {
            "manifest"
        }
    }
}

/// One dependency entry that differs between two versions of a file.
///
/// `name` is what the human reads first and is already qualified with the
/// table it came from when that matters (`serde (dev-dependencies)`). A
/// `spec` is the entry's own text - a version requirement, a git URL, a
/// `package = "..."` rename - so that what the build will actually fetch is
/// on screen, not only the name it is filed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyChange {
    Added {
        name: String,
        spec: String,
    },
    Removed {
        name: String,
        spec: String,
    },
    Changed {
        name: String,
        from: String,
        to: String,
    },
}

impl DependencyChange {
    /// Every entry whose spec differs between `before` and `after`, keyed by
    /// name, in name order.
    pub fn between(
        before: &BTreeMap<String, String>,
        after: &BTreeMap<String, String>,
    ) -> Vec<Self> {
        let mut changes = Vec::new();
        for (name, spec) in before {
            match after.get(name) {
                None => changes.push(Self::Removed {
                    name: name.clone(),
                    spec: spec.clone(),
                }),
                Some(new) if new != spec => changes.push(Self::Changed {
                    name: name.clone(),
                    from: spec.clone(),
                    to: new.clone(),
                }),
                Some(_) => {}
            }
        }
        for (name, spec) in after {
            if !before.contains_key(name) {
                changes.push(Self::Added {
                    name: name.clone(),
                    spec: spec.clone(),
                });
            }
        }
        changes.sort_by(|a, b| a.name().cmp(b.name()));
        changes
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Added { name, .. } | Self::Removed { name, .. } | Self::Changed { name, .. } => {
                name
            }
        }
    }
}

impl fmt::Display for DependencyChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // An entry can be all name (`--extra-index-url https://...` in a
            // requirements file), so an empty spec leaves no trailing space.
            Self::Added { name, spec } if spec.is_empty() => write!(f, "+ {name}"),
            Self::Removed { name, spec } if spec.is_empty() => write!(f, "- {name}"),
            Self::Added { name, spec } => write!(f, "+ {name} {spec}"),
            Self::Removed { name, spec } => write!(f, "- {name} {spec}"),
            Self::Changed { name, from, to } => write!(f, "~ {name} {from} -> {to}"),
        }
    }
}

/// What the human is shown about a change to one dependency file.
///
/// Both variants lead to the same question. A file that could not be parsed
/// is still asked about - with the changed lines instead of a dependency
/// list - because a parser that fails, or is fooled, must never be the reason
/// a change goes through unseen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyChangeDescription {
    /// The entries that differ. Never empty: an edit that changed no entry
    /// is described by its lines instead, so that the human is never told
    /// "nothing changed" by a parser that may simply have missed it.
    Dependencies(Vec<DependencyChange>),
    /// The changed lines, when the entries could not be compared - the
    /// format is not understood, this version of the file did not parse, or
    /// no entry differs. `reason` says which. Only the first lines are kept;
    /// `omitted` counts the rest.
    Lines {
        reason: String,
        added: Vec<String>,
        removed: Vec<String>,
        omitted: usize,
    },
}

/// A planned write to one dependency file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestChange {
    pub path: WorkspacePath,
    pub kind: ManifestKind,
    /// The file does not exist yet.
    pub created: bool,
    pub description: DependencyChangeDescription,
}

/// What happened to a dependency file while a command ran, as seen by
/// comparing the workspace before and after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChange {
    Created,
    Modified,
    Deleted,
}

impl FileChange {
    pub fn label(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
        }
    }
}

/// One dependency file a command changed. The path is workspace-relative and
/// `/`-separated; it comes from a directory walk, not from a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyFileChange {
    pub path: String,
    pub kind: ManifestKind,
    pub change: FileChange,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(raw: &str) -> Option<ManifestKind> {
        ManifestKind::classify(&WorkspacePath::parse(raw).unwrap())
    }

    #[test]
    fn manifests_are_recognised_whatever_their_case() {
        // A case-insensitive filesystem opens all of these as the same file.
        for spelling in [
            "Cargo.toml",
            "cargo.toml",
            "CARGO.TOML",
            "crates/x/CaRgO.ToMl",
        ] {
            assert_eq!(
                classify(spelling),
                Some(ManifestKind::CargoToml),
                "{spelling}"
            );
        }
        assert_eq!(classify("Package.JSON"), Some(ManifestKind::PackageJson));
        assert_eq!(classify("gemfile"), Some(ManifestKind::Gemfile));
    }

    #[test]
    fn every_ecosystem_has_its_lockfile_recognised() {
        let lockfiles = [
            ("Cargo.lock", ManifestKind::CargoLock),
            ("package-lock.json", ManifestKind::PackageLock),
            ("npm-shrinkwrap.json", ManifestKind::NpmShrinkwrap),
            ("yarn.lock", ManifestKind::YarnLock),
            ("pnpm-lock.yaml", ManifestKind::PnpmLock),
            ("bun.lockb", ManifestKind::BunLock),
            ("Pipfile.lock", ManifestKind::PipfileLock),
            ("poetry.lock", ManifestKind::PoetryLock),
            ("uv.lock", ManifestKind::UvLock),
            ("go.sum", ManifestKind::GoSum),
            ("Gemfile.lock", ManifestKind::GemfileLock),
            ("composer.lock", ManifestKind::ComposerLock),
        ];
        for (name, kind) in lockfiles {
            assert_eq!(classify(name), Some(kind), "{name}");
            assert!(kind.is_lockfile(), "{name}");
        }
    }

    #[test]
    fn requirement_files_are_recognised_by_prefix() {
        for name in [
            "requirements.txt",
            "requirements-dev.txt",
            "Requirements_CI.TXT",
        ] {
            assert_eq!(classify(name), Some(ManifestKind::Requirements), "{name}");
        }
        assert_eq!(classify("requirements.md"), None);
    }

    #[test]
    fn ordinary_files_are_not_dependency_files() {
        for name in [
            "src/main.rs",
            "Cargo.toml.bak",
            "notes/package.json.md",
            "README.md",
            ".",
        ] {
            assert_eq!(classify(name), None, "{name}");
        }
    }

    #[test]
    fn differences_are_reported_as_adds_removes_and_changes_in_name_order() {
        let before: BTreeMap<String, String> = [("b", "1"), ("c", "1"), ("d", "1")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let after: BTreeMap<String, String> = [("a", "1"), ("c", "2"), ("d", "1")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let shown: Vec<String> = DependencyChange::between(&before, &after)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(shown, ["+ a 1", "- b 1", "~ c 1 -> 2"]);
    }
}

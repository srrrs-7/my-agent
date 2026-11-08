//! Dependency manifests and lockfiles, and what a change to one means.
//!
//! A write to `Cargo.toml` or `package-lock.json` is not like other edits: it
//! decides which third-party code the next build fetches and runs. The rule
//! that such a write needs its own approval is a business rule, so the
//! question "is this file one of those?" is answered here, once, for every
//! layer that asks it.
//!
//! Classification looks at the file name, compared case-insensitively with
//! Unicode case included: a case-insensitive filesystem (macOS, Windows) opens
//! `cargo.toml` - and `package-lock.j\u{17F}on`, with a LONG S - as the real
//! manifest, so a spelling that slips past the comparison would slip past the
//! gate while still changing the real file. Names are compared folded, as the
//! access policy compares them (see `file_name::fold`). The directory does not
//! matter either - a nested crate or a vendored package has a manifest too,
//! and a vendored manifest is committed code the next build trusts, so a
//! change to it is a supply-chain change like any other. (What a scan of the
//! whole workspace may skip is a different question - copies a package
//! manager installed, such as `node_modules` - and is answered where the
//! scan is.) The one exception: Cargo's configuration is called `config`,
//! which is only a dependency file inside a `.cargo` directory.
//!
//! Besides the manifests and lockfiles, the files that decide *where*
//! dependencies are fetched from count too (`.cargo/config.toml`,
//! `.yarnrc.yml`, `pip.conf`, ...): pointing a registry or an index at
//! another server changes what the next build runs as surely as adding a
//! package does. `.npmrc` is absent on purpose - the access policy already
//! refuses to read or write it, since it holds tokens.
//!
//! What a change *contains* is described by parsers in the application layer.
//! The description is for the human; whether to ask at all never depends on
//! it (see [`DependencyChangeDescription`]).

use std::collections::BTreeMap;
use std::fmt;

use crate::model::file_name;
use crate::model::tool::Before;
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
    /// `.cargo/config.toml` / `.cargo/config`: registries, `[source]`
    /// replacement, `[patch]`.
    CargoConfig,
    GoWork,
    GoWorkSum,
    /// `.yarnrc.yml` / `.yarnrc`: registry and plugins.
    YarnConfig,
    /// `.pnpmfile.cjs`: hooks that rewrite resolved packages.
    PnpmHooks,
    BunConfig,
    PipConfig,
    UvConfig,
}

impl ManifestKind {
    /// The kind of dependency file `path` names, if it names one.
    pub fn classify(path: &WorkspacePath) -> Option<Self> {
        let file = path.as_path();
        let name = file.file_name()?.to_string_lossy();
        if let Some(kind) = Self::classify_name(&name) {
            return Some(kind);
        }
        let directory = file_name::fold(&file.parent()?.file_name()?.to_string_lossy());
        let in_cargo_directory = directory == ".cargo";
        let name = file_name::fold(&name);
        let is_cargo_config = name == "config" || name == "config.toml";
        (in_cargo_directory && is_cargo_config).then_some(Self::CargoConfig)
    }

    /// Like [`Self::classify`], for a bare file name - which cannot tell
    /// [`Self::CargoConfig`], whose name only counts inside `.cargo`.
    pub fn classify_name(raw: &str) -> Option<Self> {
        let name = file_name::fold(raw);
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
            "go.work" => Self::GoWork,
            "go.work.sum" => Self::GoWorkSum,
            ".yarnrc.yml" | ".yarnrc" => Self::YarnConfig,
            ".pnpmfile.cjs" => Self::PnpmHooks,
            "bunfig.toml" => Self::BunConfig,
            "pip.conf" => Self::PipConfig,
            "uv.toml" => Self::UvConfig,
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
                | Self::GoWorkSum
                | Self::GemfileLock
                | Self::ComposerLock
        )
    }

    /// Whether the file configures where and how dependencies are fetched,
    /// rather than listing them.
    pub fn is_config(self) -> bool {
        matches!(
            self,
            Self::CargoConfig
                | Self::YarnConfig
                | Self::PnpmHooks
                | Self::BunConfig
                | Self::PipConfig
                | Self::UvConfig
        )
    }

    pub fn ecosystem(self) -> &'static str {
        match self {
            Self::CargoToml | Self::CargoLock | Self::CargoConfig => "cargo",
            Self::PackageJson
            | Self::PackageLock
            | Self::NpmShrinkwrap
            | Self::YarnLock
            | Self::PnpmLock
            | Self::BunLock
            | Self::YarnConfig
            | Self::PnpmHooks
            | Self::BunConfig => "npm",
            Self::Requirements
            | Self::Pyproject
            | Self::Pipfile
            | Self::PipfileLock
            | Self::PoetryLock
            | Self::UvLock
            | Self::PipConfig
            | Self::UvConfig => "python",
            Self::GoMod | Self::GoSum | Self::GoWork | Self::GoWorkSum => "go",
            Self::Gemfile | Self::GemfileLock => "ruby",
            Self::ComposerJson | Self::ComposerLock => "php",
        }
    }

    pub fn label(self) -> &'static str {
        if self.is_lockfile() {
            "lockfile"
        } else if self.is_config() {
            "config"
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
    /// The path the call names.
    pub path: WorkspacePath,
    /// The file actually written, when it is not `path` - a symbolic link
    /// led elsewhere. Shown, because `deps.txt` is not a name anyone would
    /// stop at.
    pub target: Option<WorkspacePath>,
    pub kind: ManifestKind,
    /// The file as it is now. Its text, if any, has already gone into
    /// `description`; what matters here is which of the three cases it is.
    pub before: Before,
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

/// One dependency file a command changed. The path comes from a directory
/// walk, not from a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyFileChange {
    pub path: WorkspacePath,
    pub kind: ManifestKind,
    pub change: FileChange,
}

/// The outcome of comparing the workspace's dependency files across some
/// stretch of work - one command call, or the life of a background job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyReport {
    pub changes: Vec<DependencyFileChange>,
    /// Both scans visited every entry. When not, a change beyond the limit
    /// may have gone unseen, and the note says so rather than implying
    /// "nothing changed".
    pub complete: bool,
    /// The scan's entry limit, the usual reason it stopped.
    pub limit: usize,
}

impl DependencyReport {
    /// What the model is told, put at the head of the result it belongs to.
    /// `None` when there is nothing to say.
    ///
    /// `during` completes "dependency files changed ..." - `by this call`,
    /// `while background job 3 ran` - because whom the change is attributed
    /// to differs, and a background job's window also covers whatever else
    /// happened meanwhile.
    pub fn note(&self, during: &str) -> Option<String> {
        let mut lines = Vec::new();
        if !self.changes.is_empty() {
            let files = self
                .changes
                .iter()
                .map(|change| {
                    format!(
                        "{} ({} {})",
                        crate::text::printable(&change.path.display()),
                        change.kind.label(),
                        change.change.label()
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!(
                "[dependency files changed {during}: {files}. Nobody reviewed this change \
                 before it happened. Tell the user in your reply which dependencies were added, \
                 removed or upgraded and why; do not hide it among other work.]"
            ));
        }
        if !self.complete {
            lines.push(format!(
                "[dependency check incomplete: the workspace scan stopped after {} entries, so \
                 changes to dependency files beyond that point are not reported.]",
                self.limit
            ));
        }
        (!lines.is_empty()).then(|| lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_names_whom_the_change_is_attributed_to() {
        let report = DependencyReport {
            changes: vec![DependencyFileChange {
                path: WorkspacePath::parse("Cargo.toml").unwrap(),
                kind: ManifestKind::CargoToml,
                change: FileChange::Modified,
            }],
            complete: true,
            limit: 10,
        };
        let note = report.note("while background job 3 ran").unwrap();
        assert!(
            note.starts_with("[dependency files changed while background job 3 ran: Cargo.toml"),
            "{note}"
        );
        let quiet = DependencyReport {
            changes: Vec::new(),
            complete: true,
            limit: 10,
        };
        assert_eq!(quiet.note("by this call"), None);
    }

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
    fn unicode_spellings_that_open_a_manifest_are_recognised_as_it() {
        // Each of these opens the ASCII-named file on APFS (macOS, and a
        // Docker bind mount of it); classifying only ASCII case let them
        // change the real manifest without the dependency approval.
        for (spelling, kind) in [
            ("package-lock.j\u{17F}on", ManifestKind::PackageLock),
            ("PACKAGE-LOCK.J\u{17F}ON", ManifestKind::PackageLock),
            ("Cargo.loc\u{212A}", ManifestKind::CargoLock),
            ("go.\u{17F}um", ManifestKind::GoSum),
            ("\u{FB01}xtures/package.json", ManifestKind::PackageJson),
            ("requirement\u{17F}-dev.txt", ManifestKind::Requirements),
            (".cargo/con\u{FB01}g.toml", ManifestKind::CargoConfig),
            (".CARGO/CON\u{FB01}G", ManifestKind::CargoConfig),
        ] {
            assert_eq!(classify(spelling), Some(kind), "{spelling}");
        }
        assert_eq!(
            ManifestKind::classify_name("yarn.loc\u{212A}"),
            Some(ManifestKind::YarnLock)
        );
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

    #[test]
    fn files_that_choose_where_dependencies_come_from_are_recognised_whatever_their_case() {
        let configs = [
            (".cargo/config.toml", ManifestKind::CargoConfig),
            (".cargo/config", ManifestKind::CargoConfig),
            ("crates/x/.CARGO/Config.TOML", ManifestKind::CargoConfig),
            ("go.work", ManifestKind::GoWork),
            ("Go.Work.Sum", ManifestKind::GoWorkSum),
            (".yarnrc.yml", ManifestKind::YarnConfig),
            (".YarnRC", ManifestKind::YarnConfig),
            ("bunfig.toml", ManifestKind::BunConfig),
            (".venv/PIP.CONF", ManifestKind::PipConfig),
            ("uv.toml", ManifestKind::UvConfig),
            (".pnpmfile.cjs", ManifestKind::PnpmHooks),
        ];
        for (name, kind) in configs {
            assert_eq!(classify(name), Some(kind), "{name}");
        }
        assert_eq!(ManifestKind::CargoConfig.label(), "config");
        assert_eq!(ManifestKind::GoWorkSum.label(), "lockfile");
    }

    #[test]
    fn a_config_file_outside_cargo_is_not_a_dependency_file() {
        for name in [
            "config.toml",
            "config",
            "src/config.toml",
            "cargo/config.toml",
        ] {
            assert_eq!(classify(name), None, "{name}");
        }
        assert_eq!(
            ManifestKind::classify_name("config.toml"),
            None,
            "a bare name cannot know its directory"
        );
    }
}

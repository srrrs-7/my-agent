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
    DependencyChange, DependencyChangeDescription, ManifestKind,
};

/// Dependency entries of one file: display name to spec.
pub(crate) type Entries = BTreeMap<String, String>;

/// A parser for one format. The error says, for the human, why the file could
/// not be read as one.
type Parser = fn(&str) -> Result<Entries, String>;

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
                DependencyChangeDescription::Dependencies(changes)
            }
        }
        (Err(error), _) | (_, Err(error)) => lines::describe(
            format!("could not parse {error}; showing the changed lines"),
            before_text,
            after,
        ),
    }
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
            DependencyChangeDescription::Dependencies(changes) => {
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

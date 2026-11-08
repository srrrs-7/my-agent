//! `Cargo.toml` and `Cargo.lock`.
//!
//! Read with a small TOML lexer instead of a TOML crate (see the module
//! above). It knows strings - basic, literal and both multi-line forms -
//! comments, and bracket nesting, which is what it takes to split a file into
//! statements and find the dependency entries among them. Anything it cannot
//! make sense of is an error, and the caller falls back to showing lines.

use super::{Entries, insert};

/// Tables whose keys are dependencies, whatever they are nested under
/// (`[target.'cfg(unix)'.dependencies]`, `[workspace.dependencies]`).
const DEPENDENCY_TABLES: [&str; 5] = [
    "dependencies",
    "dev-dependencies",
    "dev_dependencies",
    "build-dependencies",
    "build_dependencies",
];

/// The registries whose `source` goes unmentioned: anything else - a git
/// repository, another registry, a path - is worth seeing.
const DEFAULT_SOURCES: [&str; 2] = [
    "registry+https://github.com/rust-lang/crates.io-index",
    "sparse+https://index.crates.io/",
];

/// Characters of a checksum shown. Enough to notice that it changed.
const CHECKSUM_CHARS: usize = 12;

/// The dependency entries of a `Cargo.toml`.
///
/// An entry is named after the crate key and, unless it is a plain
/// `[dependencies]` entry, the table it sits in. Its spec is its own text:
/// `"1"`, `{ version = "1", package = "other" }`, `{ git = "..." }`. Entries
/// split over a `[dependencies.serde]` table or dotted keys are put back
/// together. `[patch.<source>]` and `[replace]` count as dependency tables:
/// they change what every other entry resolves to.
pub(super) fn manifest(text: &str) -> Result<Entries, String> {
    let mut parts: Vec<(String, String)> = Vec::new();
    let mut header: Vec<String> = Vec::new();

    for statement in statements(text)? {
        if let Some(inner) = table_header(&statement) {
            header = split_dotted(inner)?;
            continue;
        }
        let (key, value) = split_assignment(&statement)?;
        let mut path = header.clone();
        path.extend(split_dotted(key)?);

        let Some(table) = dependency_table(&path) else {
            continue;
        };
        let label = path[..=table].join(".");
        let value = collapse_whitespace(value);
        match path.get(table + 1) {
            // `dependencies = { serde = "1" }`: the whole table is one value.
            None => parts.push((format!("({label})"), value)),
            Some(name) => {
                let name = if label == "dependencies" {
                    name.clone()
                } else {
                    format!("{name} ({label})")
                };
                let field = path[table + 2..].join(".");
                let part = if field.is_empty() {
                    value
                } else {
                    format!("{field} = {value}")
                };
                parts.push((name, part));
            }
        }
    }

    // Fields of one entry are kept in file order, which is the order a human
    // reading the file would see them in.
    let mut entries = Entries::new();
    for (name, part) in parts {
        entries
            .entry(name)
            .and_modify(|spec: &mut String| {
                spec.push_str(", ");
                spec.push_str(&part);
            })
            .or_insert(part);
    }
    Ok(entries)
}

/// Where in `path` the dependency table is, if it is in one.
fn dependency_table(path: &[String]) -> Option<usize> {
    if path.first().map(String::as_str) == Some("patch") && path.len() >= 2 {
        return Some(1);
    }
    if path.first().map(String::as_str) == Some("replace") {
        return Some(0);
    }
    path.iter()
        .position(|segment| DEPENDENCY_TABLES.contains(&segment.as_str()))
}

/// The packages a `Cargo.lock` pins, by name. Each is shown with its version,
/// its source when that is not crates.io, and the start of its checksum - a
/// checksum that changes under an unchanged version is the one thing in a
/// lockfile that is never routine.
pub(super) fn lockfile(text: &str) -> Result<Entries, String> {
    let mut entries = Entries::new();
    let mut current: Option<LockedPackage> = None;

    for statement in statements(text)? {
        if let Some(inner) = table_header(&statement) {
            if let Some(package) = current.take() {
                package.record(&mut entries)?;
            }
            if statement.starts_with("[[") && inner.trim() == "package" {
                current = Some(LockedPackage::default());
            }
            continue;
        }
        let Some(package) = current.as_mut() else {
            continue;
        };
        let (key, value) = split_assignment(&statement)?;
        let value = Some(unquote(value));
        match key.trim() {
            "name" => package.name = value,
            "version" => package.version = value,
            "source" => package.source = value,
            "checksum" => package.checksum = value,
            _ => {}
        }
    }
    if let Some(package) = current.take() {
        package.record(&mut entries)?;
    }
    Ok(entries)
}

#[derive(Default)]
struct LockedPackage {
    name: Option<String>,
    version: Option<String>,
    source: Option<String>,
    checksum: Option<String>,
}

impl LockedPackage {
    fn record(self, entries: &mut Entries) -> Result<(), String> {
        let name = self
            .name
            .ok_or_else(|| "a [[package]] has no name".to_string())?;
        let mut spec = self.version.unwrap_or_default();
        if let Some(source) = self
            .source
            .filter(|source| !DEFAULT_SOURCES.contains(&source.as_str()))
        {
            spec.push_str(&format!(" ({source})"));
        }
        if let Some(checksum) = self.checksum {
            let short: String = checksum.chars().take(CHECKSUM_CHARS).collect();
            spec.push_str(&format!(" sha256:{short}"));
        }
        insert(entries, name, spec);
        Ok(())
    }
}

/// The inside of a `[table]` or `[[array.of.tables]]` header.
fn table_header(statement: &str) -> Option<&str> {
    let statement = statement.trim();
    if let Some(inner) = statement.strip_prefix("[[") {
        return inner.strip_suffix("]]");
    }
    statement.strip_prefix('[')?.strip_suffix(']')
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Basic,
    Literal,
    MultiBasic,
    MultiLiteral,
}

/// Splits TOML into statements: one per header or key/value pair, with
/// comments removed and values that span lines (arrays, multi-line strings)
/// joined into one.
fn statements(text: &str) -> Result<Vec<String>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut quote = Quote::None;
    let mut depth: i64 = 0;
    let mut line = 1;
    let mut i = 0;

    let starts = |i: usize, pattern: &str| {
        pattern
            .chars()
            .enumerate()
            .all(|(offset, c)| chars.get(i + offset) == Some(&c))
    };

    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
        }
        match quote {
            Quote::None => match c {
                '#' => {
                    while i < chars.len() && chars[i] != '\n' {
                        i += 1;
                    }
                    continue;
                }
                '"' if starts(i, "\"\"\"") => {
                    quote = Quote::MultiBasic;
                    current.push_str("\"\"\"");
                    i += 3;
                    continue;
                }
                '\'' if starts(i, "'''") => {
                    quote = Quote::MultiLiteral;
                    current.push_str("'''");
                    i += 3;
                    continue;
                }
                '"' => quote = Quote::Basic,
                '\'' => quote = Quote::Literal,
                '[' | '{' => depth += 1,
                ']' | '}' => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(format!("unbalanced `{c}` on line {line}"));
                    }
                }
                '\n' if depth == 0 => {
                    push_statement(&mut statements, &mut current);
                    i += 1;
                    continue;
                }
                _ => {}
            },
            Quote::Basic | Quote::MultiBasic if c == '\\' => {
                current.push(c);
                if let Some(next) = chars.get(i + 1) {
                    current.push(*next);
                }
                i += 2;
                continue;
            }
            Quote::Basic if c == '"' => quote = Quote::None,
            Quote::Literal if c == '\'' => quote = Quote::None,
            Quote::Basic | Quote::Literal if c == '\n' => {
                return Err(format!("unterminated string on line {}", line - 1));
            }
            Quote::MultiBasic if starts(i, "\"\"\"") => {
                quote = Quote::None;
                current.push_str("\"\"\"");
                i += 3;
                continue;
            }
            Quote::MultiLiteral if starts(i, "'''") => {
                quote = Quote::None;
                current.push_str("'''");
                i += 3;
                continue;
            }
            _ => {}
        }
        current.push(if c == '\n' { ' ' } else { c });
        i += 1;
    }

    if quote != Quote::None {
        return Err("a string is never closed".to_string());
    }
    if depth != 0 {
        return Err("a bracket is never closed".to_string());
    }
    push_statement(&mut statements, &mut current);
    Ok(statements)
}

fn push_statement(statements: &mut Vec<String>, current: &mut String) {
    let statement = current.trim();
    if !statement.is_empty() {
        statements.push(statement.to_string());
    }
    current.clear();
}

/// `key = value`, split at the first `=` outside quotes.
fn split_assignment(statement: &str) -> Result<(&str, &str), String> {
    let mut quote = None;
    for (index, c) in statement.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(open), _) if c == open => quote = None,
            (None, '=') => return Ok((&statement[..index], statement[index + 1..].trim())),
            _ => {}
        }
    }
    Err(format!(
        "`{statement}` is neither a table header nor `key = value`"
    ))
}

/// The segments of a dotted key, unquoted: `target.'cfg(unix)'.dependencies`
/// is three segments, not four.
fn split_dotted(key: &str) -> Result<Vec<String>, String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for c in key.chars() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(open), _) if c == open => quote = None,
            (None, '.') => {
                segments.push(segment(&current, key)?);
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if quote.is_some() {
        return Err(format!("unterminated quote in key `{key}`"));
    }
    segments.push(segment(&current, key)?);
    Ok(segments)
}

fn segment(raw: &str, key: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("empty segment in key `{key}`"));
    }
    // `"\u0064ependencies"` is `dependencies` to Cargo. Rather than decode
    // escapes, refuse them: an entry the parser misreads must not drop out of
    // the list, and an error turns the description into changed lines.
    if trimmed.contains('\\') {
        return Err(format!(
            "escaped characters in key `{key}` are not supported"
        ));
    }
    Ok(trimmed.to_string())
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        })
        .unwrap_or(value)
        .to_string()
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::dependency::DependencyChange;

    fn changes(before: &str, after: &str) -> Vec<String> {
        DependencyChange::between(&manifest(before).unwrap(), &manifest(after).unwrap())
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn lock_changes(before: &str, after: &str) -> Vec<String> {
        DependencyChange::between(&lockfile(before).unwrap(), &lockfile(after).unwrap())
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    const BASE: &str = r#"
[package]
name = "demo"
description = """
A demo. [dependencies] in a string is not a table.
"""

[dependencies]
serde = { version = "1", features = [
    "derive", # comments inside arrays are fine
] }
anyhow = "1"

[dev-dependencies]
tempfile = "3"
"#;

    #[test]
    fn adds_removes_and_version_changes_are_listed() {
        let after = BASE
            .replace("anyhow = \"1\"", "anyhow = \"2\"\nreqwest = \"0.12\"")
            .replace("tempfile = \"3\"\n", "");
        assert_eq!(
            changes(BASE, &after),
            [
                "~ anyhow \"1\" -> \"2\"",
                "+ reqwest \"0.12\"",
                "- tempfile (dev-dependencies) \"3\"",
            ]
        );
    }

    #[test]
    fn a_rename_through_package_shows_the_crate_actually_fetched() {
        let after = BASE.replace(
            "anyhow = \"1\"",
            "anyhow = { package = \"anyhow-evil\", version = \"1\" }",
        );
        let shown = changes(BASE, &after);
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert!(shown[0].contains("package = \"anyhow-evil\""), "{shown:?}");
    }

    #[test]
    fn a_git_dependency_shows_its_repository() {
        let after =
            format!("{BASE}\n[build-dependencies]\ncc = {{ git = \"https://example.com/cc\" }}\n");
        assert_eq!(
            changes(BASE, &after),
            ["+ cc (build-dependencies) { git = \"https://example.com/cc\" }"]
        );
    }

    #[test]
    fn a_patch_section_is_a_dependency_change() {
        let after = format!("{BASE}\n[patch.crates-io]\nserde = {{ path = \"../serde\" }}\n");
        assert_eq!(
            changes(BASE, &after),
            ["+ serde (patch.crates-io) { path = \"../serde\" }"]
        );
    }

    #[test]
    fn entries_written_as_tables_or_dotted_keys_are_put_back_together() {
        let after = format!(
            "{BASE}\n[target.'cfg(unix)'.dependencies.nix]\nversion = \"0.29\"\ngit = \"https://example.com/nix\"\n\
             [workspace]\ndependencies.libc = \"0.2\"\n"
        );
        assert_eq!(
            changes(BASE, &after),
            [
                "+ libc (workspace.dependencies) \"0.2\"",
                "+ nix (target.cfg(unix).dependencies) version = \"0.29\", git = \"https://example.com/nix\"",
            ]
        );
    }

    #[test]
    fn comments_and_unrelated_fields_change_no_entry() {
        let after = BASE.replace(
            "name = \"demo\"",
            "name = \"demo\" # renamed later\nversion = \"0.2.0\"",
        );
        assert!(changes(BASE, &after).is_empty());
    }

    #[test]
    fn a_broken_manifest_is_an_error_not_an_empty_list() {
        assert!(manifest("[dependencies]\nserde = { version = \"1\"\n").is_err());
        assert!(manifest("[dependencies]\nserde = \"1\n").is_err());
        assert!(manifest("[dependencies]\njust some words\n").is_err());
    }

    const LOCK: &str = r#"
version = 4

[[package]]
name = "serde"
version = "1.0.200"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0123456789abcdef0123"
dependencies = [
 "serde_derive",
]

[[package]]
name = "demo"
version = "0.1.0"
"#;

    #[test]
    fn a_lockfile_lists_pinned_versions_and_their_changes() {
        let after = LOCK
            .replace("1.0.200", "1.0.201")
            .replace("0123456789ab", "fedcba987654")
            + "\n[[package]]\nname = \"evil\"\nversion = \"6.6.6\"\nsource = \"git+https://example.com/evil#abc\"\n";
        assert_eq!(
            lock_changes(LOCK, &after),
            [
                "+ evil 6.6.6 (git+https://example.com/evil#abc)",
                "~ serde 1.0.200 sha256:0123456789ab -> 1.0.201 sha256:fedcba987654",
            ]
        );
    }

    #[test]
    fn a_checksum_that_changes_under_the_same_version_is_shown() {
        let after = LOCK.replace("0123456789ab", "000000000000");
        let shown = lock_changes(LOCK, &after);
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert!(shown[0].starts_with("~ serde 1.0.200 sha256:"), "{shown:?}");
    }

    #[test]
    fn two_versions_of_one_crate_are_both_kept() {
        let after = format!("{LOCK}\n[[package]]\nname = \"serde\"\nversion = \"0.9.0\"\n");
        let shown = lock_changes(LOCK, &after);
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert!(shown[0].contains("0.9.0"), "{shown:?}");
    }
}

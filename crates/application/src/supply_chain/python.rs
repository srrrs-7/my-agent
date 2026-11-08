//! `requirements*.txt`.

use super::{Entries, insert};

/// The requirements a pip requirements file names.
///
/// A requirement is filed under its normalised project name (PEP 503), its
/// spec being everything after the name - version, extras, markers, a direct
/// URL. Option lines (`--extra-index-url`, `-e`, `-r`) are entries of their
/// own, named by the whole line: an extra index is how a dependency gets
/// swapped for someone else's package of the same name.
pub(super) fn requirements(text: &str) -> Result<Entries, String> {
    let mut entries = Entries::new();
    for line in logical_lines(text) {
        let line = strip_comment(&line);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let name_end = line
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
            .unwrap_or(line.len());
        if line.starts_with('-') || name_end == 0 {
            insert(&mut entries, collapse(line), String::new());
            continue;
        }
        let name = normalize(&line[..name_end]);
        let spec = collapse(line[name_end..].trim());
        insert(&mut entries, name, spec);
    }
    Ok(entries)
}

/// Joins `\`-continued lines.
fn logical_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        match line.trim_end().strip_suffix('\\') {
            Some(start) => {
                current.push_str(start);
                current.push(' ');
            }
            None => {
                current.push_str(line);
                lines.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// pip treats `#` as a comment at the start of a line or after whitespace;
/// a `#` inside a URL fragment (`#egg=name`) is not one.
fn strip_comment(line: &str) -> &str {
    let mut previous = ' ';
    for (index, c) in line.char_indices() {
        if c == '#' && previous.is_whitespace() {
            return &line[..index];
        }
        previous = c;
    }
    line
}

fn normalize(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut separator = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            separator = true;
            continue;
        }
        if separator && !normalized.is_empty() {
            normalized.push('-');
        }
        separator = false;
        normalized.push(c.to_ascii_lowercase());
    }
    normalized
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::dependency::DependencyChange;

    fn changes(before: &str, after: &str) -> Vec<String> {
        DependencyChange::between(
            &requirements(before).unwrap(),
            &requirements(after).unwrap(),
        )
        .iter()
        .map(ToString::to_string)
        .collect()
    }

    #[test]
    fn requirements_adds_removes_and_changes_are_listed() {
        let before = "requests==2.31.0  # http\nDjango>=4.2\nflask\n";
        let after = "requests==2.32.0\ndjango>=4.2\nnumpy==1.26.4 \\\n    --hash=sha256:abc\n";
        assert_eq!(
            changes(before, after),
            [
                "- flask",
                "+ numpy ==1.26.4 --hash=sha256:abc",
                "~ requests ==2.31.0 -> ==2.32.0",
            ]
        );
    }

    #[test]
    fn an_extra_index_and_a_direct_url_are_shown() {
        let before = "requests==2.31.0\n";
        let after = "--extra-index-url https://evil.example/simple\nrequests @ https://evil.example/requests.whl\n";
        assert_eq!(
            changes(before, after),
            [
                "+ --extra-index-url https://evil.example/simple",
                "~ requests ==2.31.0 -> @ https://evil.example/requests.whl",
            ]
        );
    }

    #[test]
    fn names_are_compared_the_way_pip_compares_them() {
        assert!(changes("Foo_Bar==1\n", "foo.bar==1\n").is_empty());
    }
}

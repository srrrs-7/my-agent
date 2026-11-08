//! `go.mod`.

use super::{Entries, insert};

/// The `require`, `replace` and `exclude` directives of a `go.mod`, in both
/// their single-line and block forms.
pub(super) fn module(text: &str) -> Result<Entries, String> {
    let mut entries = Entries::new();
    let mut block: Option<String> = None;

    for (number, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(directive) = &block {
            if line == ")" {
                block = None;
            } else {
                record(directive, line, &mut entries);
            }
            continue;
        }
        let (directive, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        if rest == "(" {
            block = Some(directive.to_string());
        } else if rest.starts_with('(') {
            return Err(format!("line {}: unexpected text after `(`", number + 1));
        } else {
            record(directive, rest, &mut entries);
        }
    }
    if block.is_some() {
        return Err("a `(` block is never closed".to_string());
    }
    Ok(entries)
}

fn record(directive: &str, line: &str, entries: &mut Entries) {
    let words: Vec<&str> = line.split_whitespace().collect();
    match directive {
        "require" => {
            if let Some((module, version)) = words.split_first() {
                insert(entries, (*module).to_string(), version.join(" "));
            }
        }
        "replace" => {
            if let Some(arrow) = words.iter().position(|word| *word == "=>") {
                let from = words[..arrow].join(" ");
                let to = words[arrow + 1..].join(" ");
                insert(entries, format!("{from} (replace)"), format!("=> {to}"));
            }
        }
        "exclude" => insert(
            entries,
            format!("{} (exclude)", words.join(" ")),
            String::new(),
        ),
        _ => {}
    }
}

/// `//` starts a comment in `go.mod`. `// indirect` markers go with it; they
/// say why a requirement is there, not what is fetched.
fn strip_comment(line: &str) -> &str {
    line.split_once("//").map_or(line, |(code, _)| code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::dependency::DependencyChange;

    fn changes(before: &str, after: &str) -> Vec<String> {
        DependencyChange::between(&module(before).unwrap(), &module(after).unwrap())
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    const BASE: &str = "module example.com/demo\n\ngo 1.22\n\nrequire (\n\tgolang.org/x/text v0.14.0\n\tgithub.com/pkg/errors v0.9.1 // indirect\n)\n";

    #[test]
    fn requires_and_replaces_are_listed() {
        let after = BASE
            .replace("v0.14.0", "v0.15.0")
            .replace("\tgithub.com/pkg/errors v0.9.1 // indirect\n", "")
            + "require github.com/evil/thing v1.0.0\nreplace golang.org/x/text => github.com/evil/text v0.15.0\n";
        assert_eq!(
            changes(BASE, &after),
            [
                "+ github.com/evil/thing v1.0.0",
                "- github.com/pkg/errors v0.9.1",
                "~ golang.org/x/text v0.14.0 -> v0.15.0",
                "+ golang.org/x/text (replace) => github.com/evil/text v0.15.0",
            ]
        );
    }

    #[test]
    fn an_unclosed_block_is_an_error() {
        assert!(module("require (\n\ta v1\n").is_err());
    }
}

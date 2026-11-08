//! Finding `@path` references in a chat line.
//!
//! Only the lexing lives here. Reading the files is
//! [`agent_application::agent::attach_references`], through the workspace's
//! `FileSystem`: this module must never touch the disk itself, or `@` would be
//! a way around the access policy.
//!
//! The syntax:
//!
//! * `@` starts a reference only at the start of a word - at the start of the
//!   line, after whitespace, or after an opening bracket. `user@host` and
//!   `me@example.com` are left alone.
//! * `@path` runs to the next whitespace, with trailing punctuation
//!   (`.,;:!?)]}'"`) dropped so `see @src/main.rs.` means `src/main.rs`.
//! * `@"a b.txt"` takes everything up to the next `"`, for paths with spaces.
//!   There are no escapes; a path containing `"` cannot be referenced. An
//!   unclosed quote is not a reference.
//!
//! Repeated references are returned once, in the order first written.

use agent_application::agent::{ReferenceLimits, attach_references};
use agent_domain::text::printable;

use crate::composition::Application;

/// `line` with the files it references attached, as the turn's input.
///
/// One stderr line per reference, attached or not: a refused one stays in the
/// text as typed and the turn runs anyway, so the line is the only place the
/// user learns the model did not get the file.
pub(crate) async fn expand(app: &Application, line: &str) -> String {
    let references = extract(line);
    if references.is_empty() {
        return line.to_string();
    }
    let limits = ReferenceLimits::for_loop(
        app.settings.agent_loop.max_tool_output_bytes,
        app.settings.agent_loop.max_history_bytes,
    );
    let attachments =
        attach_references(app.files.as_ref(), &app.workspace, &references, limits).await;

    for file in &attachments.attached {
        let note = if file.truncated { ", truncated" } else { "" };
        eprintln!(
            "  attached @{} ({} bytes{note})",
            printable(&file.path.display()),
            file.bytes
        );
    }
    for refused in &attachments.refused {
        eprintln!(
            "  not attached @{}: {}",
            printable(&refused.reference),
            printable(&refused.reason)
        );
    }
    attachments.apply(line)
}

/// Punctuation that ends a sentence or a parenthetical rather than a path.
const TRAILING: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"'];

/// The paths `line` refers to with `@`, without the `@`.
pub fn extract(line: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut previous: Option<char> = None;
    let mut chars = line.char_indices().peekable();

    while let Some((index, c)) = chars.next() {
        let starts_word =
            previous.is_none_or(|p| p.is_whitespace() || matches!(p, '(' | '[' | '{'));
        previous = Some(c);
        if c != '@' || !starts_word {
            continue;
        }

        let rest = &line[index + 1..];
        let (path, consumed) = match rest.strip_prefix('"') {
            Some(quoted) => match quoted.find('"') {
                Some(end) => (&quoted[..end], end + 2),
                None => continue,
            },
            None => {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                (rest[..end].trim_end_matches(TRAILING), end)
            }
        };

        // Skip what was consumed, so `@a@b` does not yield `b` as well and a
        // quoted path's contents are not lexed again.
        let stop = index + 1 + consumed;
        while chars.peek().is_some_and(|(next, _)| *next < stop) {
            previous = chars.next().map(|(_, c)| c);
        }

        if !path.is_empty() && !found.iter().any(|seen| seen == path) {
            found.push(path.to_string());
        }
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_after_at_is_extracted() {
        assert_eq!(extract("explain @src/main.rs please"), ["src/main.rs"]);
        assert_eq!(extract("@Cargo.toml"), ["Cargo.toml"]);
    }

    #[test]
    fn an_at_inside_a_word_is_not_a_reference() {
        assert!(extract("mail foo@bar.com about it").is_empty());
        assert!(extract("ssh user@host").is_empty());
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_the_path() {
        assert_eq!(extract("look at @notes.txt."), ["notes.txt"]);
        assert_eq!(
            extract("(see @a.rs, @b.rs; @c.rs)"),
            ["a.rs", "b.rs", "c.rs"]
        );
        assert_eq!(extract("is it @lib.rs?"), ["lib.rs"]);
    }

    #[test]
    fn a_repeated_reference_is_returned_once() {
        assert_eq!(extract("@a.rs and @b.rs and @a.rs"), ["a.rs", "b.rs"]);
    }

    #[test]
    fn a_quoted_path_may_contain_spaces() {
        assert_eq!(
            extract(r#"compare @"my notes.txt" with @plain.txt"#),
            ["my notes.txt", "plain.txt"]
        );
    }

    #[test]
    fn an_unclosed_quote_or_a_bare_at_is_nothing() {
        assert!(extract(r#"@"never closed"#).is_empty());
        assert!(extract("@ alone, and @. too").is_empty());
    }
}

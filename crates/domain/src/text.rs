//! Shared-kernel text helpers.
//!
//! Clipping strings to a byte budget without splitting a UTF-8 character is
//! needed by every layer - tool output, prompt assembly, error bodies, log
//! summaries - and getting it wrong panics at runtime rather than failing to
//! compile. It lives here, in the crate everything already depends on, so there
//! is exactly one implementation to get right.
//!
//! The same goes for [`printable`]: text a model or a file supplied reaches a
//! terminal in several places, and a human decides on the strength of what it
//! shows. One definition of "safe to show" means one list to keep complete.

/// Largest index `<= max_bytes` that lies on a character boundary.
fn boundary(text: &str, max_bytes: usize) -> usize {
    if text.len() <= max_bytes {
        return text.len();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Borrowed prefix of at most `max_bytes` bytes.
pub fn truncate(text: &str, max_bytes: usize) -> &str {
    &text[..boundary(text, max_bytes)]
}

/// Owned prefix plus whether anything was cut off.
pub fn truncate_owned(text: &str, max_bytes: usize) -> (String, bool) {
    let end = boundary(text, max_bytes);
    (text[..end].to_string(), end < text.len())
}

/// Prefix with a trailing ellipsis when the text did not fit.
pub fn clip(text: &str, max_bytes: usize) -> String {
    let end = boundary(text, max_bytes);
    if end < text.len() {
        format!("{}…", &text[..end])
    } else {
        text.to_string()
    }
}

/// First non-blank line, trimmed and clipped. Used for one-line summaries.
pub fn first_line(text: &str, max_bytes: usize) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    truncate(line, max_bytes).to_string()
}

/// Whether `c` acts on a terminal, or changes how the text around it reads,
/// instead of being shown as itself.
///
/// * every control character - C0 (escape sequences, the carriage return that
///   redraws a line, the newline that pushes a question off screen, and the
///   tab, because the callers draw one line), DEL and C1 (`U+009B` is a
///   one-byte CSI on terminals that honour it);
/// * the bidirectional controls, which make `gnp.exe` display as `exe.png`;
/// * the invisible formatting characters, which make two different strings
///   look identical - `rm -rf /tmp/x` and `rm -rf /\u{200B}tmp/x` are not the
///   same command. The soft hyphen and the Arabic letter mark are among them;
/// * the tag characters (`U+E0000`-`U+E007F`), which mirror ASCII invisibly:
///   a whole sentence can ride along in text that shows as one word, and a
///   model reads it all the same;
/// * the variation selectors (`U+FE00`-`U+FE0F`, `U+E0100`-`U+E01EF`), which
///   render as nothing on their own and can carry data the same way;
/// * the Unicode line and paragraph separators, which some terminals break on.
pub fn is_unprintable(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FEFF}'
                | '\u{E0000}'..='\u{E007F}'
                | '\u{E0100}'..='\u{E01EF}'
        )
}

/// `text` with every character [`is_unprintable`] reports replaced by `?`,
/// so that it is drawn on one line and reads as what it is.
///
/// Applied where text leaves for a terminal, once, and to anything a model or
/// a file wrote - tool arguments, tool output summaries, plan items, file
/// names. Not to the model's prose on stdout: that is the answer, it may
/// legitimately span lines, and nobody approves anything on the strength of
/// it.
pub fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if is_unprintable(c) { '?' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_splits_a_character() {
        // Each of these is 3 bytes.
        assert_eq!(truncate("あいうえお", 4), "あ");
        assert_eq!(truncate("あいうえお", 3), "あ");
        assert_eq!(truncate("あいうえお", 2), "");
    }

    #[test]
    fn short_text_is_untouched() {
        assert_eq!(truncate("hi", 10), "hi");
        assert_eq!(truncate_owned("hi", 10), ("hi".to_string(), false));
        assert_eq!(clip("hi", 10), "hi");
    }

    #[test]
    fn reports_truncation() {
        let (text, truncated) = truncate_owned("abcdef", 3);
        assert_eq!(text, "abc");
        assert!(truncated);
    }

    #[test]
    fn clip_marks_the_cut() {
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(clip("日本語です", 6), "日本…");
    }

    #[test]
    fn first_line_skips_blank_leading_lines() {
        assert_eq!(first_line("\n\n  hello  \nworld", 100), "hello");
        assert_eq!(first_line("", 100), "");
        assert_eq!(first_line("abcdef", 3), "abc");
    }

    #[test]
    fn a_zero_budget_yields_nothing() {
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn nothing_that_drives_the_terminal_survives_printable() {
        assert_eq!(
            printable("evil\x1b[2J\r\nsafe\u{202E}gnp.exe"),
            "evil?[2J??safe?gnp.exe"
        );
        // C1 CSI, tab, DEL, zero-width space, BOM, isolate, line separator.
        assert_eq!(
            printable("a\u{9B}b\tc\u{7F}d\u{200B}e\u{FEFF}f\u{2066}g\u{2028}h"),
            "a?b?c?d?e?f?g?h"
        );
    }

    #[test]
    fn characters_that_hide_text_are_unprintable() {
        for (what, c) in [
            ("tag space", '\u{E0020}'),
            ("tag letter", '\u{E0041}'),
            ("cancel tag", '\u{E007F}'),
            ("language tag", '\u{E0001}'),
            ("variation selector 1", '\u{FE00}'),
            ("variation selector 16", '\u{FE0F}'),
            ("variation selector 17", '\u{E0100}'),
            ("variation selector 256", '\u{E01EF}'),
            ("line separator", '\u{2028}'),
            ("paragraph separator", '\u{2029}'),
            ("arabic letter mark", '\u{061C}'),
            ("soft hyphen", '\u{00AD}'),
        ] {
            assert!(is_unprintable(c), "{what} (U+{:04X})", c as u32);
            assert_eq!(printable(&format!("a{c}b")), "a?b", "{what}");
        }
        // The neighbours of those ranges are ordinary characters.
        for c in ['\u{FDFF}', '\u{FE10}', '\u{E0080}', '\u{E01F0}', '\u{00AC}'] {
            assert!(!is_unprintable(c), "U+{:04X}", c as u32);
        }
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        let text = "cargo test -- --nocapture 日本語 é ✓ \"quoted\"";
        assert_eq!(printable(text), text);
    }
}

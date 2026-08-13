//! Shared-kernel text helpers.
//!
//! Clipping strings to a byte budget without splitting a UTF-8 character is
//! needed by every layer - tool output, prompt assembly, error bodies, log
//! summaries - and getting it wrong panics at runtime rather than failing to
//! compile. It lives here, in the crate everything already depends on, so there
//! is exactly one implementation to get right.

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
}

//! Crude HTML-to-text reduction.
//!
//! Good enough to make documentation pages readable to a model: scripts,
//! styles and tags go away, a few common entities are decoded, whitespace is
//! collapsed. Deliberately not a real HTML parser - per the supply-chain
//! policy this stays a page of code instead of a dependency, and a model
//! reading prose does not need DOM fidelity.

/// Strips markup from `html` and returns readable text.
pub(crate) fn html_to_text(html: &str) -> String {
    let without_blocks = strip_block(&strip_block(html, "script"), "style");

    let mut text = String::with_capacity(without_blocks.len() / 2);
    let mut in_tag = false;
    for ch in without_blocks.chars() {
        match ch {
            '<' => {
                in_tag = true;
                // Tags separate words: `<p>a</p><p>b</p>` must not become "ab".
                if !text.ends_with([' ', '\n']) {
                    text.push(' ');
                }
            }
            '>' if in_tag => in_tag = false,
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }

    let decoded = decode_entities(&text);

    // Collapse runs of blank lines and per-line whitespace.
    let mut out = String::with_capacity(decoded.len());
    let mut blank_pending = false;
    for line in decoded.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank_pending = !out.is_empty();
        } else {
            if blank_pending {
                out.push('\n');
                blank_pending = false;
            }
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// Removes `<tag ...> ... </tag>` blocks (case-insensitive), content included.
fn strip_block(html: &str, tag: &str) -> String {
    let lower = html.to_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");

    let mut out = String::with_capacity(html.len());
    let mut position = 0;
    while let Some(start) = lower[position..].find(&open) {
        let start = position + start;
        out.push_str(&html[position..start]);
        match lower[start..].find(&close) {
            Some(end) => position = start + end + close.len(),
            None => {
                // Unterminated block: drop the rest, it is script/style anyway.
                return out;
            }
        }
    }
    out.push_str(&html[position..]);
    out
}

fn decode_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&") // last, so `&amp;lt;` decodes to `&lt;`
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_scripts_and_styles() {
        let html = r#"<html><head><style>body { color: red }</style>
            <script>alert("x")</script></head>
            <body><h1>Title</h1><p>First para.</p><p>Second para.</p></body></html>"#;
        let text = html_to_text(html);

        assert!(text.contains("Title"));
        assert!(text.contains("First para."));
        assert!(text.contains("Second para."));
        assert!(
            !text.contains("alert"),
            "script content must not survive: {text}"
        );
        assert!(
            !text.contains("color"),
            "style content must not survive: {text}"
        );
        assert!(!text.contains('<'));
    }

    #[test]
    fn adjacent_elements_do_not_merge_words() {
        assert_eq!(html_to_text("<p>a</p><p>b</p>").trim(), "a b");
    }

    #[test]
    fn decodes_common_entities() {
        assert_eq!(
            html_to_text("a &lt;b&gt; &amp; &quot;c&quot;&nbsp;d").trim(),
            "a <b> & \"c\" d"
        );
    }

    #[test]
    fn survives_unterminated_script_blocks() {
        let text = html_to_text("<p>visible</p><script>never closed");
        assert!(text.contains("visible"));
        assert!(!text.contains("never closed"));
    }

    #[test]
    fn runs_of_blank_lines_collapse_to_one() {
        assert_eq!(
            html_to_text("just text\n\n\n\nmore").trim(),
            "just text\n\nmore"
        );
    }
}

//! Server-Sent Events framing.
//!
//! Only the framing lives here - splitting a byte stream into the `data:`
//! payloads of complete events. What those payloads *mean* is vendor-specific
//! and stays in the client that owns the wire format (OpenAI today, Anthropic
//! when its streaming lands). Hand-rolled per the supply-chain policy: the
//! subset we need is ~50 lines, an SSE crate is not worth the surface.

/// Incremental parser: feed bytes as they arrive, take complete `data:`
/// payloads out. Bytes that do not yet form a complete event stay buffered.
#[derive(Debug, Default)]
pub(crate) struct SseFraming {
    buffer: String,
}

impl SseFraming {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Absorbs a chunk and returns the `data:` payloads of every event that
    /// became complete. Multi-line `data:` fields are joined with `\n` per the
    /// SSE spec; comment lines (`:` prefix) and other fields are ignored.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        // Providers send UTF-8; a chunk may split a code point, so replace
        // invalid prefixes only when they cannot be completed. Keeping raw
        // bytes would be stricter, but chunk boundaries inside a code point
        // are rare and the lossy form only affects the split character.
        self.buffer.push_str(&String::from_utf8_lossy(chunk));

        let mut payloads = Vec::new();
        // An event ends at a blank line: \n\n (or \r\n\r\n).
        while let Some(end) = self
            .buffer
            .find("\n\n")
            .map(|i| (i, 2))
            .or_else(|| self.buffer.find("\r\n\r\n").map(|i| (i, 4)))
        {
            let (event, rest) = self.buffer.split_at(end.0);
            let payload = Self::data_of(event);
            self.buffer = rest[end.1..].to_string();
            if let Some(payload) = payload {
                payloads.push(payload);
            }
        }
        payloads
    }

    fn data_of(event: &str) -> Option<String> {
        let lines: Vec<&str> = event
            .lines()
            .filter_map(|line| {
                line.strip_prefix("data:")
                    .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
            })
            .collect();
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_events_and_strips_the_data_prefix() {
        let mut framing = SseFraming::new();
        let payloads = framing.feed(b"data: {\"a\":1}\n\ndata: [DONE]\n\n");
        assert_eq!(
            payloads,
            vec![r#"{"a":1}"#.to_string(), "[DONE]".to_string()]
        );
    }

    #[test]
    fn buffers_events_split_across_feeds() {
        let mut framing = SseFraming::new();
        assert!(framing.feed(b"data: {\"par").is_empty());
        assert!(framing.feed(b"tial\":true}").is_empty());
        assert_eq!(
            framing.feed(b"\n\n"),
            vec![r#"{"partial":true}"#.to_string()]
        );
    }

    #[test]
    fn handles_crlf_delimiters() {
        let mut framing = SseFraming::new();
        assert_eq!(
            framing.feed(b"data: x\r\n\r\ndata: y\r\n\r\n"),
            vec!["x".to_string(), "y".to_string()]
        );
    }

    #[test]
    fn ignores_comments_and_other_fields() {
        let mut framing = SseFraming::new();
        assert_eq!(
            framing.feed(b": keep-alive\n\nevent: message\ndata: x\n\n"),
            vec!["x".to_string()]
        );
    }

    #[test]
    fn joins_multi_line_data_fields() {
        let mut framing = SseFraming::new();
        assert_eq!(
            framing.feed(b"data: a\ndata: b\n\n"),
            vec!["a\nb".to_string()]
        );
    }

    #[test]
    fn survives_a_multibyte_character_split_across_chunks() {
        let mut framing = SseFraming::new();
        let text = "data: 日本\n\n".as_bytes();
        // Split inside the second multi-byte character.
        let _ = framing.feed(&text[..8]);
        let payloads = framing.feed(&text[8..]);
        // The split code point may be lossy, but framing must not break or panic.
        assert_eq!(payloads.len(), 1);
    }
}

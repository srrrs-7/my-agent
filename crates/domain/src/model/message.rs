//! Conversation messages.
//!
//! The representation is deliberately *block based* (like Anthropic's Messages
//! API) rather than flat strings: it is the richer of the two mainstream
//! shapes, so mapping down to OpenAI's `tool_calls` / `role: "tool"` form is
//! lossless, while the reverse would not be.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::image::Image;
use super::tool::{ToolCall, ToolResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    /// Carries [`ContentBlock::ToolResult`] blocks back to the model.
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    /// An image the user attached to their message. Only user messages carry
    /// one: tool results are text (an image read by the model itself is not
    /// supported yet).
    ///
    /// Not text: [`Message::text`] skips it. Where only text can go - a
    /// terminal, a compaction transcript, a provider without vision - it
    /// appears as its [`Display`](std::fmt::Display) form, `[image: 12 KB png]`.
    Image(Image),
    /// Something a provider put in an assistant turn that this program does
    /// not interpret but must hand back, byte for byte, on later requests of
    /// the same conversation - Anthropic's `thinking` and `redacted_thinking`
    /// blocks, whose signature the API checks and whose loss breaks both the
    /// model's reasoning across tool calls and the prompt cache.
    ///
    /// `dialect` names the adapter that produced it. Only that adapter sends
    /// it back; every other one leaves it out, the way OpenAI-compatible
    /// servers would reject a block type they never emitted. `data` is the
    /// block exactly as it arrived.
    ///
    /// It is not text: [`Message::text`] skips it, and nothing shows it to
    /// the user or summarises it.
    Opaque {
        dialect: String,
        data: Value,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn approx_bytes(&self) -> usize {
        match self {
            Self::Text { text } => text.len(),
            Self::ToolCall(call) => call.name.as_str().len() + call.arguments.to_string().len(),
            Self::ToolResult(result) => result.content.len(),
            // Nominal, not the encoded length: see `model::image`.
            Self::Image(_) => Image::NOMINAL_BYTES,
            Self::Opaque { data, .. } => data.to_string().len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,

    /// Where this message fell in the session, counted in messages.
    ///
    /// Assigned by [`Conversation`](crate::model::conversation::Conversation)
    /// as the message is appended, never reused, and never restarted when a
    /// compaction shortens the vector. It is what the retention policy weighs
    /// a message by - see
    /// [`Conversation::distance_from_newest`](crate::model::conversation::Conversation::distance_from_newest).
    ///
    /// `Option` because a message exists before it joins a conversation, and
    /// because a history persisted before this field existed has none. Absent
    /// means "ask the vector instead", never "discard".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

impl Message {
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Self {
            role,
            content,
            seq: None,
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::new(Role::User, vec![ContentBlock::text(text)])
    }

    /// A user message of `text` followed by `images`. With no images it is
    /// exactly [`Self::user`].
    pub fn user_with_images(text: impl Into<String>, images: Vec<Image>) -> Self {
        let mut content = vec![ContentBlock::text(text)];
        content.extend(images.into_iter().map(ContentBlock::Image));
        Self::new(Role::User, content)
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self::new(Role::Assistant, vec![ContentBlock::text(text)])
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self::new(Role::Assistant, content)
    }

    pub fn tool_results(results: Vec<ToolResult>) -> Self {
        Self::new(
            Role::Tool,
            results.into_iter().map(ContentBlock::ToolResult).collect(),
        )
    }

    /// Stamps the message with its position in the session.
    ///
    /// Only [`Conversation`](crate::model::conversation::Conversation) should
    /// call this: it owns the counter, and a number handed out by anyone else
    /// would not be comparable with the rest.
    #[must_use]
    pub fn with_seq(mut self, seq: u64) -> Self {
        self.seq = Some(seq);
        self
    }

    /// All text blocks concatenated - what a human would read.
    pub fn text(&self) -> String {
        let parts: Vec<&str> = self
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        parts.join("\n")
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call),
            _ => None,
        })
    }

    pub fn has_tool_calls(&self) -> bool {
        self.tool_calls().next().is_some()
    }

    /// Cheap size proxy used by the context window policy. Not a token count -
    /// tokenisation belongs to a provider, the loop only needs a stable
    /// monotonic measure to decide what to drop first.
    pub fn approx_bytes(&self) -> usize {
        self.content
            .iter()
            .map(ContentBlock::approx_bytes)
            .sum::<usize>()
            + 16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tool::{ToolCallId, ToolName};
    use serde_json::json;

    #[test]
    fn text_concatenates_only_text_blocks() {
        let name = ToolName::new("read_file").unwrap();
        let message = Message::assistant(vec![
            ContentBlock::text("hello"),
            ContentBlock::ToolCall(ToolCall::new(ToolCallId::new("1"), name, json!({}))),
            ContentBlock::text("world"),
        ]);
        assert_eq!(message.text(), "hello\nworld");
        assert!(message.has_tool_calls());
    }

    /// The session log stores messages as JSON, so an opaque block has to
    /// come back from it exactly as it went in - a resumed conversation sends
    /// it to the provider again.
    #[test]
    fn an_opaque_block_round_trips_through_json_unchanged_and_is_not_text() {
        let data = json!({"type": "thinking", "thinking": "", "signature": "EqQBCkYIARgCKkA="});
        let message = Message::assistant(vec![
            ContentBlock::Opaque {
                dialect: "anthropic".into(),
                data: data.clone(),
            },
            ContentBlock::text("done"),
        ]);

        let restored: Message =
            serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();

        assert_eq!(restored, message);
        assert_eq!(restored.text(), "done");
        assert!(message.approx_bytes() > data.to_string().len());
    }

    #[test]
    fn an_image_is_not_text_and_weighs_a_fixed_nominal_amount() {
        let mut big = crate::model::image::tests::TINY_PNG.to_vec();
        big.resize(2 * 1024 * 1024, 0);
        let message = Message::user_with_images("look", vec![Image::from_bytes(big).unwrap()]);

        assert_eq!(message.text(), "look");
        assert_eq!(
            message.approx_bytes(),
            "look".len() + Image::NOMINAL_BYTES + 16,
            "a 2 MiB image must not count as 2.7 MB of base64 against the history budget"
        );
    }

    /// The session log keeps the image itself, so a resumed conversation
    /// sends the model the same picture.
    #[test]
    fn an_image_round_trips_through_the_session_log_format() {
        let image = Image::from_bytes(crate::model::image::tests::TINY_PNG.to_vec()).unwrap();
        let message = Message::user_with_images("look", vec![image.clone()]).with_seq(3);

        let line = serde_json::to_string(&message).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["content"][1]["type"], "image");
        assert_eq!(value["content"][1]["media_type"], "image/png");
        assert_eq!(value["content"][1]["data"], image.to_base64());

        let restored: Message = serde_json::from_str(&line).unwrap();
        assert_eq!(restored, message);
    }

    /// A history written before sequence numbers existed must still load, and
    /// an unstamped message must not serialise a null field into one that was
    /// written after.
    #[test]
    fn the_sequence_number_is_optional_in_both_directions() {
        let without: Message =
            serde_json::from_str(r#"{"role":"user","content":[{"type":"text","text":"hi"}]}"#)
                .expect("a history without sequence numbers must still load");
        assert_eq!(without.seq, None);
        assert_eq!(without.text(), "hi");

        let json = serde_json::to_string(&without).unwrap();
        assert!(!json.contains("seq"), "{json}");

        let with = without.with_seq(7);
        assert_eq!(with.seq, Some(7));
        assert!(serde_json::to_string(&with).unwrap().contains(r#""seq":7"#));
    }
}

//! Helpers shared by tool implementations.
//!
//! Text clipping lives in [`agent_domain::text`]; this module only deals with
//! turning what a *model* sent into what a tool expects.

use agent_domain::error::ToolError;
use agent_domain::model::tool::ToolName;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Deserialises model-supplied arguments, turning a schema mismatch into a
/// message the model can act on rather than an opaque serde error.
///
/// Real models get this wrong in predictable ways, so the shapes below are
/// normalised before deserialising instead of being rejected.
pub fn parse_arguments<T: DeserializeOwned>(
    tool: &ToolName,
    arguments: Value,
) -> Result<T, ToolError> {
    let arguments = match arguments {
        // `null` or `""` for a tool that takes no arguments.
        Value::Null => Value::Object(Default::default()),
        Value::String(text) if text.trim().is_empty() => Value::Object(Default::default()),
        // A JSON *string* containing the real object.
        Value::String(text) => serde_json::from_str(&text).unwrap_or(Value::String(text)),
        other => other,
    };

    serde_json::from_value(arguments).map_err(|error| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: format!("{error}. Check the tool's JSON schema and try again."),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Input {
        path: String,
        #[serde(default)]
        limit: Option<usize>,
    }

    fn tool() -> ToolName {
        ToolName::new("read_file").unwrap()
    }

    #[test]
    fn parses_plain_objects() {
        let parsed: Input = parse_arguments(&tool(), json!({"path": "a.rs"})).unwrap();
        assert_eq!(
            parsed,
            Input {
                path: "a.rs".into(),
                limit: None
            }
        );
    }

    #[test]
    fn parses_arguments_wrapped_in_a_json_string() {
        let parsed: Input = parse_arguments(&tool(), json!(r#"{"path":"a.rs"}"#)).unwrap();
        assert_eq!(parsed.path, "a.rs");
    }

    #[test]
    fn reports_schema_mismatches_usefully() {
        let error = parse_arguments::<Input>(&tool(), json!({"wrong": 1})).unwrap_err();
        assert!(error.to_string().contains("path"));
    }

    #[test]
    fn ignores_extra_fields() {
        let parsed: Input =
            parse_arguments(&tool(), json!({"path": "a.rs", "unexpected": true})).unwrap();
        assert_eq!(parsed.path, "a.rs");
    }
}

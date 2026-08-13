use std::sync::Arc;

use agent_domain::error::ToolError;
use agent_domain::model::tool::{ToolDefinition, ToolName, ToolOutcome, ToolSafety};
use agent_domain::model::workspace::WorkspaceRoot;
use agent_domain::ports::file_system::FileSystem;
use agent_domain::ports::tool::Tool;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::util::parse_arguments;

/// Replaces an exact excerpt inside an existing file.
///
/// Literal string matching, not fuzzy and not regex. That is a deliberate
/// trade: a match either is what the model saw in `read_file` or the edit is
/// refused, so an edit can never silently land in the wrong place. Ambiguity
/// (several matches) is likewise an error rather than a guess.
pub struct EditFileTool {
    file_system: Arc<dyn FileSystem>,
    root: Arc<WorkspaceRoot>,
}

#[derive(Debug, Deserialize)]
struct Input {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

impl EditFileTool {
    pub fn new(file_system: Arc<dyn FileSystem>, root: Arc<WorkspaceRoot>) -> Self {
        Self { file_system, root }
    }

    fn name() -> ToolName {
        ToolName::new("edit_file").expect("static tool name is valid")
    }
}

#[async_trait]
impl Tool for EditFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::name(),
            description: "Replace an exact excerpt in an existing file.\n\
                          `old_string` must match the file byte for byte and must be unique \
                          unless `replace_all` is true, so include enough surrounding context. \
                          Copy it from `read_file` output without the line-number prefix."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path relative to the workspace root."
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Exact text to replace, including indentation."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "Replacement text."
                    },
                    "replace_all": {
                        "type": "boolean",
                        "default": false,
                        "description": "Replace every occurrence instead of requiring a unique match."
                    }
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
            safety: ToolSafety::Mutating,
        }
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        let name = Self::name();
        let input: Input = parse_arguments(&name, arguments)?;

        if input.old_string.is_empty() {
            return Err(ToolError::invalid_input(
                &name,
                "`old_string` must not be empty. Use `write_file` to create a new file.",
            ));
        }
        if input.old_string == input.new_string {
            return Err(ToolError::invalid_input(
                &name,
                "`old_string` and `new_string` are identical, so this edit would do nothing.",
            ));
        }

        let path = self
            .root
            .resolve(&input.path)
            .map_err(|error| ToolError::invalid_input(&name, error.to_string()))?;

        let content = self
            .file_system
            .read_to_string(&path)
            .await
            .map_err(|error| ToolError::execution(&name, error.to_string()))?;

        let occurrences = content.matches(&input.old_string).count();
        match occurrences {
            0 => {
                return Err(ToolError::execution(
                    &name,
                    format!(
                        "`old_string` was not found in `{path}`. Read the file again and copy the \
                         excerpt exactly - whitespace and indentation must match."
                    ),
                ));
            }
            n if n > 1 && !input.replace_all => {
                return Err(ToolError::execution(
                    &name,
                    format!(
                        "`old_string` matches {n} times in `{path}`. Add surrounding lines until \
                         it is unique, or set `replace_all` to true."
                    ),
                ));
            }
            _ => {}
        }

        let updated = if input.replace_all {
            content.replace(&input.old_string, &input.new_string)
        } else {
            content.replacen(&input.old_string, &input.new_string, 1)
        };

        self.file_system
            .write(&path, &updated)
            .await
            .map_err(|error| ToolError::execution(&name, error.to_string()))?;

        let replaced = if input.replace_all { occurrences } else { 1 };
        let line = line_of(&content, &input.old_string);
        let location = line
            .map(|line| format!(" at line {line}"))
            .unwrap_or_default();

        Ok(ToolOutcome::new(format!(
            "Replaced {replaced} occurrence(s) in `{path}`{location}."
        ))
        .with_summary(format!("edited {path}{location}")))
    }
}

/// 1-based line number of the first occurrence, for a friendlier summary.
fn line_of(content: &str, needle: &str) -> Option<usize> {
    let index = content.find(needle)?;
    Some(content[..index].matches('\n').count() + 1)
}

#[cfg(test)]
mod tests {
    use super::line_of;

    #[test]
    fn reports_the_first_matching_line() {
        let content = "one\ntwo\nthree\n";
        assert_eq!(line_of(content, "one"), Some(1));
        assert_eq!(line_of(content, "three"), Some(3));
        assert_eq!(line_of(content, "four"), None);
    }
}

use std::sync::Arc;

use agent_domain::error::ToolError;
use agent_domain::model::tool::{ToolDefinition, ToolName, ToolOutcome, ToolSafety};
use agent_domain::model::workspace::WorkspaceRoot;
use agent_domain::ports::file_system::FileSystem;
use agent_domain::ports::tool::Tool;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use agent_domain::text;

use crate::tools::util::parse_arguments;

/// Lines returned when the model does not ask for a window.
const DEFAULT_LIMIT: usize = 1_500;
/// Very long lines (minified assets, embedded blobs) are clipped per line.
const MAX_LINE_BYTES: usize = 2_000;

/// Reads a text file with line numbers.
///
/// The numbers matter: they give the model a stable way to talk about
/// locations, and paging through a large file with `offset` costs one small
/// message instead of one enormous one.
pub struct ReadFileTool {
    file_system: Arc<dyn FileSystem>,
    root: Arc<WorkspaceRoot>,
}

#[derive(Debug, Deserialize)]
struct Input {
    path: String,
    /// 1-based line to start from.
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

impl ReadFileTool {
    pub fn new(file_system: Arc<dyn FileSystem>, root: Arc<WorkspaceRoot>) -> Self {
        Self { file_system, root }
    }

    fn name() -> ToolName {
        ToolName::new("read_file").expect("static tool name is valid")
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::name(),
            description: "Read a text file from the workspace, returned with line numbers.\n\
                          Use `offset` and `limit` to page through large files. Always read a \
                          file before editing it."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path relative to the workspace root, e.g. `src/main.rs`."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "1-based line number to start reading from."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of lines to return."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            safety: ToolSafety::ReadOnly,
        }
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        let name = Self::name();
        let input: Input = parse_arguments(&name, arguments)?;

        let path = self
            .root
            .resolve(&input.path)
            .map_err(|error| ToolError::invalid_input(&name, error.to_string()))?;

        let content = self
            .file_system
            .read_to_string(&path)
            .await
            .map_err(|error| ToolError::execution(&name, error.to_string()))?;

        if content.is_empty() {
            return Ok(ToolOutcome::new(format!("`{path}` exists but is empty."))
                .with_summary(format!("{path} (empty)")));
        }

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let start = input.offset.unwrap_or(1).max(1);
        if start > total {
            return Err(ToolError::execution(
                &name,
                format!("offset {start} is past the end of `{path}`, which has {total} lines"),
            ));
        }
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT).max(1);
        let end = (start - 1 + limit).min(total);

        let mut rendered = String::with_capacity(content.len().min(64 * 1024));
        for (offset, line) in lines[start - 1..end].iter().enumerate() {
            let (line, clipped) = text::truncate_owned(line, MAX_LINE_BYTES);
            rendered.push_str(&format!("{:>6}\t{line}", start + offset));
            if clipped {
                rendered.push_str(" … [line truncated]");
            }
            rendered.push('\n');
        }

        let shown = end - start + 1;
        if shown < total {
            rendered.push_str(&format!(
                "\n[showing lines {start}-{end} of {total}; call again with offset={} for more]\n",
                end + 1
            ));
        }

        Ok(ToolOutcome::new(rendered).with_summary(format!("{path} ({shown}/{total} lines)")))
    }
}

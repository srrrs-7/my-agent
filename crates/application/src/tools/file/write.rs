use std::sync::Arc;

use agent_domain::error::ToolError;
use agent_domain::model::tool::{PlannedWrite, ToolDefinition, ToolName, ToolOutcome, ToolSafety};
use agent_domain::model::workspace::{WorkspacePath, WorkspaceRoot};
use agent_domain::ports::file_system::FileSystem;
use agent_domain::ports::tool::Tool;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::file::human_bytes;
use crate::tools::util::{ToolErrorContext, parse_arguments};

/// Creates a file or replaces its entire contents.
pub struct WriteFileTool {
    file_system: Arc<dyn FileSystem>,
    root: Arc<WorkspaceRoot>,
}

#[derive(Debug, Deserialize)]
struct Input {
    path: String,
    content: String,
}

impl WriteFileTool {
    pub fn new(file_system: Arc<dyn FileSystem>, root: Arc<WorkspaceRoot>) -> Self {
        Self { file_system, root }
    }

    fn name() -> ToolName {
        ToolName::new("write_file").expect("static tool name is valid")
    }

    /// The validated destination and contents. Shared by `preview` and
    /// `execute` so that what is approved is what gets written.
    fn target(&self, arguments: Value) -> Result<(WorkspacePath, String), ToolError> {
        let name = Self::name();
        let input: Input = parse_arguments(&name, arguments)?;

        let path = self.root.resolve(&input.path).for_tool(&name)?;

        if path.is_root() {
            return Err(ToolError::invalid_input(
                &name,
                "`path` must name a file, not the root",
            ));
        }
        Ok((path, input.content))
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::name(),
            description: "Write a text file, creating parent directories as needed.\n\
                          This replaces the whole file. To change part of an existing file use \
                          `edit_file` instead, and read the file first."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path relative to the workspace root."
                    },
                    "content": {
                        "type": "string",
                        "description": "Full contents of the file."
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
            safety: ToolSafety::Mutating,
        }
    }

    async fn preview(&self, arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
        let (path, content) = self.target(arguments.clone())?;
        // Missing and unreadable look the same here, and both count as
        // "different from what will be written" - never as "unchanged".
        let before = self.file_system.read_to_string(&path).await.ok();
        Ok(vec![PlannedWrite {
            path,
            before,
            after: content,
        }])
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        let name = Self::name();
        let (path, content) = self.target(arguments)?;

        let existed = self.file_system.exists(&path).await.for_tool(&name)?;

        self.file_system
            .write(&path, &content)
            .await
            .for_tool(&name)?;

        let action = if existed { "Updated" } else { "Created" };
        let size = human_bytes(content.len() as u64);
        let lines = content.lines().count();

        Ok(
            ToolOutcome::new(format!("{action} `{path}` ({size}, {lines} lines)."))
                .with_summary(format!("{} {path} ({size})", action.to_lowercase())),
        )
    }
}

use std::fmt::Write as _;
use std::sync::Arc;

use agent_domain::error::ToolError;
use agent_domain::model::tool::{ToolDefinition, ToolName, ToolOutcome, ToolSafety};
use agent_domain::model::workspace::WorkspaceRoot;
use agent_domain::ports::file_system::{EntryKind, FileSystem};
use agent_domain::ports::tool::Tool;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::file::human_bytes;
use crate::tools::util::parse_arguments;

/// Lists one directory (non-recursive).
pub struct ListDirectoryTool {
    file_system: Arc<dyn FileSystem>,
    root: Arc<WorkspaceRoot>,
}

#[derive(Debug, Deserialize)]
struct Input {
    #[serde(default)]
    path: Option<String>,
}

impl ListDirectoryTool {
    pub fn new(file_system: Arc<dyn FileSystem>, root: Arc<WorkspaceRoot>) -> Self {
        Self { file_system, root }
    }

    fn name() -> ToolName {
        ToolName::new("list_directory").expect("static tool name is valid")
    }
}

#[async_trait]
impl Tool for ListDirectoryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::name(),
            description: "List the entries of one directory in the workspace (not recursive).\n\
                          Omit `path` to list the workspace root."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory relative to the workspace root. Defaults to the root."
                    }
                },
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
            .resolve(input.path.as_deref().unwrap_or(""))
            .map_err(|error| ToolError::invalid_input(&name, error.to_string()))?;

        let entries = self
            .file_system
            .list_dir(&path)
            .await
            .map_err(|error| ToolError::execution(&name, error.to_string()))?;

        if entries.is_empty() {
            return Ok(ToolOutcome::new(format!("`{path}` is empty."))
                .with_summary(format!("{path} (empty)")));
        }

        let mut rendered = format!("{path}\n");
        for entry in &entries {
            let leaf = entry
                .path
                .display()
                .rsplit('/')
                .next()
                .unwrap_or_else(|| entry.path.as_path().to_str().unwrap_or("?"))
                .to_string();
            match entry.kind {
                EntryKind::Directory => {
                    let _ = writeln!(rendered, "  {leaf}/");
                }
                EntryKind::Symlink => {
                    let _ = writeln!(rendered, "  {leaf} -> (symlink)");
                }
                EntryKind::File => {
                    let _ = writeln!(rendered, "  {leaf}  ({})", human_bytes(entry.size_bytes));
                }
            }
        }

        let directories = entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Directory)
            .count();
        let files = entries.len() - directories;

        Ok(ToolOutcome::new(rendered)
            .with_summary(format!("{path}: {directories} dirs, {files} files")))
    }
}

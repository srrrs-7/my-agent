use std::fmt::Write as _;
use std::sync::Arc;

use agent_domain::error::ToolError;
use agent_domain::model::tool::{ToolDefinition, ToolName, ToolOutcome, ToolSafety};
use agent_domain::model::workspace::WorkspaceRoot;
use agent_domain::ports::search::{FileSearcher, SearchQuery};
use agent_domain::ports::tool::Tool;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use agent_domain::text;

use crate::tools::util::{ToolErrorContext, parse_arguments};

const DEFAULT_MAX_RESULTS: usize = 100;
const HARD_MAX_RESULTS: usize = 500;
const MAX_LINE_BYTES: usize = 300;

/// Regex search across the workspace, honouring `.gitignore`.
pub struct SearchFilesTool {
    searcher: Arc<dyn FileSearcher>,
    root: Arc<WorkspaceRoot>,
}

#[derive(Debug, Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    include: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    max_results: Option<usize>,
}

impl SearchFilesTool {
    pub fn new(searcher: Arc<dyn FileSearcher>, root: Arc<WorkspaceRoot>) -> Self {
        Self { searcher, root }
    }

    fn name() -> ToolName {
        ToolName::new("search_files").expect("static tool name is valid")
    }
}

#[async_trait]
impl Tool for SearchFilesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::name(),
            description: "Search file contents with a regular expression.\n\
                          Ignored files (`.gitignore`) and binaries are skipped. Use this to \
                          locate symbols before reading whole files."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Rust-flavoured regular expression matched per line."
                    },
                    "path": {
                        "type": "string",
                        "description": "Subtree to search, relative to the workspace root. Defaults to the root."
                    },
                    "include": {
                        "type": "string",
                        "description": "Glob restricting which files are searched, e.g. `**/*.rs`."
                    },
                    "case_insensitive": {
                        "type": "boolean",
                        "default": false
                    },
                    "max_results": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of matching lines to return."
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            safety: ToolSafety::ReadOnly,
        }
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        let name = Self::name();
        let input: Input = parse_arguments(&name, arguments)?;

        if input.pattern.trim().is_empty() {
            return Err(ToolError::invalid_input(
                &name,
                "`pattern` must not be empty",
            ));
        }

        let root = self
            .root
            .resolve(input.path.as_deref().unwrap_or(""))
            .for_tool(&name)?;

        let max_results = input
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, HARD_MAX_RESULTS);

        let hits = self
            .searcher
            .search(SearchQuery {
                pattern: input.pattern.clone(),
                root: root.clone(),
                include_glob: input.include.clone(),
                case_insensitive: input.case_insensitive,
                max_results,
            })
            .await
            .for_tool(&name)?;

        if hits.is_empty() {
            return Ok(ToolOutcome::new(format!(
                "No match for `{}` under `{root}`.",
                input.pattern
            ))
            .with_summary("no matches".to_string()));
        }

        let mut rendered = String::with_capacity(hits.len() * 96);
        for hit in &hits {
            let (line, clipped) = text::truncate_owned(hit.line.trim_end(), MAX_LINE_BYTES);
            let ellipsis = if clipped { " …" } else { "" };
            let _ = writeln!(
                rendered,
                "{}:{}: {line}{ellipsis}",
                hit.path, hit.line_number
            );
        }

        if hits.len() >= max_results {
            let _ = writeln!(
                rendered,
                "\n[stopped at {max_results} matches; narrow `pattern`, `path` or `include` to see the rest]"
            );
        }

        let files = hits
            .iter()
            .map(|hit| hit.path.display())
            .collect::<std::collections::BTreeSet<_>>()
            .len();

        Ok(ToolOutcome::new(rendered)
            .with_summary(format!("{} matches in {files} files", hits.len())))
    }
}

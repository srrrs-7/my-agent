use std::fmt::Write as _;
use std::sync::Arc;

use agent_domain::error::ToolError;
use agent_domain::model::glob::GlobPattern;
use agent_domain::model::tool::{PlannedWrite, ToolDefinition, ToolName, ToolOutcome, ToolSafety};
use agent_domain::model::workspace::WorkspaceRoot;
use agent_domain::ports::search::{FileSearcher, GlobQuery};
use agent_domain::ports::tool::Tool;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::util::{ToolErrorContext, parse_arguments};

const DEFAULT_MAX_RESULTS: usize = 100;
const HARD_MAX_RESULTS: usize = 1000;

/// Finds files by path pattern, honouring `.gitignore` and the access policy.
///
/// The counterpart of `search_files`: that one answers "where is this text",
/// this one "which files are there". Without it the model walks the tree one
/// `list_directory` call at a time.
pub struct GlobTool {
    searcher: Arc<dyn FileSearcher>,
    root: Arc<WorkspaceRoot>,
}

#[derive(Debug, Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
}

impl GlobTool {
    pub fn new(searcher: Arc<dyn FileSearcher>, root: Arc<WorkspaceRoot>) -> Self {
        Self { searcher, root }
    }

    fn name() -> ToolName {
        ToolName::new("glob").expect("static tool name is valid")
    }
}

#[async_trait]
impl Tool for GlobTool {
    async fn preview(&self, _arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
        // Lists paths only; nothing is written.
        Ok(Vec::new())
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::name(),
            description:
                "Find files by path pattern (e.g. `src/**/*.rs`), newest-modified first.\n\
                          Matches paths, not contents - use `search_files` for contents. \
                          `.gitignore` syntax: a pattern without `/` (`*.toml`) matches at any \
                          depth; one with `/` is relative to `path`. Ignored files are skipped, \
                          and hidden ones unless the pattern names them (`.github/**/*.yml`)."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob relative to `path`, e.g. `**/*.rs` or `docs/*.md`. No leading `/` and no `..`."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search in, relative to the workspace root. Defaults to the root."
                    },
                    "max_results": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of paths to return."
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

        let root = self
            .root
            .resolve(input.path.as_deref().unwrap_or(""))
            .for_tool(&name)?;
        let pattern = GlobPattern::new(&input.pattern).for_tool(&name)?;
        let max_results = input
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, HARD_MAX_RESULTS);

        let found = self
            .searcher
            .glob(GlobQuery {
                pattern,
                root: root.clone(),
                max_results,
            })
            .await
            .for_tool(&name)?;

        if found.paths.is_empty() {
            return Ok(ToolOutcome::new(format!(
                "No file matches `{}` under `{root}`. Ignored and protected files are never \
                 listed; a pattern without `/` already matches at any depth.",
                input.pattern
            ))
            .with_summary("no matches".to_string()));
        }

        let mut rendered = String::with_capacity(found.paths.len() * 48);
        for path in &found.paths {
            let _ = writeln!(rendered, "{path}");
        }
        if found.truncated() {
            let _ = writeln!(
                rendered,
                "\n[showing the {shown} most recently modified of {total} matches; narrow \
                 `pattern` or `path` to see the rest]",
                shown = found.paths.len(),
                total = found.total
            );
        }

        let summary = if found.truncated() {
            format!("{} of {} files", found.paths.len(), found.total)
        } else {
            format!("{} files", found.paths.len())
        };
        Ok(ToolOutcome::new(rendered).with_summary(summary))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::error::FsError;
    use agent_domain::model::workspace::WorkspacePath;
    use agent_domain::ports::search::{GlobMatches, SearchHit, SearchQuery};
    use std::sync::Mutex;

    /// Answers every glob with the same matches and remembers the query.
    struct FixedGlob {
        matches: GlobMatches,
        seen: Mutex<Option<GlobQuery>>,
    }

    #[async_trait]
    impl FileSearcher for FixedGlob {
        async fn search(&self, _: SearchQuery) -> Result<Vec<SearchHit>, FsError> {
            unreachable!("glob never searches contents")
        }

        async fn glob(&self, query: GlobQuery) -> Result<GlobMatches, FsError> {
            *self.seen.lock().unwrap() = Some(query);
            Ok(self.matches.clone())
        }
    }

    fn tool(paths: &[&str], total: usize) -> (GlobTool, Arc<FixedGlob>) {
        let searcher = Arc::new(FixedGlob {
            matches: GlobMatches {
                paths: paths
                    .iter()
                    .map(|path| WorkspacePath::parse(path).unwrap())
                    .collect(),
                total,
            },
            seen: Mutex::new(None),
        });
        let root = Arc::new(WorkspaceRoot::new("/workspace").unwrap());
        (GlobTool::new(searcher.clone(), root), searcher)
    }

    fn run(tool: &GlobTool, arguments: Value) -> Result<ToolOutcome, ToolError> {
        futures::executor::block_on(tool.execute(arguments))
    }

    #[test]
    fn lists_one_path_per_line() {
        let (tool, searcher) = tool(&["src/b.rs", "src/a.rs"], 2);
        let outcome = run(&tool, json!({"pattern": "**/*.rs", "path": "src"})).unwrap();

        assert_eq!(outcome.content, "src/b.rs\nsrc/a.rs\n");
        let query = searcher.seen.lock().unwrap().clone().unwrap();
        assert_eq!(query.root.display(), "src");
        assert_eq!(query.max_results, DEFAULT_MAX_RESULTS);
    }

    #[test]
    fn a_truncated_list_says_how_many_were_left_out_and_what_to_do() {
        let (tool, _) = tool(&["a.rs", "b.rs"], 40);
        let outcome = run(&tool, json!({"pattern": "*.rs", "max_results": 2})).unwrap();

        assert!(
            outcome
                .content
                .contains("showing the 2 most recently modified of 40 matches"),
            "{}",
            outcome.content
        );
        assert!(outcome.content.contains("narrow `pattern` or `path`"));
    }

    #[test]
    fn no_match_is_an_answer_not_an_error() {
        let (tool, _) = tool(&[], 0);
        let outcome = run(&tool, json!({"pattern": "*.zig"})).unwrap();
        assert!(outcome.content.starts_with("No file matches `*.zig`"));
    }

    #[test]
    fn a_search_root_outside_the_workspace_is_refused() {
        let (tool, searcher) = tool(&[], 0);
        let error = run(&tool, json!({"pattern": "*", "path": "../elsewhere"})).unwrap_err();

        assert!(matches!(error, ToolError::InvalidInput { .. }), "{error}");
        assert!(searcher.seen.lock().unwrap().is_none());
    }

    #[test]
    fn a_pattern_reaching_outside_the_subtree_is_refused_before_searching() {
        let (tool, searcher) = tool(&[], 0);
        let error = run(&tool, json!({"pattern": "../*.rs"})).unwrap_err();

        assert!(matches!(error, ToolError::InvalidInput { .. }), "{error}");
        assert!(
            error.to_string().contains("`path`"),
            "says what to pass instead: {error}"
        );
        assert!(searcher.seen.lock().unwrap().is_none());
    }
}

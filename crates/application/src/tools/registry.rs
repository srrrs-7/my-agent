//! The set of tools advertised to the model.

use std::collections::BTreeMap;
use std::sync::Arc;

use agent_domain::model::tool::{ToolDefinition, ToolName};
use agent_domain::ports::tool::Tool;

/// Ordered so the tool list in the system prompt is stable between runs -
/// unstable prompts defeat provider-side prompt caching.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<ToolName, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.definition().name, tool);
    }

    /// Builder form, for the composition root.
    #[must_use]
    pub fn with(mut self, tool: Arc<dyn Tool>) -> Self {
        self.register(tool);
        self
    }

    /// Replaces every tool with `wrap(definition, tool)`, for stacking a
    /// decorator over the whole set in one step. The decorator is expected to
    /// keep the tool's name; the result is keyed by the name it reports, as
    /// [`register`](Self::register) would key it.
    #[must_use]
    pub fn map(self, wrap: impl Fn(&ToolDefinition, Arc<dyn Tool>) -> Arc<dyn Tool>) -> Self {
        let mut mapped = Self::new();
        for tool in self.tools.into_values() {
            let definition = tool.definition();
            mapped.register(wrap(&definition, tool));
        }
        mapped
    }

    /// The tools that only observe ([`ToolSafety::ReadOnly`]), as they are -
    /// decorators and all. What a sub-agent is given: it shares the parent's
    /// approval gate, but a gate can be told to approve everything, and a
    /// tool that is not in the registry cannot be approved at all.
    ///
    /// [`ToolSafety::ReadOnly`]: agent_domain::model::tool::ToolSafety::ReadOnly
    #[must_use]
    pub fn read_only(&self) -> Self {
        Self {
            tools: self
                .tools
                .iter()
                .filter(|(_, tool)| tool.definition().safety.is_read_only())
                .map(|(name, tool)| (name.clone(), tool.clone()))
                .collect(),
        }
    }

    pub fn get(&self, name: &ToolName) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().map(ToolName::to_string).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::plan::TodoWriteTool;
    use agent_domain::model::tool::ToolSafety;

    #[test]
    fn map_hands_each_tool_its_definition_and_keeps_what_it_returns() {
        let original: Arc<dyn Tool> = Arc::new(TodoWriteTool::new());
        let replacement: Arc<dyn Tool> = Arc::new(TodoWriteTool::new());
        let name = original.definition().name;
        let registry = ToolRegistry::new().with(original.clone());

        let mapped = registry.map(|definition, tool| {
            assert_eq!(definition.name, name);
            assert!(
                Arc::ptr_eq(&tool, &original),
                "the current tool is passed in"
            );
            replacement.clone()
        });

        assert_eq!(mapped.names(), [name.to_string()]);
        let kept = mapped.get(&name).expect("the name is kept");
        assert!(
            Arc::ptr_eq(&kept, &replacement),
            "the returned tool replaces it"
        );
    }

    #[test]
    fn read_only_keeps_exactly_the_tools_that_only_observe() {
        let registry = ToolRegistry::new()
            .with(Arc::new(TodoWriteTool::new()))
            .with(Arc::new(Labelled("writer", ToolSafety::Mutating)))
            .with(Arc::new(Labelled("fetcher", ToolSafety::Network)))
            .with(Arc::new(Labelled("shell", ToolSafety::Destructive)))
            .with(Arc::new(Labelled("reader", ToolSafety::ReadOnly)));

        let mut names = registry.read_only().names();
        names.sort();
        assert_eq!(names, ["reader", "todo_write"]);
        assert_eq!(registry.len(), 5, "the original is left as it was");
    }

    /// A tool that is only a name and a safety class.
    struct Labelled(&'static str, ToolSafety);

    #[async_trait::async_trait]
    impl Tool for Labelled {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: ToolName::new(self.0).unwrap(),
                description: String::new(),
                input_schema: serde_json::json!({"type": "object"}),
                safety: self.1,
            }
        }

        async fn preview(
            &self,
            _arguments: &serde_json::Value,
        ) -> Result<Vec<agent_domain::model::tool::PlannedWrite>, agent_domain::error::ToolError>
        {
            Ok(Vec::new())
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
        ) -> Result<agent_domain::model::tool::ToolOutcome, agent_domain::error::ToolError>
        {
            Ok(agent_domain::model::tool::ToolOutcome::new(""))
        }
    }
}

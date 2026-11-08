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
}

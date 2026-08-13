//! `agent tools` - what the model is offered.

use agent_domain::model::tool::ToolDefinition;
use serde_json::Value;

use crate::composition::Application;

pub fn execute(app: &Application) {
    println!("{} tools available to the model:\n", app.tools.len());
    for definition in app.tools.definitions() {
        print_tool(&definition);
    }
}

fn print_tool(definition: &ToolDefinition) {
    println!("  {} [{}]", definition.name, definition.safety.label());
    for line in definition.description.lines() {
        println!("      {line}");
    }
    if let Some(arguments) = render_arguments(&definition.input_schema) {
        println!("      arguments: {arguments}");
    }
    println!();
}

/// `path, [offset], [limit]` - optional arguments in brackets.
fn render_arguments(schema: &Value) -> Option<String> {
    let properties = schema.get("properties")?.as_object()?;
    if properties.is_empty() {
        return None;
    }

    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    Some(
        properties
            .keys()
            .map(|key| {
                if required.contains(&key.as_str()) {
                    key.clone()
                } else {
                    format!("[{key}]")
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn marks_optional_arguments() {
        let schema = json!({
            "type": "object",
            "properties": {"path": {}, "limit": {}},
            "required": ["path"]
        });
        assert_eq!(render_arguments(&schema).as_deref(), Some("[limit], path"));
    }

    #[test]
    fn handles_schemas_without_arguments() {
        assert_eq!(render_arguments(&json!({"type": "object"})), None);
        assert_eq!(
            render_arguments(&json!({"type": "object", "properties": {}})),
            None
        );
    }

    #[test]
    fn treats_a_missing_required_list_as_all_optional() {
        let schema = json!({"type": "object", "properties": {"a": {}}});
        assert_eq!(render_arguments(&schema).as_deref(), Some("[a]"));
    }
}

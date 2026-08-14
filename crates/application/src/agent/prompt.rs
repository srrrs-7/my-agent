//! System-prompt assembly.
//!
//! Context engineering happens here: the model is told where it is, what it may
//! touch, what the workspace looks like, and how to behave - once, up front -
//! so that individual turns can stay short.
//!
//! Three [`PromptBuilder`]s live here:
//!
//! * [`DefaultPromptBuilder`] - the built-in policy below.
//! * [`FixedPromptBuilder`] - an operator-supplied prompt used verbatim.
//! * [`AppendingPromptBuilder`] - a decorator adding operator instructions to
//!   whatever the wrapped builder produced.
//!
//! Replacing the prompt does not loosen the sandbox: path confinement is
//! enforced by `WorkspaceRoot` / `LocalFileSystem`, never by prompt text.

use std::fmt::Write as _;
use std::sync::Arc;

use agent_domain::model::context::ContextSnapshot;
use agent_domain::model::tool::ToolDefinition;
use agent_domain::ports::prompt::PromptBuilder;
use agent_domain::text;

/// Cap on the project instruction file so a huge `AGENTS.md` cannot crowd out
/// the conversation.
const MAX_PROJECT_INSTRUCTIONS_BYTES: usize = 8 * 1024;

/// The built-in prompt policy: environment, workspace overview, tool list,
/// working rules, project instructions.
///
/// This is the [`PromptBuilder`] the composition root wires by default;
/// operator-supplied prompts (see the backlog) become sibling implementations
/// or decorators rather than edits to this one.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultPromptBuilder;

impl PromptBuilder for DefaultPromptBuilder {
    fn build(&self, context: &ContextSnapshot, tools: &[ToolDefinition]) -> String {
        build_system_prompt(context, tools)
    }
}

/// An operator-supplied system prompt, used verbatim.
///
/// Context and tool list are deliberately ignored: whoever replaces the prompt
/// owns all of its content. Tool *calling* still works, because tool
/// definitions travel in the request separately from the prompt.
#[derive(Debug, Clone)]
pub struct FixedPromptBuilder {
    prompt: String,
}

impl FixedPromptBuilder {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
        }
    }
}

impl PromptBuilder for FixedPromptBuilder {
    fn build(&self, _context: &ContextSnapshot, _tools: &[ToolDefinition]) -> String {
        self.prompt.clone()
    }
}

/// Appends operator instructions to whatever the wrapped builder produced.
///
/// The extra text lands at the very end - after the project instruction file -
/// because later text is what a model treats as most binding when
/// instructions conflict, and an explicit operator override should win.
pub struct AppendingPromptBuilder {
    inner: Arc<dyn PromptBuilder>,
    extra: String,
}

impl AppendingPromptBuilder {
    pub fn new(inner: Arc<dyn PromptBuilder>, extra: impl Into<String>) -> Self {
        Self {
            inner,
            extra: extra.into(),
        }
    }
}

impl PromptBuilder for AppendingPromptBuilder {
    fn build(&self, context: &ContextSnapshot, tools: &[ToolDefinition]) -> String {
        let mut prompt = self.inner.build(context, tools);
        if !prompt.is_empty() && !prompt.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push('\n');
        prompt.push_str(&self.extra);
        prompt.push('\n');
        prompt
    }
}

pub fn build_system_prompt(context: &ContextSnapshot, tools: &[ToolDefinition]) -> String {
    let mut prompt = String::with_capacity(2048);

    prompt.push_str(
        "You are `agent`, a command-line software engineering assistant.\n\
         You work inside a sandboxed workspace and change it only through the tools listed below.\n",
    );

    prompt.push_str("\n# Environment\n");
    let _ = writeln!(prompt, "- Workspace root: {}", context.workspace_root);
    let _ = writeln!(prompt, "- Platform: {}", context.os);
    let _ = writeln!(prompt, "- Today: {}", context.today);
    let _ = writeln!(
        prompt,
        "- Git repository: {}",
        if context.is_git_repository {
            "yes"
        } else {
            "no"
        }
    );

    if !context.directory_overview.is_empty() {
        prompt.push_str("\n# Workspace overview\n");
        prompt.push_str(
            "A shallow listing, provided so you do not have to spend a turn discovering it.\n\
             It is not exhaustive - use the tools to look further.\n\n```\n",
        );
        for entry in &context.directory_overview {
            let _ = writeln!(prompt, "{entry}");
        }
        prompt.push_str("```\n");
    }

    if !tools.is_empty() {
        prompt.push_str("\n# Tools\n");
        for tool in tools {
            let headline = tool.description.lines().next().unwrap_or_default();
            let _ = writeln!(
                prompt,
                "- `{}` ({}): {}",
                tool.name,
                tool.safety.label(),
                headline
            );
        }
    }

    prompt.push_str(
        "\n# How to work\n\
         1. Understand before you act. Read the files you are about to change; never guess at\n   \
            their contents or invent APIs.\n\
         2. Paths are relative to the workspace root. Anything outside of it is inaccessible,\n   \
            and asking for it will fail - do not try to work around the sandbox.\n\
         3. `edit_file` matches an exact, literal excerpt. Include enough surrounding lines to\n   \
            make it unique, and copy the text verbatim from `read_file` output (without the\n   \
            line-number prefix).\n\
         4. Prefer the smallest change that fully satisfies the request. Do not reformat,\n   \
            rename or \"improve\" code you were not asked about.\n\
         5. Batch independent read-only lookups into a single turn; they run concurrently.\n\
         6. When a tool returns an error, read it carefully and fix the cause. If the same call\n   \
            fails twice, stop and explain the problem instead of retrying.\n\
         7. If a request is ambiguous in a way that changes the outcome, ask before writing.\n\
         8. When you are done, answer the user directly and concisely. Do not narrate the tool\n   \
            calls you made unless you are asked to.\n\
         9. Reply in the same language the user writes in.\n",
    );

    if let Some(instructions) = &context.project_instructions {
        let trimmed = instructions.trim();
        if !trimmed.is_empty() {
            prompt.push_str(
                "\n# Project instructions\n\
                 The following comes from the project's own instruction file. It takes precedence\n\
                 over the general guidance above whenever the two disagree.\n\n",
            );
            prompt.push_str(text::truncate(trimmed, MAX_PROJECT_INSTRUCTIONS_BYTES));
            prompt.push('\n');
        }
    }

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::tool::{ToolName, ToolSafety};
    use serde_json::json;

    fn snapshot() -> ContextSnapshot {
        ContextSnapshot {
            workspace_root: "/workspace".into(),
            os: "linux".into(),
            today: "2026-08-13".into(),
            is_git_repository: true,
            project_instructions: Some("Always run `make check` before finishing.".into()),
            directory_overview: vec!["crates/".into(), "Makefile".into()],
        }
    }

    #[test]
    fn includes_environment_tools_and_project_instructions() {
        let tools = vec![ToolDefinition {
            name: ToolName::new("read_file").unwrap(),
            description: "Read a file.\nMore detail.".into(),
            input_schema: json!({}),
            safety: ToolSafety::ReadOnly,
        }];
        let prompt = build_system_prompt(&snapshot(), &tools);

        assert!(prompt.contains("/workspace"));
        assert!(prompt.contains("2026-08-13"));
        assert!(prompt.contains("`read_file` (read-only): Read a file."));
        assert!(
            !prompt.contains("More detail."),
            "only the headline goes into the prompt"
        );
        assert!(prompt.contains("make check"));
    }

    #[test]
    fn omits_empty_sections() {
        let mut context = snapshot();
        context.project_instructions = None;
        context.directory_overview.clear();
        let prompt = build_system_prompt(&context, &[]);
        assert!(!prompt.contains("# Project instructions"));
        assert!(!prompt.contains("# Workspace overview"));
        assert!(!prompt.contains("# Tools"));
    }

    #[test]
    fn oversized_project_instructions_are_clipped() {
        let mut context = snapshot();
        context.project_instructions = Some("あ".repeat(MAX_PROJECT_INSTRUCTIONS_BYTES));
        // Would panic if the clip landed mid-character.
        let prompt = build_system_prompt(&context, &[]);
        assert!(prompt.contains("# Project instructions"));
        assert!(prompt.len() < MAX_PROJECT_INSTRUCTIONS_BYTES * 3);
    }

    #[test]
    fn a_fixed_prompt_is_verbatim_and_ignores_context_and_tools() {
        let tools = vec![ToolDefinition {
            name: agent_domain::model::tool::ToolName::new("read_file").unwrap(),
            description: "Read a file.".into(),
            input_schema: serde_json::json!({}),
            safety: agent_domain::model::tool::ToolSafety::ReadOnly,
        }];
        let prompt = FixedPromptBuilder::new("You are a terse bot.").build(&snapshot(), &tools);
        assert_eq!(prompt, "You are a terse bot.");
    }

    #[test]
    fn appended_instructions_land_after_the_project_instructions() {
        let builder =
            AppendingPromptBuilder::new(Arc::new(DefaultPromptBuilder), "Reply in Japanese.");
        let prompt = builder.build(&snapshot(), &[]);

        assert!(
            prompt.ends_with("Reply in Japanese.\n"),
            "appended text is last"
        );
        let instructions = prompt
            .find("make check")
            .expect("project instructions present");
        let appended = prompt.rfind("Reply in Japanese.").unwrap();
        assert!(
            appended > instructions,
            "operator instructions come after the project instruction file"
        );
    }

    #[test]
    fn appending_separates_with_a_blank_line() {
        let builder =
            AppendingPromptBuilder::new(Arc::new(FixedPromptBuilder::new("base")), "extra");
        assert_eq!(builder.build(&snapshot(), &[]), "base\n\nextra\n");
    }
}

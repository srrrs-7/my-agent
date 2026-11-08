//! `/model`, `/compact` and `/context` in `chat`: what the session runs on and
//! what it carries.
//!
//! Everything goes to stderr, like every other meta command: stdout is the
//! model's answer. A model reference was typed by the user and is echoed
//! back, so it goes through [`printable`] first.

use std::process::ExitCode;

use agent_application::agent::{ContextReport, Interrupt, ManualCompaction, Session};
use agent_domain::model::llm::ModelId;
use agent_domain::text::printable;
use agent_infrastructure::config::RouterKind;

use crate::composition::Application;
use crate::ctrl_c::{CtrlC, Sigint, TurnEnd, drive};
use crate::render::describe_usage;

/// What follows `/model`: nothing to show the current model, one reference to
/// switch to it, or why it was not understood.
pub(crate) fn model_argument(argument: &str) -> Result<Option<ModelId>, String> {
    let argument = argument.trim();
    if argument.is_empty() {
        return Ok(None);
    }
    if argument.split_whitespace().nth(1).is_some() {
        return Err(format!(
            "`/model` takes one model reference, like `claude-sonnet-5-5` or \
             `cloud/claude-sonnet-5-5` (got `{}`)",
            printable(argument)
        ));
    }
    Ok(Some(ModelId::new(argument)))
}

/// `/model` and `/model <ref>`.
///
/// Switching changes nothing but the session: the loop, its tools and the
/// system prompt stay as they are, and the next request simply names another
/// model.
pub(crate) fn model(app: &Application, session: &mut Session, wanted: Option<ModelId>) {
    let current = current_model(app, session);
    let Some(wanted) = wanted else {
        let how = if session.model_override().is_some() {
            "chosen with /model"
        } else {
            "configured"
        };
        eprintln!("  model: {} ({how})", printable(&current));
        return;
    };
    if wanted.as_str() == current {
        eprintln!("  already on {}", printable(&current));
        return;
    }

    if let Some(note) = unrouted_prefix(app, &wanted) {
        eprintln!("  note: {note}");
    }
    let dropped = session.switch_model(wanted.clone());
    eprintln!(
        "  model: {} (was {}) - the provider's prompt cache starts over on the next request",
        printable(wanted.as_str()),
        printable(&current)
    );
    if dropped > 0 {
        eprintln!(
            "  {dropped} thinking block(s) of the previous model were left out of the history"
        );
    }
}

/// The model the session's next request goes to, as a name to show.
fn current_model(app: &Application, session: &Session) -> String {
    match app.agent.model(session) {
        Some(model) => model.as_str().to_string(),
        // No `--model` and no `/model`: the configured provider's own.
        None => app
            .settings
            .default_provider_settings()
            .model
            .as_str()
            .to_string(),
    }
}

/// Why a `<prefix>/<model>` reference will not pick the provider it seems to
/// name, if it will not.
///
/// The router is what decides, at request time; this only says in advance
/// what it is going to do, so a typo in an alias is seen before a request
/// goes to the wrong place. Not an error: a vendor id may contain a slash
/// (`meta-llama/Llama-3.1-8B`), and then this is exactly what is wanted.
fn unrouted_prefix(app: &Application, model: &ModelId) -> Option<String> {
    let (prefix, rest) = model.split_provider_hint()?;
    let llm = &app.settings.llm;
    let known = llm.providers.iter().any(|p| p.id.as_str() == prefix);
    if known && !rest.is_empty() && llm.router != RouterKind::Static {
        return None;
    }
    let why = if llm.router == RouterKind::Static {
        "only one provider is configured, so no prefix selects one".to_string()
    } else {
        format!(
            "`{}` is not a configured provider (configured: {})",
            printable(prefix),
            llm.providers
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Some(format!(
        "{why}; `{}` goes to `{}` as the model name",
        printable(model.as_str()),
        llm.default_provider
    ))
}

/// `/compact`. Ctrl-C stops the summary like it stops a turn; `Some` when it
/// was pressed twice and the process should exit with that code.
pub(crate) async fn compact(
    app: &Application,
    session: &mut Session,
    sigint: &mut Sigint,
    keys: &mut CtrlC,
) -> Option<ExitCode> {
    let interrupt = Interrupt::new();
    let outcome = match drive(
        app.agent.compact_now(session, &interrupt),
        &interrupt,
        sigint,
        keys,
    )
    .await
    {
        TurnEnd::Finished(outcome) => outcome,
        TurnEnd::Abandoned(code) => return Some(code),
    };
    match outcome {
        ManualCompaction::Compacted(report) => eprintln!(
            "  compacted {} message(s): history {} -> {} bytes; the summary cost {}",
            report.folded_messages,
            report.before_bytes,
            report.after_bytes,
            describe_usage(&report.usage)
        ),
        ManualCompaction::NothingToFold => eprintln!(
            "  nothing to compact: the whole history is within the part kept word for word"
        ),
        ManualCompaction::Failed => eprintln!(
            "  the summary could not be produced; the history is unchanged (-v shows why)"
        ),
        ManualCompaction::Interrupted => {
            eprintln!("  compaction interrupted; the history is unchanged");
        }
    }
    None
}

/// `/context`.
pub(crate) fn context(app: &Application, session: &Session) {
    let report = app.agent.context_report(session);
    eprint!("{}", describe(&report, &current_model(app, session)));
}

fn describe(report: &ContextReport, model: &str) -> String {
    let mut out = format!("  {:<12} {}\n", "model", printable(model));
    let system = match report.system_bytes {
        Some(bytes) => format!("{bytes} bytes"),
        None => "not built yet - it is rendered on the first turn".to_string(),
    };
    out.push_str(&format!("  {:<12} {system}\n", "system"));
    out.push_str(&format!(
        "  {:<12} {} bytes in {} definition(s)\n",
        "tools", report.tools_bytes, report.tool_count
    ));
    out.push_str(&format!(
        "  {:<12} {} bytes in {} message(s), budget {} bytes\n",
        "history", report.history_bytes, report.messages, report.history_budget
    ));
    let plan = match report.plan_note {
        Some(note) if note.in_history => format!("{} bytes, in the history", note.bytes),
        Some(note) => format!("{} bytes, held back from the budget", note.bytes),
        None => "none (no unfinished plan)".to_string(),
    };
    out.push_str(&format!("  {:<12} {plan}\n", "plan note"));
    let last = match &report.last_usage {
        Some(usage) => describe_usage(usage),
        None => "none yet".to_string(),
    };
    out.push_str(&format!("  {:<12} {last}\n", "last request"));
    out.push_str(&format!("  {:<12} {}\n", "compactions", report.compactions));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_takes_at_most_one_reference() {
        assert_eq!(model_argument(""), Ok(None));
        assert_eq!(
            model_argument(" cloud/claude-sonnet-5-5 "),
            Ok(Some(ModelId::new("cloud/claude-sonnet-5-5")))
        );
        assert!(model_argument("two words").is_err());
    }

    #[test]
    fn a_report_before_the_first_turn_says_the_system_prompt_is_not_built() {
        let report = ContextReport {
            model: None,
            system_bytes: None,
            tool_count: 2,
            tools_bytes: 300,
            history_bytes: 0,
            messages: 0,
            history_budget: 1024,
            plan_note: None,
            last_usage: None,
            compactions: 0,
        };
        let text = describe(&report, "mock");
        assert!(text.contains("not built yet"), "{text}");
        assert!(text.contains("300 bytes in 2 definition(s)"), "{text}");
        assert!(text.contains("budget 1024 bytes"), "{text}");
        assert!(text.contains("none yet"), "{text}");
    }
}

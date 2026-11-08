//! `/undo` and `/rewind` in `chat`: run them and say what happened.
//!
//! Everything goes to stderr, like every other meta command: stdout is the
//! model's answer. File names were chosen by the model, so they go through
//! [`printable`] before they reach the terminal.

use agent_application::agent::Session;
use agent_application::checkpoint::RestoreReport;
use agent_domain::model::workspace::WorkspacePath;
use agent_domain::text::printable;

use crate::composition::Application;

pub(crate) async fn undo(app: &Application, session: &mut Session) {
    match app.agent.undo(session).await {
        Ok(report) => {
            eprint!("{}", describe(&report));
            if report.changed_files() {
                eprintln!(
                    "  the conversation is unchanged; the model is told on your next message"
                );
            }
        }
        Err(error) => eprintln!("  {error}"),
    }
}

pub(crate) async fn rewind(app: &Application, session: &mut Session, turns: usize) {
    match app.agent.rewind(session, turns).await {
        Ok(report) => {
            eprint!("{}", describe(&report.files));
            eprintln!(
                "  conversation rewound by {turns} turn(s): {} message(s) removed",
                report.dropped_messages
            );
            if let Some(error) = report.log_error {
                eprintln!(
                    "  warning: the rewind could not be written to the session log ({}); \
                     resuming this session would bring those messages back",
                    printable(&error)
                );
            }
        }
        Err(error) => eprintln!("  {error}"),
    }
}

/// One line per file, then the calls that were not taken back.
fn describe(report: &RestoreReport) -> String {
    if report.is_empty() {
        return "  no file was changed in that turn\n".to_string();
    }
    let mut out = String::new();
    let mut each = |label: &str, paths: &[WorkspacePath], detail: &str| {
        for path in paths {
            out.push_str(&format!(
                "  {label:<9} {}{detail}\n",
                printable(&path.display())
            ));
        }
    };
    each("restored", &report.restored, "");
    each("deleted", &report.removed, " (created by the agent)");
    each(
        "conflict",
        &report.conflicts,
        " - changed since the agent wrote it; left as it is",
    );
    for (path, why) in &report.unknown {
        out.push_str(&format!(
            "  {:<9} {} - its earlier content was never read ({}); left as it is\n",
            "kept",
            printable(&path.display()),
            printable(why)
        ));
    }
    for (path, error) in &report.failed {
        out.push_str(&format!(
            "  {:<9} {} - {}\n",
            "failed",
            printable(&path.display()),
            printable(error)
        ));
    }
    if !report.not_undone.is_empty() {
        let names: Vec<String> = report
            .not_undone
            .iter()
            .map(|name| printable(name.as_str()))
            .collect();
        out.push_str(&format!(
            "  not undone: {} - their effects are not recorded; check the workspace yourself\n",
            names.join(", ")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::tool::ToolName;

    fn path(raw: &str) -> WorkspacePath {
        WorkspacePath::parse(raw).unwrap()
    }

    #[test]
    fn every_outcome_is_named_and_control_characters_are_neutralised() {
        let report = RestoreReport {
            restored: vec![path("a.txt")],
            removed: vec![path("new\u{1b}[2J.txt")],
            conflicts: vec![path("b.txt")],
            not_undone: vec![ToolName::new("run_command").unwrap()],
            ..RestoreReport::default()
        };
        let text = describe(&report);

        assert!(text.contains("restored  a.txt"), "{text}");
        assert!(text.contains("deleted   new?[2J.txt"), "{text}");
        assert!(text.contains("conflict  b.txt"), "{text}");
        assert!(text.contains("not undone: run_command"), "{text}");
        assert!(!text.contains('\u{1b}'), "{text:?}");
    }
}

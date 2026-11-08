//! The smallest embedding: configure from the environment, ask once, print
//! the answer and every event as a JSON line.
//!
//! ```bash
//! AGENT_PROVIDER=ollama AGENT_MODEL=qwen3:8b \
//!   cargo run -p agent-sdk --example ask -- "What does this project do?"
//! ```
//!
//! Needs a reachable model, so CI only builds it.

use std::sync::Arc;

use agent_sdk::events::{EventDto, Versioned};
use agent_sdk::{Agent, AgentEvent, Config, EventSink, approval};

/// Prints each event on stderr in the published schema.
struct JsonLines;

impl EventSink for JsonLines {
    fn emit(&self, event: AgentEvent) {
        if let Ok(line) = serde_json::to_string(&Versioned::new(EventDto::from(&event))) {
            eprintln!("{line}");
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), agent_sdk::Error> {
    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");

    let agent = Agent::builder(Config::from_env()?)
        .events(Arc::new(JsonLines))
        // Read-only calls are approved by the default AGENT_APPROVAL policy;
        // everything else is refused, and the model is told why.
        .approval(approval::from_fn(|request| {
            approval::ApprovalDecision::Deny {
                reason: format!("this example does not allow `{}`", request.call.name),
            }
        }))
        .build()
        .await?;

    let outcome = agent.run(prompt).await?;
    println!("{}", outcome.final_text);
    Ok(())
}

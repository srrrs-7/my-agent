//! `agent doctor` - resolved configuration plus a live probe.
//!
//! The probe matters more than the configuration dump: almost every first-run
//! failure is a wrong `AGENT_BASE_URL` or a missing key, and both surface here
//! as one line instead of as a confusing failure mid-conversation.

use std::io::Write;
use std::time::Instant;

use agent_domain::model::llm::{ChatRequest, GenerationParams};
use agent_domain::model::message::Message;
use anyhow::{Result, anyhow};

use crate::composition::Application;

pub async fn execute(app: &Application) -> Result<()> {
    print_configuration(app);

    print!("\nconnectivity ... ");
    let _ = std::io::stdout().flush();

    let started = Instant::now();
    match app.provider.chat(probe()).await {
        Ok(response) => {
            println!(
                "ok ({} via {} in {:.2}s)",
                response.model,
                response.provider,
                started.elapsed().as_secs_f64()
            );
            Ok(())
        }
        Err(error) => {
            println!("FAILED");
            Err(anyhow!(error).context("the LLM endpoint did not answer"))
        }
    }
}

fn print_configuration(app: &Application) {
    println!("configuration");
    for (key, value) in agent_infrastructure::config::describe(&app.settings) {
        println!("  {key:<20} {value}");
    }
    println!("  {:<20} {}", "tools", app.tools.names().join(", "));
    println!(
        "  {:<20} tools={}",
        "capabilities",
        app.provider.capabilities().supports_tools
    );
    print_sandbox(app);
}

/// What the sandbox *is*, not what was configured.
///
/// These two can differ, and the difference is the whole point of printing it:
/// an operator who set a requirement and got something else needs to see that
/// here rather than infer it from a command that failed strangely later.
fn print_sandbox(app: &Application) {
    let Some(runner) = &app.commands else {
        println!("  {:<20} run_command is not registered", "sandbox");
        return;
    };

    use agent_domain::ports::command::CommandRunner as _;
    println!("  {:<20} {}", "sandbox", runner.sandbox().describe());

    let egress = match runner.egress_port() {
        Some(port) => format!(
            "{} (via the proxy on 127.0.0.1:{port})",
            app.settings.shell.allowed_domains.join(", ")
        ),
        None => "none - every outbound connection is refused".to_string(),
    };
    println!("  {:<20} {egress}", "command egress");
}

/// Smallest request that still proves the endpoint, the key and the model name
/// are all usable.
fn probe() -> ChatRequest {
    ChatRequest::new(vec![Message::user("Reply with the single word: ok")]).with_params(
        GenerationParams {
            temperature: Some(0.0),
            max_tokens: Some(16),
            top_p: None,
            stop_sequences: Vec::new(),
        },
    )
}

//! Terminal input.

use std::io::Write;

use anyhow::{Context, Result};

/// Reads one line from stdin, writing the prompt to **stderr** so stdout stays
/// a clean channel for the model's output.
///
/// Returns `Ok(None)` on EOF (Ctrl-D). Runs on the blocking pool because
/// `std::io::stdin` is blocking and the approval gate may be called from inside
/// the agent loop.
pub async fn prompt_line(prompt: &str) -> Result<Option<String>> {
    let prompt = prompt.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let mut stderr = std::io::stderr();
        write!(stderr, "{prompt}")?;
        stderr.flush()?;

        let mut buffer = String::new();
        let read = std::io::stdin().read_line(&mut buffer)?;
        Ok(if read == 0 { None } else { Some(buffer) })
    })
    .await
    .context("the input task panicked")?
}

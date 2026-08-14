//! `agent sessions` - what the log has kept, and getting rid of it.

use anyhow::{Context, Result};

use crate::composition::Application;

pub async fn list(app: &Application) -> Result<()> {
    let sessions = app
        .sessions
        .list()
        .await
        .context("cannot read the session directory")?;

    if sessions.is_empty() {
        println!("no sessions in {}", app.settings.session_dir().display());
        if !app.settings.session.log {
            println!("(AGENT_SESSION_LOG is off, so nothing new is being recorded)");
        }
        return Ok(());
    }

    println!("{:<38} {:>8}  STARTED WITH", "SESSION", "SIZE");
    for session in &sessions {
        println!(
            "{:<38} {:>8}  {}",
            session.id,
            human_bytes(session.bytes),
            session.preview
        );
    }
    println!("\nresume the newest with: agent chat --resume");

    Ok(())
}

/// Sizes as something to compare at a glance, since the reason to read this
/// column is deciding what to delete.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("G", 1 << 30), ("M", 1 << 20), ("K", 1 << 10)];

    for (suffix, scale) in UNITS {
        if bytes >= scale {
            return format!("{:.1}{suffix}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes}B")
}

pub async fn delete(app: &Application, session_id: &str) -> Result<()> {
    app.sessions
        .delete(session_id)
        .await
        .with_context(|| format!("cannot delete session `{session_id}`"))?;

    println!("deleted {session_id}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_at_a_glance() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(999), "999B");
        assert_eq!(human_bytes(1024), "1.0K");
        assert_eq!(human_bytes(1_572_864), "1.5M");
        assert_eq!(human_bytes(3 << 30), "3.0G");
    }
}

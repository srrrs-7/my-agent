//! File tools.
//!
//! All of them speak in workspace-relative paths and obtain those paths from
//! [`agent_domain::model::workspace::WorkspaceRoot`], which is what keeps the
//! model inside the sandbox.

mod edit;
mod list;
mod read;
mod search;
mod write;

pub use edit::EditFileTool;
pub use list::ListDirectoryTool;
pub use read::ReadFileTool;
pub use search::SearchFilesTool;
pub use write::WriteFileTool;

/// Human-friendly byte count for tool output.
pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::human_bytes;

    #[test]
    fn formats_byte_counts() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}

//! The hooks file (`AGENT_HOOKS`).
//!
//! ```json
//! {
//!   "pre_tool_use":  [{ "match": "run_command", "command": "/opt/hooks/check.sh", "timeout_secs": 5 }],
//!   "post_tool_use": [{ "match": "write_file", "command": "prettier --write \"$(jq -r .arguments.path)\"" }],
//!   "stop":          [{ "command": "/opt/hooks/turn-done.sh" }]
//! }
//! ```
//!
//! A pre hook can refuse a call, so the file decides what the agent may do
//! and is the operator's alone, on the terms of the MCP configuration and the
//! permission rules: it must live outside the workspace, and so must every
//! file its commands name.
//!
//! That last check is a heuristic, and a wider one than MCP's. A hook runs in
//! the workspace (a formatter has to find the file it formats), so a relative
//! word resolves there too: `sh scripts/check.sh` would run whatever the
//! repository - or the model, one call earlier - put in `scripts/check.sh`.
//! Every word of the line that names an existing file inside the workspace,
//! absolute or relative, is therefore refused. A command that reaches
//! workspace code some other way (a variable, `$(cat ...)`, an interpreter's
//! own search path) is not caught.
//!
//! Unknown fields are errors, so a misspelt `timeout_sec` or a `match` on a
//! stop hook cannot silently fall back to something the operator did not
//! write.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use agent_domain::model::tool::ToolName;
use serde::Deserialize;
use thiserror::Error;

use crate::mcp::config::refuse_inside;

/// Larger than any hand-written hooks file needs.
pub const MAX_HOOKS_BYTES: u64 = 256 * 1024;
/// Per event. Each matching hook is a process on every call.
pub const MAX_HOOKS_PER_EVENT: usize = 32;
/// Longest a hook may be given. A pre hook holds up the call it is about.
pub const MAX_TIMEOUT_SECS: u64 = 600;
/// What a hook gets when the file does not say.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Matches every tool.
const ANY_TOOL: &str = "*";

#[derive(Debug, Error)]
pub enum HooksError {
    #[error("cannot read the hooks file `{path}`: {message}")]
    Unreadable { path: String, message: String },

    #[error("the hooks file `{path}` is larger than {limit} bytes")]
    TooLarge { path: String, limit: u64 },

    #[error("{0}")]
    InsideWorkspace(String),

    #[error("the hooks file is not valid: {message}")]
    Parse { message: String },

    #[error("{event} hook {index}: {reason}")]
    Invalid {
        event: &'static str,
        index: usize,
        reason: String,
    },
}

/// One hook on a tool event.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolHook {
    /// A tool name, or `*` for every tool.
    #[serde(rename = "match")]
    pub matcher: String,
    /// A `/bin/sh -c` line.
    pub command: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

impl ToolHook {
    pub fn matches(&self, tool: &str) -> bool {
        self.matcher == ANY_TOOL || self.matcher == tool
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }
}

/// One hook on the end of a turn. No `match`: there is no tool to match.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopHook {
    pub command: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

impl StopHook {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

/// The whole file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HooksConfig {
    #[serde(default)]
    pub pre_tool_use: Vec<ToolHook>,
    #[serde(default)]
    pub post_tool_use: Vec<ToolHook>,
    #[serde(default)]
    pub stop: Vec<StopHook>,
}

impl HooksConfig {
    /// Reads, parses and checks the file at `path` against the canonical
    /// `workspace`: neither the file nor any file its commands name may live
    /// inside it.
    pub fn load(path: &Path, workspace: &Path) -> Result<Self, HooksError> {
        let unreadable = |error: std::io::Error| HooksError::Unreadable {
            path: path.display().to_string(),
            message: error.to_string(),
        };
        let resolved = std::fs::canonicalize(path).map_err(unreadable)?;
        refuse_inside(workspace, &resolved, "the hooks file")
            .map_err(|error| HooksError::InsideWorkspace(error.to_string()))?;

        let size = std::fs::metadata(&resolved).map_err(unreadable)?.len();
        if size > MAX_HOOKS_BYTES {
            return Err(HooksError::TooLarge {
                path: path.display().to_string(),
                limit: MAX_HOOKS_BYTES,
            });
        }
        let config = Self::parse(&std::fs::read_to_string(&resolved).map_err(unreadable)?)?;
        config.refuse_workspace_files(workspace)?;
        Ok(config)
    }

    /// The format and the limits, with nothing read from disk.
    pub fn parse(text: &str) -> Result<Self, HooksError> {
        let config: Self = serde_json::from_str(text).map_err(|error| HooksError::Parse {
            message: error.to_string(),
        })?;
        for (event, count) in [
            ("pre_tool_use", config.pre_tool_use.len()),
            ("post_tool_use", config.post_tool_use.len()),
            ("stop", config.stop.len()),
        ] {
            if count > MAX_HOOKS_PER_EVENT {
                return Err(HooksError::Parse {
                    message: format!(
                        "{count} `{event}` hooks, more than the {MAX_HOOKS_PER_EVENT} allowed"
                    ),
                });
            }
        }
        for (event, hooks) in [
            ("pre_tool_use", &config.pre_tool_use),
            ("post_tool_use", &config.post_tool_use),
        ] {
            for (index, hook) in hooks.iter().enumerate() {
                check_matcher(&hook.matcher)
                    .and_then(|()| check_command(&hook.command, hook.timeout_secs))
                    .map_err(|reason| invalid(event, index, reason))?;
            }
        }
        for (index, hook) in config.stop.iter().enumerate() {
            check_command(&hook.command, hook.timeout_secs)
                .map_err(|reason| invalid("stop", index, reason))?;
        }
        Ok(config)
    }

    pub fn is_empty(&self) -> bool {
        self.pre_tool_use.is_empty() && self.post_tool_use.is_empty() && self.stop.is_empty()
    }

    /// The tool names a `match` gives that are not in `registered` - each a
    /// hook that would never run. Read by the composition root, which knows
    /// the tools; a pre hook meant to refuse something and quietly matching
    /// nothing is a protection the operator believes they have.
    pub fn unknown_tools(&self, registered: &[String]) -> Vec<String> {
        let mut unknown: Vec<String> = self
            .pre_tool_use
            .iter()
            .chain(&self.post_tool_use)
            .map(|hook| hook.matcher.as_str())
            .filter(|matcher| *matcher != ANY_TOOL)
            .filter(|matcher| !registered.iter().any(|name| name == matcher))
            .map(str::to_string)
            .collect();
        unknown.sort();
        unknown.dedup();
        unknown
    }

    fn commands(&self) -> impl Iterator<Item = (&'static str, usize, &str)> {
        self.pre_tool_use
            .iter()
            .enumerate()
            .map(|(index, hook)| ("pre_tool_use", index, hook.command.as_str()))
            .chain(
                self.post_tool_use
                    .iter()
                    .enumerate()
                    .map(|(index, hook)| ("post_tool_use", index, hook.command.as_str())),
            )
            .chain(
                self.stop
                    .iter()
                    .enumerate()
                    .map(|(index, hook)| ("stop", index, hook.command.as_str())),
            )
    }

    fn refuse_workspace_files(&self, workspace: &Path) -> Result<(), HooksError> {
        for (event, index, command) in self.commands() {
            if let Some(file) = workspace_file(command, workspace) {
                return Err(invalid(
                    event,
                    index,
                    format!(
                        "the command names `{}`, a file inside the workspace. Anything in the \
                         workspace can be changed by the repository or by the model, so it \
                         cannot decide what a hook runs. Move the script outside the workspace \
                         and name it by its absolute path",
                        file.display()
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// Counted from 1, as a person reading the file counts.
fn invalid(event: &'static str, index: usize, reason: String) -> HooksError {
    HooksError::Invalid {
        event,
        index: index + 1,
        reason,
    }
}

fn check_matcher(matcher: &str) -> Result<(), String> {
    if matcher == ANY_TOOL {
        return Ok(());
    }
    ToolName::new(matcher)
        .map(|_| ())
        .map_err(|_| format!("`match` must be a tool name or `*`, got `{matcher}`"))
}

fn check_command(command: &str, timeout_secs: u64) -> Result<(), String> {
    if command.trim().is_empty() {
        return Err("`command` is empty".to_string());
    }
    if command.contains('\0') {
        return Err("`command` contains a NUL byte".to_string());
    }
    if !(1..=MAX_TIMEOUT_SECS).contains(&timeout_secs) {
        return Err(format!(
            "`timeout_secs` must be between 1 and {MAX_TIMEOUT_SECS}, got {timeout_secs}"
        ));
    }
    Ok(())
}

/// The first word of `command` that names an existing file inside
/// `workspace` - see the module docs for why relative words count.
fn workspace_file(command: &str, workspace: &Path) -> Option<PathBuf> {
    let words = command
        .split(|c: char| c.is_whitespace() || ";|&()<>`".contains(c))
        .map(|word| word.trim_matches(|c| c == '\'' || c == '"'))
        .filter(|word| !word.is_empty() && !word.starts_with('-') && !word.contains('$'));
    for word in words {
        let path = Path::new(word);
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            workspace.join(path)
        };
        // Lexically first: a link inside the workspace that points out today
        // can be pointed elsewhere tomorrow, by whoever can write there.
        if lexically_inside(&candidate, workspace) && candidate.is_file() {
            return Some(candidate);
        }
        if let Ok(target) = std::fs::canonicalize(&candidate)
            && target.is_file()
            && target.starts_with(workspace)
        {
            return Some(target);
        }
    }
    None
}

/// Whether `path` stays under `root` once `.` and `..` are taken literally.
fn lexically_inside(path: &Path, root: &Path) -> bool {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normal.pop();
            }
            Component::CurDir => {}
            other => normal.push(other),
        }
    }
    normal.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path()).unwrap();
        (dir, path)
    }

    #[test]
    fn a_full_file_parses_with_defaults_filled_in() {
        let config = HooksConfig::parse(
            r#"{
                "pre_tool_use": [{"match": "run_command", "command": "exit 0", "timeout_secs": 5}],
                "post_tool_use": [{"match": "*", "command": "cat >/dev/null"}],
                "stop": [{"command": "true"}]
            }"#,
        )
        .unwrap();
        assert_eq!(config.pre_tool_use[0].timeout(), Duration::from_secs(5));
        assert_eq!(
            config.post_tool_use[0].timeout(),
            Duration::from_secs(DEFAULT_TIMEOUT_SECS)
        );
        assert!(config.post_tool_use[0].matches("anything"));
        assert!(config.pre_tool_use[0].matches("run_command"));
        assert!(!config.pre_tool_use[0].matches("write_file"));
    }

    #[test]
    fn unknown_fields_are_refused_at_every_level() {
        for text in [
            r#"{"pre_tool_use": [], "on_error": []}"#,
            r#"{"pre_tool_use": [{"match": "*", "command": "x", "timeout_sec": 5}]}"#,
            // A stop hook has no tool to match: a `match` there is a mistake.
            r#"{"stop": [{"match": "*", "command": "x"}]}"#,
        ] {
            let error = HooksConfig::parse(text).unwrap_err();
            assert!(
                matches!(error, HooksError::Parse { .. }),
                "{text} must be refused: {error}"
            );
            assert!(error.to_string().contains("unknown field"), "{error}");
        }
    }

    #[test]
    fn a_bad_match_or_timeout_names_the_hook() {
        let error = HooksConfig::parse(
            r#"{"post_tool_use": [{"match": "*", "command": "x"}, {"match": "write file", "command": "x"}]}"#,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("post_tool_use hook 2: `match`"),
            "{error}"
        );

        let error =
            HooksConfig::parse(r#"{"stop": [{"command": "x", "timeout_secs": 0}]}"#).unwrap_err();
        assert!(error.to_string().contains("timeout_secs"), "{error}");
    }

    #[test]
    fn a_hooks_file_inside_the_workspace_is_refused() {
        let (_dir, workspace) = canonical_tempdir();
        let path = workspace.join("hooks.json");
        std::fs::write(&path, "{}").unwrap();

        let error = HooksConfig::load(&path, &workspace).unwrap_err();
        assert!(matches!(error, HooksError::InsideWorkspace(_)), "{error}");
        assert!(error.to_string().contains("the hooks file"), "{error}");
    }

    #[test]
    fn a_link_from_outside_to_a_file_inside_is_refused_too() {
        let (_dir, workspace) = canonical_tempdir();
        let (_outside_dir, outside) = canonical_tempdir();
        std::fs::write(workspace.join("hooks.json"), "{}").unwrap();
        let link = outside.join("hooks.json");
        std::os::unix::fs::symlink(workspace.join("hooks.json"), &link).unwrap();

        let error = HooksConfig::load(&link, &workspace).unwrap_err();
        assert!(matches!(error, HooksError::InsideWorkspace(_)), "{error}");
    }

    fn load_with(command: &str, workspace: &Path, outside: &Path) -> Result<(), HooksError> {
        let path = outside.join("hooks.json");
        let text = serde_json::json!({
            "pre_tool_use": [{"match": "*", "command": command}]
        });
        std::fs::write(&path, text.to_string()).unwrap();
        HooksConfig::load(&path, workspace).map(|_| ())
    }

    #[test]
    fn a_command_naming_a_workspace_file_is_refused_absolute_or_relative() {
        let (_dir, workspace) = canonical_tempdir();
        let (_outside_dir, outside) = canonical_tempdir();
        std::fs::create_dir(workspace.join("scripts")).unwrap();
        std::fs::write(workspace.join("scripts/check.sh"), "exit 0").unwrap();
        std::fs::write(outside.join("check.sh"), "exit 0").unwrap();

        for command in [
            format!("/bin/sh {}/scripts/check.sh", workspace.display()),
            "sh scripts/check.sh".to_string(),
            "cd x && sh './scripts/check.sh'".to_string(),
            "true; sh scripts/../scripts/check.sh".to_string(),
        ] {
            let error = load_with(&command, &workspace, &outside).unwrap_err();
            assert!(
                error.to_string().contains("inside the workspace"),
                "`{command}` must be refused: {error}"
            );
        }

        // Outside the workspace, and words that are not files there, pass.
        for command in [
            format!("/bin/sh {}/check.sh", outside.display()),
            "jq -e '.tool != \"run_command\"'".to_string(),
            "prettier --write \"$(jq -r .arguments.path)\"".to_string(),
            "sh -c 'exit 0' scripts".to_string(),
        ] {
            load_with(&command, &workspace, &outside)
                .unwrap_or_else(|error| panic!("`{command}` must load: {error}"));
        }
    }

    #[test]
    fn a_workspace_link_to_an_outside_script_is_refused() {
        let (_dir, workspace) = canonical_tempdir();
        let (_outside_dir, outside) = canonical_tempdir();
        std::fs::write(outside.join("check.sh"), "exit 0").unwrap();
        std::os::unix::fs::symlink(outside.join("check.sh"), workspace.join("check.sh")).unwrap();

        // Today the link points out; whoever writes the workspace can point
        // it somewhere else tomorrow.
        let error = load_with("sh check.sh", &workspace, &outside).unwrap_err();
        assert!(
            error.to_string().contains("inside the workspace"),
            "{error}"
        );
    }

    #[test]
    fn unknown_tools_are_reported_and_the_wildcard_is_not() {
        let config = HooksConfig::parse(
            r#"{
                "pre_tool_use": [{"match": "run_comand", "command": "x"}, {"match": "*", "command": "x"}],
                "post_tool_use": [{"match": "write_file", "command": "x"}]
            }"#,
        )
        .unwrap();
        assert_eq!(
            config.unknown_tools(&["write_file".to_string(), "run_command".to_string()]),
            vec!["run_comand".to_string()]
        );
    }
}

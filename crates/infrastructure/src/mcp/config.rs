//! The MCP configuration file (`AGENT_MCP_CONFIG`).
//!
//! Whoever controls this file chooses programs to start, so it is the
//! operator's, and only the operator's:
//!
//! * **It must live outside the workspace.** A repository that shipped one
//!   would start code of its own choosing the moment the agent was launched in
//!   it, and the model could rewrite one for the next session.
//! * **So must every program it starts.** An absolute path is required (no
//!   `PATH` lookup), it is resolved through symlinks, and refused if it lands
//!   in the workspace. An absolute argument naming a *file* in the workspace
//!   is refused too - `/usr/bin/node /workspace/server.js` is the same hole by
//!   another door. This last check is a heuristic: an interpreter told to load
//!   workspace code some other way (an environment variable, a relative
//!   import) is not caught.
//!
//! Unknown fields are errors rather than ignored, so a misspelt
//! `workspace_acess` cannot silently fall back to a default.
//!
//! Secrets do not belong in the file. Reads are not confined, so anything
//! written here can be read by every server and every `run_command` child
//! that knows where to look. A server's credentials are named in `env_from`
//! instead and copied from the agent's own environment at startup; there they
//! are withheld from every other child by value.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// Larger than any hand-written configuration needs.
pub const MAX_CONFIG_BYTES: u64 = 256 * 1024;
/// Each server is a process, a proxy and a directory for the whole session.
pub const MAX_SERVERS: usize = 8;
pub const MAX_ARGS: usize = 64;
pub const MAX_ENV: usize = 64;
pub const MAX_ALLOW_HOSTS: usize = 64;
/// Long enough to be descriptive, short enough that `mcp__<name>__` leaves
/// most of the provider's 64 characters to the tool.
pub const MAX_SERVER_NAME_CHARS: usize = 16;

/// Whether a server may write to the workspace.
///
/// There is no read-only level, because there is nothing to enforce it with:
/// every sandboxed process can read the whole filesystem, workspace included
/// (Landlock confines writes; a read allowlist tight enough to matter breaks
/// every runtime). Reads are open either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceAccess {
    /// Writes go to the server's own directory and nowhere else. The default:
    /// a server runs between calls too, where neither the approval prompt nor
    /// the dependency watch can see what it does.
    #[default]
    None,
    /// The workspace is writable as well.
    ReadWrite,
}

/// One server, as the operator wrote it.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Absolute path of the program. Replaced by its resolved location once
    /// the file has been checked.
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    /// Set in the server's environment after everything secret-looking from
    /// this process's own environment has been removed. Written in the file,
    /// so readable by every server and child: for values that are not
    /// secret. Still hidden from tool output, in case one is.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Names of variables copied from the agent's own environment into the
    /// server's, at startup - the way to hand a server a secret. Missing one
    /// fails startup. The values are hidden from tool output and withheld
    /// from every other child.
    #[serde(default)]
    pub env_from: Vec<String>,
    /// What `env_from` resolved to. Filled by [`McpConfig::load`] (or
    /// [`McpConfig::resolve_env`]), never read from the file.
    #[serde(skip)]
    pub inherited: BTreeMap<String, String>,
    /// Domain suffixes the server may reach through its egress proxy. Empty
    /// means no network at all.
    #[serde(default)]
    pub allow_hosts: Vec<String>,
    #[serde(default)]
    pub workspace_access: WorkspaceAccess,
}

/// Environment values may be secrets, so they never reach a log.
impl fmt::Debug for ServerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerConfig")
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("env_from", &self.env_from)
            .field("allow_hosts", &self.allow_hosts)
            .field("workspace_access", &self.workspace_access)
            .finish()
    }
}

/// The whole file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    pub servers: BTreeMap<String, ServerConfig>,
}

/// Why the configuration was refused. Read by the operator at startup.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum McpConfigError {
    #[error("cannot read the MCP configuration `{path}`: {message}")]
    Unreadable { path: String, message: String },

    #[error("the MCP configuration `{path}` is larger than {limit} bytes")]
    TooLarge { path: String, limit: u64 },

    #[error("the MCP configuration is not valid: {message}")]
    Parse { message: String },

    #[error("MCP server `{server}`: {reason}")]
    Invalid { server: String, reason: String },

    #[error(
        "{what} `{path}` is inside the workspace. Anything in the workspace can be changed by \
         the repository or by the model, so it cannot decide what runs. Move it outside the \
         workspace"
    )]
    InsideWorkspace { what: String, path: String },

    #[error(
        "MCP server `{server}`: `env_from` names `{name}`, which is not set in the agent's \
         environment. Set it (for example in `.env`), or remove it from `env_from`"
    )]
    MissingVariable { server: String, name: String },
}

impl McpConfig {
    /// Reads, parses and checks the file at `path` against the canonical
    /// `workspace` - including that neither the file nor any program it
    /// starts lives inside it - and resolves every `env_from` against
    /// `environment` (the agent's own variables).
    pub fn load(
        path: &Path,
        workspace: &Path,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, McpConfigError> {
        let unreadable = |error: std::io::Error| McpConfigError::Unreadable {
            path: path.display().to_string(),
            message: error.to_string(),
        };
        let resolved = std::fs::canonicalize(path).map_err(unreadable)?;
        refuse_inside(workspace, &resolved, "the MCP configuration")?;

        let size = std::fs::metadata(&resolved).map_err(unreadable)?.len();
        if size > MAX_CONFIG_BYTES {
            return Err(McpConfigError::TooLarge {
                path: path.display().to_string(),
                limit: MAX_CONFIG_BYTES,
            });
        }
        let text = std::fs::read_to_string(&resolved).map_err(unreadable)?;

        let mut config = Self::parse(&text)?;
        for (name, server) in &mut config.servers {
            server.resolve(name, workspace)?;
        }
        config.resolve_env(environment)?;
        Ok(config)
    }

    /// Copies every server's `env_from` variables out of `environment`.
    /// A name that is not there fails, naming it: a server started without
    /// its credential would fail later, and less clearly.
    pub fn resolve_env(
        &mut self,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), McpConfigError> {
        for (server_name, server) in &mut self.servers {
            server.inherited = server
                .env_from
                .iter()
                .map(|name| {
                    environment(name)
                        .map(|value| (name.clone(), value))
                        .ok_or_else(|| McpConfigError::MissingVariable {
                            server: server_name.clone(),
                            name: name.clone(),
                        })
                })
                .collect::<Result<_, _>>()?;
        }
        Ok(())
    }

    /// The format and the limits, with nothing read from disk.
    pub fn parse(text: &str) -> Result<Self, McpConfigError> {
        let config: Self = serde_json::from_str(text).map_err(|error| McpConfigError::Parse {
            message: error.to_string(),
        })?;
        if config.servers.len() > MAX_SERVERS {
            return Err(McpConfigError::Parse {
                message: format!(
                    "{} servers are configured, more than the limit of {MAX_SERVERS}",
                    config.servers.len()
                ),
            });
        }
        for (name, server) in &config.servers {
            check_server_name(name)?;
            server.check(name)?;
        }
        Ok(config)
    }

    /// Every value handed to a server, through `env` or `env_from`, for the
    /// redactor. A server can echo what it was given, and the operator may
    /// well have put it there because it is a credential.
    pub fn secret_values(&self) -> Vec<String> {
        self.servers
            .values()
            .flat_map(|server| server.env.values().chain(server.inherited.values()))
            .cloned()
            .collect()
    }

    /// The values `env_from` resolved to, to be withheld from every child by
    /// value (see `ExecConfig::withheld`): the variable they came from is in
    /// the agent's environment, and other servers and `run_command` inherit
    /// it unless removed.
    ///
    /// Literal `env` values are not included. They are documented as not
    /// secret, and withholding an ordinary value such as a path would take
    /// unrelated variables - `PATH` among them - with it.
    pub fn inherited_values(&self) -> Vec<String> {
        self.servers
            .values()
            .flat_map(|server| server.inherited.values().cloned())
            .collect()
    }
}

fn check_server_name(name: &str) -> Result<(), McpConfigError> {
    let valid = !name.is_empty()
        && name.len() <= MAX_SERVER_NAME_CHARS
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(McpConfigError::Invalid {
            server: name.to_string(),
            reason: format!(
                "server names may only use lowercase letters, digits and `-`, 1 to \
                 {MAX_SERVER_NAME_CHARS} characters (e.g. `issues` or `db-read`)"
            ),
        })
    }
}

impl ServerConfig {
    fn check(&self, name: &str) -> Result<(), McpConfigError> {
        let invalid = |reason: String| McpConfigError::Invalid {
            server: name.to_string(),
            reason,
        };
        if !self.command.is_absolute() {
            return Err(invalid(format!(
                "`command` must be an absolute path (no PATH lookup), got `{}`",
                self.command.display()
            )));
        }
        for (field, count, limit) in [
            ("args", self.args.len(), MAX_ARGS),
            ("env", self.env.len(), MAX_ENV),
            ("env_from", self.env_from.len(), MAX_ENV),
            ("allow_hosts", self.allow_hosts.len(), MAX_ALLOW_HOSTS),
        ] {
            if count > limit {
                return Err(invalid(format!(
                    "`{field}` has {count} entries, more than the limit of {limit}"
                )));
            }
        }
        if self.args.iter().any(|arg| arg.contains('\0')) {
            return Err(invalid("an argument contains a NUL byte".to_string()));
        }
        for (key, value) in &self.env {
            if key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0') {
                return Err(invalid(format!(
                    "`{key}` is not a usable environment variable name and value"
                )));
            }
        }
        for name in &self.env_from {
            if name.is_empty() || name.contains('=') || name.contains('\0') {
                return Err(invalid(format!(
                    "`{name}` in `env_from` is not a usable environment variable name"
                )));
            }
            if self.env.contains_key(name) {
                return Err(invalid(format!(
                    "`{name}` is in both `env` and `env_from`; keep one (`env_from` for a secret)"
                )));
            }
        }
        if self.allow_hosts.iter().any(|host| host.trim().is_empty()) {
            return Err(invalid("`allow_hosts` contains an empty entry".to_string()));
        }
        Ok(())
    }

    /// Pins `command` to the file it resolves to, and refuses it - or a file
    /// argument - inside the workspace.
    fn resolve(&mut self, name: &str, workspace: &Path) -> Result<(), McpConfigError> {
        let resolved =
            std::fs::canonicalize(&self.command).map_err(|error| McpConfigError::Invalid {
                server: name.to_string(),
                reason: format!("cannot use `{}`: {error}", self.command.display()),
            })?;
        if !resolved.is_file() {
            return Err(McpConfigError::Invalid {
                server: name.to_string(),
                reason: format!("`{}` is not a file", self.command.display()),
            });
        }
        refuse_inside(
            workspace,
            &resolved,
            &format!("the program of MCP server `{name}`"),
        )?;

        for arg in &self.args {
            let path = Path::new(arg);
            if !path.is_absolute() {
                continue;
            }
            let Ok(target) = std::fs::canonicalize(path) else {
                continue;
            };
            if target.is_file() {
                refuse_inside(
                    workspace,
                    &target,
                    &format!("a file argument of MCP server `{name}`"),
                )?;
            }
        }

        self.command = resolved;
        Ok(())
    }
}

fn refuse_inside(workspace: &Path, resolved: &Path, what: &str) -> Result<(), McpConfigError> {
    if resolved.starts_with(workspace) {
        return Err(McpConfigError::InsideWorkspace {
            what: what.to_string(),
            path: resolved.display().to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path()).unwrap();
        (dir, path)
    }

    fn write_config(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("mcp.json");
        std::fs::write(&path, text).unwrap();
        path
    }

    /// `/bin/sh` exists everywhere this is built and is never in a workspace.
    const SH_SERVER: &str = r#"{"servers": {"files": {"command": "/bin/sh"}}}"#;

    /// An agent environment with nothing in it.
    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn a_minimal_server_gets_the_safe_defaults() {
        let config = McpConfig::parse(SH_SERVER).unwrap();
        let server = &config.servers["files"];
        assert_eq!(server.workspace_access, WorkspaceAccess::None);
        assert!(server.allow_hosts.is_empty());
        assert!(server.env.is_empty());
        assert!(server.env_from.is_empty());
    }

    #[test]
    fn env_from_copies_the_agents_value_and_marks_it_secret() {
        let mut config = McpConfig::parse(
            r#"{"servers": {"files": {"command": "/bin/sh", "env": {"MODE": "fast"},
                "env_from": ["ISSUES_TOKEN"]}}}"#,
        )
        .unwrap();
        config
            .resolve_env(&|name| (name == "ISSUES_TOKEN").then(|| "from-the-agent-env".into()))
            .unwrap();

        let server = &config.servers["files"];
        assert_eq!(
            server.inherited,
            BTreeMap::from([("ISSUES_TOKEN".into(), "from-the-agent-env".into())])
        );
        assert_eq!(config.inherited_values(), ["from-the-agent-env"]);
        let mut secrets = config.secret_values();
        secrets.sort();
        assert_eq!(secrets, ["fast", "from-the-agent-env"]);
        assert!(
            !format!("{config:?}").contains("from-the-agent-env"),
            "{config:?}"
        );
    }

    #[test]
    fn a_missing_env_from_variable_fails_and_names_it() {
        let mut config = McpConfig::parse(
            r#"{"servers": {"files": {"command": "/bin/sh", "env_from": ["ISSUES_TOKEN"]}}}"#,
        )
        .unwrap();
        let error = config.resolve_env(&no_env).unwrap_err();
        assert_eq!(
            error,
            McpConfigError::MissingVariable {
                server: "files".into(),
                name: "ISSUES_TOKEN".into(),
            }
        );
        assert!(error.to_string().contains("`ISSUES_TOKEN`"), "{error}");
    }

    #[test]
    fn load_resolves_env_from() {
        let (_ws, workspace) = canonical_tempdir();
        let (_cfg, outside) = canonical_tempdir();
        let path = write_config(
            &outside,
            r#"{"servers": {"files": {"command": "/bin/sh", "env_from": ["T"]}}}"#,
        );
        assert!(matches!(
            McpConfig::load(&path, &workspace, &no_env),
            Err(McpConfigError::MissingVariable { .. })
        ));
        let config = McpConfig::load(&path, &workspace, &|_| Some("v".into())).unwrap();
        assert_eq!(config.servers["files"].inherited["T"], "v");
    }

    #[test]
    fn a_bad_or_doubled_env_from_name_is_refused() {
        for text in [
            r#"{"servers": {"files": {"command": "/bin/sh", "env_from": [""]}}}"#,
            r#"{"servers": {"files": {"command": "/bin/sh", "env_from": ["A=B"]}}}"#,
            r#"{"servers": {"files": {"command": "/bin/sh", "env": {"T": "x"}, "env_from": ["T"]}}}"#,
        ] {
            assert!(
                matches!(McpConfig::parse(text), Err(McpConfigError::Invalid { .. })),
                "{text}"
            );
        }
    }

    #[test]
    fn workspace_access_is_spelled_in_kebab_case() {
        let config = McpConfig::parse(
            r#"{"servers": {"files": {"command": "/bin/sh", "workspace_access": "read-write"}}}"#,
        )
        .unwrap();
        assert_eq!(
            config.servers["files"].workspace_access,
            WorkspaceAccess::ReadWrite
        );
        assert!(
            McpConfig::parse(
                r#"{"servers": {"files": {"command": "/bin/sh", "workspace_access": "read-only"}}}"#,
            )
            .is_err(),
            "there is no read-only level to ask for"
        );
    }

    #[test]
    fn bad_json_is_refused() {
        assert!(matches!(
            McpConfig::parse("{\"servers\": "),
            Err(McpConfigError::Parse { .. })
        ));
    }

    #[test]
    fn unknown_fields_are_refused_rather_than_ignored() {
        for text in [
            r#"{"servers": {"files": {"command": "/bin/sh", "workspace_acess": "read-write"}}}"#,
            r#"{"servers": {}, "mcpServers": {}}"#,
        ] {
            let error = McpConfig::parse(text).unwrap_err();
            assert!(error.to_string().contains("unknown field"), "{error}");
        }
    }

    #[test]
    fn a_bad_server_name_is_refused() {
        for name in ["", "Files", "my_server", "a.b", "seventeen-chars-x"] {
            let text = format!(r#"{{"servers": {{"{name}": {{"command": "/bin/sh"}}}}}}"#);
            assert!(
                matches!(McpConfig::parse(&text), Err(McpConfigError::Invalid { .. })),
                "`{name}` must be refused"
            );
        }
        assert!(McpConfig::parse(r#"{"servers": {"db-2": {"command": "/bin/sh"}}}"#).is_ok());
    }

    #[test]
    fn a_relative_command_is_refused() {
        let error = McpConfig::parse(r#"{"servers": {"files": {"command": "npx"}}}"#).unwrap_err();
        assert!(error.to_string().contains("absolute"), "{error}");
    }

    #[test]
    fn env_values_are_secrets_and_stay_out_of_debug_output() {
        let config = McpConfig::parse(
            r#"{"servers": {"files": {"command": "/bin/sh", "env": {"TOKEN": "s3cr3t-value"}}}}"#,
        )
        .unwrap();
        assert_eq!(config.secret_values(), vec!["s3cr3t-value".to_string()]);
        let debug = format!("{config:?}");
        assert!(
            debug.contains("TOKEN") && !debug.contains("s3cr3t"),
            "{debug}"
        );
    }

    #[test]
    fn a_config_outside_the_workspace_loads_with_the_command_resolved() {
        let (_ws, workspace) = canonical_tempdir();
        let (_cfg, outside) = canonical_tempdir();
        let path = write_config(&outside, SH_SERVER);

        let config = McpConfig::load(&path, &workspace, &no_env).unwrap();
        assert!(config.servers["files"].command.is_absolute());
    }

    #[test]
    fn a_config_inside_the_workspace_is_refused() {
        let (_ws, workspace) = canonical_tempdir();
        let path = write_config(&workspace, SH_SERVER);

        let error = McpConfig::load(&path, &workspace, &no_env).unwrap_err();
        assert!(
            matches!(error, McpConfigError::InsideWorkspace { .. }),
            "{error:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_outside_pointing_at_a_config_inside_is_refused() {
        let (_ws, workspace) = canonical_tempdir();
        let (_cfg, outside) = canonical_tempdir();
        let inside = write_config(&workspace, SH_SERVER);
        let link = outside.join("mcp.json");
        std::os::unix::fs::symlink(&inside, &link).unwrap();

        assert!(matches!(
            McpConfig::load(&link, &workspace, &no_env),
            Err(McpConfigError::InsideWorkspace { .. })
        ));
    }

    #[test]
    fn a_program_inside_the_workspace_is_refused() {
        let (_ws, workspace) = canonical_tempdir();
        let (_cfg, outside) = canonical_tempdir();
        let program = workspace.join("server");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();
        let path = write_config(
            &outside,
            &format!(
                r#"{{"servers": {{"files": {{"command": "{}"}}}}}}"#,
                program.display()
            ),
        );

        let error = McpConfig::load(&path, &workspace, &no_env).unwrap_err();
        assert!(
            matches!(&error, McpConfigError::InsideWorkspace { what, .. } if what.contains("program")),
            "{error:?}"
        );
    }

    #[test]
    fn a_script_argument_inside_the_workspace_is_refused_but_the_workspace_itself_is_not() {
        let (_ws, workspace) = canonical_tempdir();
        let (_cfg, outside) = canonical_tempdir();
        let script = workspace.join("server.js");
        std::fs::write(&script, "").unwrap();

        let path = write_config(
            &outside,
            &format!(
                r#"{{"servers": {{"files": {{"command": "/bin/sh", "args": ["{}"]}}}}}}"#,
                script.display()
            ),
        );
        assert!(matches!(
            McpConfig::load(&path, &workspace, &no_env),
            Err(McpConfigError::InsideWorkspace { .. })
        ));

        // A server that is meant to work on the workspace is told where it is.
        let path = write_config(
            &outside,
            &format!(
                r#"{{"servers": {{"files": {{"command": "/bin/sh", "args": ["{}"]}}}}}}"#,
                workspace.display()
            ),
        );
        assert!(McpConfig::load(&path, &workspace, &no_env).is_ok());
    }

    #[test]
    fn a_missing_program_is_refused_with_its_path() {
        let (_ws, workspace) = canonical_tempdir();
        let (_cfg, outside) = canonical_tempdir();
        let path = write_config(
            &outside,
            r#"{"servers": {"files": {"command": "/nonexistent/server"}}}"#,
        );
        let error = McpConfig::load(&path, &workspace, &no_env).unwrap_err();
        assert!(error.to_string().contains("/nonexistent/server"), "{error}");
    }
}

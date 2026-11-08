//! The agent's configuration, as an embedder supplies it.

use std::fmt;
use std::path::{Path, PathBuf};

use agent_infrastructure::config::{MapEnv, Settings, SystemEnv};

use crate::Error;

/// Everything the agent is configured with.
///
/// Read from the same `AGENT_*` variables the `agent` binary reads (README
/// §8 lists them), either from the process environment or from pairs the
/// embedder keeps wherever it likes. The variables are the stable surface;
/// the parsed form behind this type is not.
///
/// The variables also feed the redactor: a value that looks like a secret is
/// kept out of what the model sees, as in the binary.
#[derive(Clone)]
pub struct Config {
    settings: Settings,
    /// The variables the configuration was read from, for the redactor and
    /// for MCP servers' `env_from`.
    environment: Vec<(String, String)>,
}

impl Config {
    /// Reads the process environment. No `.env` file is loaded: that is a
    /// convenience of the binary, and a library that read files on its own
    /// would surprise its host.
    pub fn from_env() -> Result<Self, Error> {
        let settings = Settings::from_env().map_err(Error::new)?;
        Ok(Self {
            settings,
            environment: SystemEnv.vars(),
        })
    }

    /// Reads `vars` instead of the process environment - for a host that
    /// keeps its configuration elsewhere, and for tests, which must not
    /// touch the process environment. A variable left out takes the same
    /// default it would in the binary; `AGENT_WORKSPACE` left out means the
    /// current directory.
    pub fn from_vars<I, K, V>(vars: I) -> Result<Self, Error>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut env = MapEnv::default();
        let mut environment = Vec::new();
        for (key, value) in vars {
            env.set(key.as_ref(), value.as_ref());
            environment.push((key.as_ref().to_string(), value.as_ref().to_string()));
        }
        let settings = Settings::from_source(&env).map_err(Error::new)?;
        Ok(Self {
            settings,
            environment,
        })
    }

    /// Points the agent at `workspace`, whatever `AGENT_WORKSPACE` said. The
    /// directory must exist when the agent is built.
    pub fn with_workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        self.settings.workspace = workspace.into();
        self
    }

    /// The workspace the agent will work in, as configured (not yet
    /// canonicalised).
    pub fn workspace(&self) -> &Path {
        &self.settings.workspace
    }

    /// The parsed settings. Not stable: their shape follows the
    /// infrastructure crate.
    #[doc(hidden)]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub(crate) fn into_parts(self) -> (Settings, Vec<(String, String)>) {
        (self.settings, self.environment)
    }
}

/// Only the workspace: the rest holds API keys.
impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("workspace", &self.settings.workspace)
            .finish_non_exhaustive()
    }
}

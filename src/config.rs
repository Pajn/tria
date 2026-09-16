use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Client configuration stored at `$XDG_CONFIG_HOME/tria/config.toml`, defaulting to
/// `~/.config/tria/config.toml` (mode 0600).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    pub url: Option<String>,
    pub token: Option<String>,
    /// Shell command `gl` and `:git` run in the thread's directory. Defaults to `lazygit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_command: Option<String>,
    /// Editor for `ge` and `gE`. Defaults to `$VISUAL`, then `$EDITOR`, then `nvim`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,
    /// Command that starts a local server when none is running. Defaults to `t3 serve`;
    /// empty turns starting one off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_command: Option<String>,
    /// The last model picked, used for the next new thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<crate::model::ModelSelection>,
}

impl Config {
    /// Remember a model choice for the next new thread. Re-reads the file first so a
    /// concurrent edit elsewhere is not overwritten, and failures are not worth a toast.
    pub fn remember_model(selection: &crate::model::ModelSelection) {
        let mut config = match Self::load() {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!(%err, "reading config to remember the model");
                return;
            }
        };
        if config.model.as_ref() == Some(selection) {
            return;
        }
        config.model = Some(selection.clone());
        if let Err(err) = config.save() {
            tracing::warn!(%err, "saving the remembered model");
        }
    }
}

pub const DEFAULT_GIT_COMMAND: &str = "lazygit";

impl Config {
    pub fn editor(&self) -> String {
        self.editor
            .clone()
            .filter(|e| !e.trim().is_empty())
            .or_else(|| {
                std::env::var("VISUAL")
                    .ok()
                    .filter(|e| !e.trim().is_empty())
            })
            .or_else(|| {
                std::env::var("EDITOR")
                    .ok()
                    .filter(|e| !e.trim().is_empty())
            })
            .unwrap_or_else(|| "nvim".to_string())
    }

    /// The command that starts a server. An empty setting is kept as empty: it is how
    /// starting one is turned off.
    pub fn server_command(&self) -> String {
        self.server_command
            .clone()
            .unwrap_or_else(|| crate::server::DEFAULT_COMMAND.to_string())
    }

    pub fn git_command(&self) -> String {
        self.git_command
            .clone()
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_GIT_COMMAND.to_string())
    }
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        let dir = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => dirs::home_dir()
                .context("no home directory")?
                .join(".config"),
        };
        Ok(dir.join("tria").join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        match fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        fs::write(&path, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

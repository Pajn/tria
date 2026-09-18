use std::{collections::BTreeMap, fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Client configuration stored at `$XDG_CONFIG_HOME/tria/config.toml`, defaulting to
/// `~/.config/tria/config.toml` (mode 0600).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    pub url: Option<String>,
    pub token: Option<String>,
    /// Shell command `gl` and `:git` run in the thread's directory. Defaults to
    /// `lazygit`. The same thing as `programs.l`, which wins where both are set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_command: Option<String>,
    /// Programs bound to `g` and one more key, each run in the thread's directory in the
    /// terminal pane. The key is the one pressed after `g`; the value is the command.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub programs: BTreeMap<String, String>,
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

/// The keys tria answers after `g` itself. A program cannot be given one of these,
/// because the key would never reach it.
pub const TAKEN_KEYS: &str = "!AESTWaegstwxy";

/// A program bound to `g` and one more key, run in the thread's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub key: char,
    pub command: String,
}

impl Program {
    /// The terminal it reuses: one per thread and per binding, so two programs left
    /// running in a thread do not take turns in the same shell.
    pub fn terminal_id(&self) -> String {
        format!("tria-g{}", self.key)
    }

    /// What names it on the command line, which is the program without its arguments.
    pub fn name(&self) -> &str {
        self.command
            .split_whitespace()
            .next()
            .unwrap_or(&self.command)
    }
}

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

    /// The programs `g` will run, and the reasons any of them was refused.
    ///
    /// `l` is lazygit until the config says otherwise, which keeps the binding tria
    /// shipped with and makes it one of the ones that can be moved or taken away. A key
    /// set to nothing removes its binding.
    pub fn programs(&self) -> (Vec<Program>, Vec<String>) {
        let mut programs = vec![Program {
            key: 'l',
            command: self.git_command(),
        }];
        let mut refused = Vec::new();
        for (key, command) in &self.programs {
            let mut letters = key.chars();
            let (Some(key), None) = (letters.next(), letters.next()) else {
                refused.push(format!("`{key}` is not a single key"));
                continue;
            };
            if TAKEN_KEYS.contains(key) {
                refused.push(format!("g{key} is tria's own"));
                continue;
            }
            programs.retain(|program| program.key != key);
            let command = command.trim();
            if !command.is_empty() {
                programs.push(Program {
                    key,
                    command: command.to_string(),
                });
            }
        }
        programs.sort_by_key(|program| program.key);
        (programs, refused)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml: &str) -> Config {
        toml::from_str(toml).expect("a config somebody could have written")
    }

    /// The binding tria ships with is one of the ones the config can move, replace or
    /// take away — otherwise `gl` would be the one program nobody could change.
    #[test]
    fn the_config_says_what_g_runs() {
        let plain = config("");
        assert_eq!(
            plain.programs().0,
            vec![Program {
                key: 'l',
                command: "lazygit".into()
            }]
        );

        let mine = config("[programs]\nb = \"yazi\"\nl = \"gitui\"\n");
        let (programs, refused) = mine.programs();
        assert_eq!(
            programs,
            vec![
                Program {
                    key: 'b',
                    command: "yazi".into()
                },
                Program {
                    key: 'l',
                    command: "gitui".into()
                },
            ]
        );
        assert!(refused.is_empty());
        // Each binding has a terminal of its own, so two programs left running in one
        // thread do not take turns in the same shell.
        assert_eq!(programs[0].terminal_id(), "tria-gb");
        assert_ne!(programs[0].terminal_id(), programs[1].terminal_id());
        assert_eq!(programs[0].name(), "yazi");

        // The older setting still works, and the table wins where both name `l`.
        let older = config("git_command = \"gitui\"\n");
        assert_eq!(older.programs().0[0].command, "gitui");
        let both = config("git_command = \"gitui\"\n[programs]\nl = \"tig\"\n");
        assert_eq!(both.programs().0[0].command, "tig");

        // Nothing is a way of taking the binding away.
        assert!(config("[programs]\nl = \"\"\n").programs().0.is_empty());
    }

    /// A key tria answers itself would never reach the program, so it is refused out
    /// loud rather than bound to something that never runs.
    #[test]
    fn a_key_tria_answers_itself_cannot_be_taken() {
        let (programs, refused) = config("[programs]\nw = \"yazi\"\nbb = \"yazi\"\n").programs();
        assert_eq!(programs.len(), 1, "only the one tria ships with");
        assert_eq!(refused.len(), 2, "{refused:?}");
        assert!(
            refused.iter().any(|said| said.contains("gw")),
            "{refused:?}"
        );
    }
}

use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// The file a running server persists under the T3 home userdata directory.
#[derive(Debug, Deserialize)]
struct ServerRuntimeState {
    origin: String,
}

fn runtime_state_path() -> Result<PathBuf> {
    let home = match std::env::var_os("T3CODE_HOME") {
        Some(home) => PathBuf::from(home),
        None => dirs::home_dir().context("no home directory")?.join(".t3"),
    };
    Ok(home.join("userdata").join("server-runtime.json"))
}

/// Origin of the local server, read from `server-runtime.json`.
pub fn local_origin() -> Result<String> {
    let path = runtime_state_path()?;
    let text = fs::read_to_string(&path)
        .with_context(|| format!("no running local server found ({})", path.display()))?;
    let state: ServerRuntimeState = serde_json::from_str(&text)?;
    Ok(state.origin)
}

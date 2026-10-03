//! SSH transport. OpenSSH owns config, keys and forwarding; T3 owns remote work.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command, task::JoinHandle};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub host: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub identity: String,
}

impl Target {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.host.trim().is_empty(),
            "Enter an SSH host or config alias"
        );
        ensure!(
            !self.host.starts_with('-')
                && !self
                    .host
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control()),
            "Invalid SSH host"
        );
        ensure!(
            !self.user.starts_with('-')
                && !self
                    .user
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control()),
            "Invalid SSH user"
        );
        ensure!(self.port != Some(0), "SSH port must be between 1 and 65535");
        Ok(())
    }

    pub fn label(&self) -> String {
        let host = if self.user.is_empty() {
            self.host.clone()
        } else {
            format!("{}@{}", self.user, self.host)
        };
        match self.port {
            Some(port) => format!("{host}:{port}"),
            None => host,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new("ssh");
        command.args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
        ]);
        if !self.user.is_empty() {
            command.arg("-l").arg(&self.user);
        }
        if let Some(port) = self.port {
            command.arg("-p").arg(port.to_string());
        }
        if !self.identity.is_empty() {
            let path = self
                .identity
                .strip_prefix("~/")
                .and_then(|rest| dirs::home_dir().map(|home| home.join(rest)))
                .unwrap_or_else(|| PathBuf::from(&self.identity));
            command.arg("-i").arg(path);
        }
        command.kill_on_drop(true);
        command
    }

    /// Script bytes go over stdin; paths never become executable shell text.
    pub async fn script(&self, script: &str, wait: Duration) -> Result<Vec<u8>> {
        self.validate()?;
        let mut child = self
            .command()
            .arg(&self.host)
            .arg("sh -lc 'sh -s'")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting ssh")?;
        let mut stdin = child.stdin.take().context("SSH stdin unavailable")?;
        let writer = tokio::spawn({
            let script = script.as_bytes().to_vec();
            async move { stdin.write_all(&script).await }
        });
        let output = tokio::time::timeout(wait, child.wait_with_output())
            .await
            .context("SSH operation timed out")??;
        writer.abort();
        if !output.status.success() {
            // Never report stdout: bootstrap output can contain an access token.
            bail!(
                "SSH {}: {}",
                self.label(),
                String::from_utf8_lossy(&output.stderr)
                    .trim()
                    .chars()
                    .take(1200)
                    .collect::<String>()
            );
        }
        Ok(output.stdout)
    }

    pub async fn connect(&self) -> Result<(String, String, Tunnel)> {
        let bytes = self
            .script(include_str!("ssh_bootstrap.sh"), Duration::from_secs(300))
            .await?;
        let fields = framed(&bytes, b"TRIA_SERVER\0")?;
        let port: u16 = fields
            .first()
            .context("missing remote port")?
            .parse()
            .context("invalid remote port")?;
        ensure!(port != 0, "invalid remote port");
        let token = fields
            .get(1)
            .filter(|t| !t.is_empty())
            .context("missing SSH session token")?
            .clone();
        let host = fields.get(2).context("missing remote forward host")?;
        ensure!(
            !host.is_empty()
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-')),
            "invalid remote forward host"
        );
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.clone()
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let local_port = listener.local_addr()?.port();
        drop(listener);
        let origin = format!("http://127.0.0.1:{local_port}");
        let target = self.clone();
        let task = tokio::spawn(async move {
            loop {
                let result = target
                    .command()
                    .args([
                        "-N",
                        "-o",
                        "ExitOnForwardFailure=yes",
                        "-o",
                        "ControlMaster=no",
                        "-o",
                        "ControlPath=none",
                        "-o",
                        "ForkAfterAuthentication=no",
                        "-L",
                    ])
                    .arg(format!("127.0.0.1:{local_port}:{host}:{port}"))
                    .arg(&target.host)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
                tracing::warn!(host = %target.label(), ?result, "SSH tunnel stopped; restoring forward");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        let tunnel = Tunnel(task);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            if crate::server::is_listening(&origin).await {
                // Prove that this is the authenticated T3 server, not merely a local socket.
                tokio::time::timeout(
                    Duration::from_secs(10),
                    crate::auth::websocket_ticket(&origin, &token),
                )
                .await
                .context("SSH server authentication timed out")??;
                return Ok((origin, token, tunnel));
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "SSH port forward did not open"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

pub struct Tunnel(JoinHandle<()>);
impl Drop for Tunnel {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub fn quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

pub fn framed(bytes: &[u8], marker: &[u8]) -> Result<Vec<String>> {
    let at = bytes
        .windows(marker.len())
        .position(|w| w == marker)
        .context("missing SSH response marker")?;
    let bytes = &bytes[at + marker.len()..];
    ensure!(bytes.last() == Some(&0), "incomplete SSH response");
    bytes[..bytes.len() - 1]
        .split(|b| *b == 0)
        .map(|part| String::from_utf8(part.to_vec()).context("SSH paths must be UTF-8"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[tokio::test]
    async fn bootstrap_starts_once_then_reuses_the_servers_runtime_file() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("tria-bootstrap-{}", crate::commands::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        let runner = root.join("t3");
        std::fs::write(&runner, r#"#!/bin/sh
set -eu
case "$1/$2" in
  __ssh-helper/runtime-port) [ -f "$3" ] || exit 1; if grep '"host"' "$3" >/dev/null; then exit 1; fi; printf '123 3773';;
  __ssh-helper/pick-port) printf '3773';;
  __ssh-helper/wait-ready) i=0; while [ ! -f "$T3CODE_HOME/userdata/server-runtime.json" ]; do sleep 0.1; i=$((i+1)); [ "$i" -lt 30 ] || exit 1; done;;
  serve/--host) printf 'started\n' >> "$T3CODE_HOME/starts"; touch "$T3CODE_HOME/userdata/server-runtime.json";;
  auth/session) printf 'test-session-token\n';;
  *) exit 2;;
esac
"#).unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
        let script = include_str!("ssh_bootstrap.sh").replace(
            "TRIA_RUNNER=\"$(command -v t3 || true)\"",
            &format!("TRIA_RUNNER={}", quote(runner.to_str().unwrap())),
        );
        for _ in 0..2 {
            let output = Command::new("sh")
                .arg("-c")
                .arg(&script)
                .env("T3CODE_HOME", &root)
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                framed(&output.stdout, b"TRIA_SERVER\0").unwrap(),
                ["3773", "test-session-token", "127.0.0.1"]
            );
        }
        assert_eq!(
            std::fs::read_to_string(root.join("starts")).unwrap(),
            "started\n"
        );
        std::fs::write(
            root.join("userdata/server-runtime.json"),
            format!(
                r#"{{"pid":{},"port":4555,"host":"192.168.0.3"}}"#,
                std::process::id()
            ),
        )
        .unwrap();
        let output = Command::new("sh")
            .arg("-c")
            .arg(&script)
            .env("T3CODE_HOME", &root)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            framed(&output.stdout, b"TRIA_SERVER\0").unwrap(),
            ["4555", "test-session-token", "192.168.0.3"]
        );
        assert_eq!(
            std::fs::read_to_string(root.join("starts")).unwrap(),
            "started\n"
        );
        assert!(!root.join("userdata/tria-start.lock").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_options_and_preserves_config_aliases() {
        for host in ["-oProxyCommand=bad", "a\nb", "a b"] {
            assert!(
                Target {
                    host: host.into(),
                    ..Target::default()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            Target {
                host: "work-server".into(),
                ..Target::default()
            }
            .validate()
            .is_ok()
        );
    }
    #[test]
    fn frames_survive_login_banners_without_exposing_secrets_in_errors() {
        assert_eq!(
            framed(b"welcome\nTRIA_SERVER\0 3773\0secret\0", b"TRIA_SERVER\0").unwrap(),
            [" 3773", "secret"]
        );
        assert!(
            framed(b"secret", b"TRIA_SERVER\0")
                .unwrap_err()
                .to_string()
                .contains("marker")
        );
        assert!(framed(b"TRIA_SERVER\0secret", b"TRIA_SERVER\0").is_err());
    }
    #[tokio::test]
    async fn shell_quoting_keeps_metacharacters_literal() {
        let input = "space ' quote\n$(touch /bad); `false`";
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s' {}", quote(input)))
            .output()
            .await
            .unwrap();
        assert_eq!(String::from_utf8(output.stdout).unwrap(), input);
    }
}

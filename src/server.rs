//! Starting a local T3 Code server when there is not one running already.
//!
//! tria is a client: it does not own the server, and the server owns things that must
//! outlive a chat window — provider sessions, background tasks, terminals. So a server
//! started here is left running when tria exits, the same as one started any other way.
//! For a server that is always up, the CLI installs one as a background service.

use std::{
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};

/// How long to wait for a server that was just started to accept connections.
const START_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(100);
/// Long enough to tell "nothing is listening" from a loopback connection being set up.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

pub const DEFAULT_COMMAND: &str = "t3 serve";

/// Whether something accepts connections at the origin. It says nothing about what is
/// listening: a port answered by something else is a problem to report, not one to fix
/// by starting a second server on top of it.
pub async fn is_listening(origin: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return false;
    };
    let connect = tokio::net::TcpStream::connect((host, port));
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, connect).await,
        Ok(Ok(_))
    )
}

/// Whether the origin names this machine, which is the only kind of server tria can
/// start. Pointed at another host, an unreachable server is that host's business.
pub fn is_local(origin: &str) -> bool {
    url::Url::parse(origin)
        .ok()
        .and_then(|url| url.host_str().map(str::to_lowercase))
        .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]"))
}

/// The origin to talk to, starting a server first when the local one is not up. The
/// flag says whether this call is what started it.
///
/// `origin` is what the caller already knows: the `--url` flag, the stored one, or the
/// runtime file. `None` means even the runtime file was missing, which is itself a sign
/// that no server has run.
pub async fn ensure(origin: Option<String>, command: &str) -> Result<(String, bool)> {
    if let Some(origin) = &origin
        && is_listening(origin).await
    {
        return Ok((origin.clone(), false));
    }
    if let Some(origin) = &origin
        && !is_local(origin)
    {
        bail!("no server answering at {origin}");
    }
    if command.trim().is_empty() {
        bail!(
            "no local T3 Code server running, and starting one is turned off \
             (server_command is empty in the config file)"
        );
    }
    println!("No T3 Code server running. Starting one with `{command}`.");
    let origin = start(command).await?;
    println!("Server at {origin}. It keeps running after tria exits.");
    Ok((origin, true))
}

/// Run the command and wait for the server it starts to answer. Returns its origin,
/// which is read from the runtime file rather than assumed: the server picks the port.
async fn start(command: &str) -> Result<String> {
    // Through a shell, so `server_command` can be any command line, and its output to a
    // file so a server that fails to start can say why. Truncated per run: the server
    // keeps its own logs, this is only the first few lines of a bad start.
    let log_path = crate::config::Config::path()?
        .parent()
        .context("no config directory")?
        .join("server-start.log");
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let mut spawn = Command::new("sh");
    spawn
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        // Its own session, so quitting tria — or the terminal tria runs in — does not
        // take the server with it.
        spawn.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = spawn
        .spawn()
        .with_context(|| format!("running `{command}`"))?;

    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        // The command giving up is the fast answer: an uninstalled `t3` says so at once
        // rather than after the whole timeout.
        if let Some(status) = child.try_wait()? {
            let output = std::fs::read_to_string(&log_path).unwrap_or_default();
            let detail = output.lines().take(5).collect::<Vec<_>>().join("\n");
            let detail = if detail.trim().is_empty() {
                format!("see {}", log_path.display())
            } else {
                detail
            };
            bail!("`{command}` exited ({status}) without starting a server:\n{detail}");
        }
        // The file is re-read each time round because the server writes it as it comes
        // up, and it is the server that picks the port. A file left behind by one that
        // is gone names a port nothing answers on, so reaching it is what tells the two
        // apart.
        if let Ok(origin) = crate::discovery::local_origin()
            && is_listening(&origin).await
        {
            return Ok(origin);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "`{command}` did not start a server within {}s (see {})",
                START_TIMEOUT.as_secs(),
                log_path.display()
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A socket that is accepting, and the origin naming it.
    async fn listener() -> (tokio::net::TcpListener, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, format!("http://127.0.0.1:{port}"))
    }

    #[test]
    fn loopback_in_its_several_spellings_is_local() {
        for origin in [
            "http://localhost:3773",
            "http://127.0.0.1:3773",
            "http://[::1]:3773",
            "http://LocalHost:3773",
        ] {
            assert!(is_local(origin), "{origin}");
        }
        for origin in [
            "https://example.com:3773",
            "http://192.168.1.4:3773",
            "nonsense",
        ] {
            assert!(!is_local(origin), "{origin}");
        }
    }

    #[tokio::test]
    async fn listening_is_about_the_socket_answering() {
        let (listener, origin) = listener().await;
        assert!(is_listening(&origin).await);
        drop(listener);
        assert!(!is_listening(&origin).await);
    }

    #[tokio::test]
    async fn a_server_that_answers_is_used_as_it_is() {
        let (_listener, origin) = listener().await;
        // Even with starting one turned off: nothing needs starting.
        let (used, started) = ensure(Some(origin.clone()), "").await.unwrap();
        assert_eq!((used, started), (origin, false));
    }

    #[tokio::test]
    async fn another_host_is_not_ours_to_start() {
        let err = ensure(Some("http://nowhere.invalid:3773".into()), "t3 serve")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no server answering"), "{err}");
    }

    #[tokio::test]
    async fn an_empty_command_turns_starting_one_off() {
        let err = ensure(None, "  ").await.unwrap_err().to_string();
        assert!(err.contains("turned off"), "{err}");
    }
}

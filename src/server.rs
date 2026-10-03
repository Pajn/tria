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

/// Watches discovery without changing the connection. A second server can share the
/// first one's database and execute its turns, even when tria stays on the first socket.
pub struct WarningMonitor {
    pub updates: tokio::sync::mpsc::UnboundedReceiver<Option<String>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for WarningMonitor {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl WarningMonitor {
    /// No local discovery for SSH forwards or remote connections.
    pub fn new(origin: Option<String>) -> Self {
        Self::watch(
            origin.filter(|origin| is_local(origin)),
            crate::discovery::runtime_state_path().ok(),
            Duration::from_secs(5),
        )
    }

    fn watch(origin: Option<String>, path: Option<std::path::PathBuf>, period: Duration) -> Self {
        let (updates, received) = tokio::sync::mpsc::unbounded_channel();
        let task = origin.zip(path).map(|(origin, path)| {
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(period);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut previous = None;
                loop {
                    tick.tick().await;
                    let runtime = crate::discovery::read_runtime(&path).ok();
                    let warning = match runtime {
                        Some(runtime) => duplicate_warning(&origin, &runtime).await,
                        None => None,
                    };
                    if warning != previous {
                        if let Some(warning) = &warning {
                            tracing::warn!(%warning, "another local T3 server detected");
                        }
                        previous = warning.clone();
                        if updates.send(warning).is_err() {
                            break;
                        }
                    }
                }
            })
        });
        Self {
            updates: received,
            task,
        }
    }
}

/// Loopback aliases and a trailing slash can name the very same listener.
fn same_local_listener(first: &str, second: &str) -> bool {
    fn host(url: &url::Url) -> String {
        match url.host_str().unwrap_or_default() {
            "localhost" => "127.0.0.1".into(),
            host => host.to_owned(),
        }
    }
    match (url::Url::parse(first), url::Url::parse(second)) {
        (Ok(first), Ok(second)) => {
            first.scheme() == second.scheme()
                && host(&first) == host(&second)
                && first.port_or_known_default() == second.port_or_known_default()
        }
        _ => first == second,
    }
}

async fn duplicate_warning(
    connected: &str,
    runtime: &crate::discovery::ServerRuntimeState,
) -> Option<String> {
    if !is_local(connected)
        || !is_local(&runtime.origin)
        || same_local_listener(connected, &runtime.origin)
    {
        return None;
    }
    let (connected_up, other_up) =
        tokio::join!(is_listening(connected), is_listening(&runtime.origin),);
    if !connected_up || !other_up {
        return None;
    }
    let pid = runtime
        .pid
        .map(|pid| format!(" (PID {pid})"))
        .unwrap_or_default();
    Some(format!(
        "Two local T3 servers may execute threads twice. Stop one server.\n\
         Connected: {connected}. Other: {}{pid}. Tria stays on its current connection.",
        runtime.origin,
    ))
}

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
    let running = crate::discovery::local_origin().ok();
    ensure_among(origin, running, command).await
}

/// `ensure`, given the origin the runtime file names, if there is one.
async fn ensure_among(
    origin: Option<String>,
    running: Option<String>,
    command: &str,
) -> Result<(String, bool)> {
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
    // The stored origin can be out of date while a server is up somewhere else: the
    // desktop app, say, on a port of its own. Two servers on one T3 home share its
    // database without knowing about each other, and each acts on what the other writes,
    // so a message sent to one gets answered by both. The one already running is the
    // one to use.
    if let Some(running) = running
        && origin.as_ref() != Some(&running)
        && is_listening(&running).await
    {
        if let Some(origin) = &origin {
            println!("Nothing answers at {origin}. Using the T3 Code server at {running}.");
        }
        return Ok((running, false));
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

    #[tokio::test]
    async fn duplicate_warning_requires_two_distinct_live_local_servers() {
        assert!(!same_local_listener(
            "http://127.0.0.1:3773",
            "http://[::1]:3773"
        ));
        let (_connected, origin) = listener().await;
        let (other, other_origin) = listener().await;
        let mut runtime = crate::discovery::ServerRuntimeState {
            origin: other_origin.clone(),
            pid: Some(123),
        };
        let warning = duplicate_warning(&origin, &runtime).await.unwrap();
        assert!(warning.contains(&origin));
        assert!(warning.contains(&other_origin));
        assert!(warning.contains("PID 123"));
        assert!(warning.contains("execute threads twice"));

        runtime.origin = origin.replace("127.0.0.1", "localhost") + "/";
        assert!(duplicate_warning(&origin, &runtime).await.is_none());
        runtime.origin = "https://example.com:3773".into();
        assert!(duplicate_warning(&origin, &runtime).await.is_none());
        runtime.origin = other_origin;
        assert!(
            duplicate_warning("https://example.com", &runtime)
                .await
                .is_none()
        );
        drop(other);
        let (_reserved, absent) = absent_server();
        runtime.origin = absent;
        assert!(duplicate_warning(&origin, &runtime).await.is_none());
    }

    #[tokio::test]
    async fn warning_monitor_notices_runtime_changes_and_clears_for_unavailable_server() {
        let (_connected, origin) = listener().await;
        let (other, other_origin) = listener().await;
        let path = std::env::temp_dir().join(format!("tria-runtime-{}.json", uuid::Uuid::new_v4()));
        let write = |origin: &str| {
            std::fs::write(
                &path,
                serde_json::json!({ "origin": origin, "pid": 123 }).to_string(),
            )
            .unwrap();
        };
        write(&origin);
        let mut monitor = WarningMonitor::watch(
            Some(origin.clone()),
            Some(path.clone()),
            Duration::from_millis(10),
        );
        // The first check names our own server and produces no warning.
        assert!(
            tokio::time::timeout(Duration::from_millis(30), monitor.updates.recv())
                .await
                .is_err()
        );
        write(&other_origin);
        let warning = tokio::time::timeout(Duration::from_secs(2), monitor.updates.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(warning.contains(&origin));
        assert!(warning.contains(&other_origin));
        // A persistent warning needs no repeated notifications.
        assert!(
            tokio::time::timeout(Duration::from_millis(30), monitor.updates.recv())
                .await
                .is_err()
        );
        drop(other);
        let (_reserved, absent) = absent_server();
        write(&absent);
        let cleared = tokio::time::timeout(Duration::from_secs(2), monitor.updates.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(cleared.is_none());
        let task = monitor.task.as_ref().unwrap().abort_handle();
        drop(monitor);
        tokio::task::yield_now().await;
        assert!(task.is_finished());
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn remote_connections_do_not_watch_local_discovery() {
        let monitor = WarningMonitor::new(Some("https://example.com".into()));
        assert!(monitor.task.is_none());
        let monitor = WarningMonitor::new(None);
        assert!(monitor.task.is_none());
    }

    /// A socket that is accepting, and the origin naming it.
    async fn listener() -> (tokio::net::TcpListener, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, format!("http://127.0.0.1:{port}"))
    }

    /// Reserve the port without accepting connections, so a parallel test cannot
    /// reuse it and make an absent server appear alive.
    fn absent_server() -> (tokio::net::TcpSocket, String) {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = socket.local_addr().unwrap().port();
        (socket, format!("http://127.0.0.1:{port}"))
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
        let (_listener, origin) = listener().await;
        assert!(is_listening(&origin).await);
        let (_reserved, absent) = absent_server();
        assert!(!is_listening(&absent).await);
    }

    #[tokio::test]
    async fn a_server_that_answers_is_used_as_it_is() {
        let (_listener, origin) = listener().await;
        // Even with starting one turned off: nothing needs starting.
        let (used, started) = ensure_among(Some(origin.clone()), None, "").await.unwrap();
        assert_eq!((used, started), (origin, false));
    }

    #[tokio::test]
    async fn another_host_is_not_ours_to_start() {
        let err = ensure_among(Some("http://nowhere.invalid:3773".into()), None, "t3 serve")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no server answering"), "{err}");
    }

    #[tokio::test]
    async fn an_empty_command_turns_starting_one_off() {
        let err = ensure_among(None, None, "  ")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("turned off"), "{err}");
    }

    /// A stored origin nothing answers at, with a server running elsewhere on this
    /// machine: that server is used, and a second one is not started beside it.
    #[tokio::test]
    async fn a_server_running_elsewhere_is_used_rather_than_starting_another() {
        let (_running, running) = listener().await;
        let (_reserved, stored) = absent_server();
        // Starting one is turned off, so reaching the start would be an error.
        let (used, started) = ensure_among(Some(stored), Some(running.clone()), "")
            .await
            .unwrap();
        assert_eq!((used, started), (running, false));
    }

    /// A runtime file left behind by a server that has gone names nothing to use.
    #[tokio::test]
    async fn a_runtime_file_nothing_answers_at_is_passed_over() {
        let (_reserved, left_behind) = absent_server();
        let err = ensure_among(None, Some(left_behind), "")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("turned off"), "{err}");
    }
}

//! Route `tria open` to a live client on the same tmux server.
//!
//! Each client advertises a private Unix socket on its pane. The socket's reply
//! confirms that the UI accepted the directory before the caller switches panes.

#[cfg(unix)]
pub use unix::{listen, reuse};

#[cfg(not(unix))]
pub async fn reuse(_root: &str, _origin: Option<&str>) -> anyhow::Result<bool> {
    Ok(false)
}

#[cfg(not(unix))]
pub async fn listen(
    _events: tokio::sync::mpsc::UnboundedSender<crate::app::AppEvent>,
    _origin: &str,
) -> anyhow::Result<Option<()>> {
    Ok(None)
}

#[cfg(unix)]
mod unix {
    use std::{
        fs,
        os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
        process::Stdio,
        time::Duration,
    };

    use anyhow::{Context as _, Result, bail};
    use serde::{Deserialize, Serialize, de::DeserializeOwned};
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::{UnixListener, UnixStream},
        process::Command,
        sync::{mpsc, oneshot},
        task::JoinHandle,
        time::timeout,
    };

    use crate::app::AppEvent;

    const OPTION: &str = "@tria-open-socket";
    const LIMIT: u64 = 16 * 1024;
    const WAIT: Duration = Duration::from_secs(5);

    #[derive(Clone)]
    struct Tmux {
        socket: PathBuf,
        pane: String,
    }

    impl Tmux {
        fn from_env() -> Option<Self> {
            let tmux = std::env::var("TMUX").ok()?;
            let mut parts = tmux.rsplitn(3, ',');
            parts.next()?;
            parts.next()?;
            let socket = PathBuf::from(parts.next()?);
            let pane = std::env::var("TMUX_PANE").ok()?;
            Some(Self { socket, pane })
        }

        fn command(&self) -> Command {
            let mut command = Command::new("tmux");
            command.args(["-S"]).arg(&self.socket);
            command.env("TMUX_PANE", &self.pane);
            command.stdin(Stdio::inherit()).kill_on_drop(true);
            command
        }

        async fn call(&self, args: &[&str]) -> Result<String> {
            let output = timeout(WAIT, self.command().args(args).output())
                .await
                .context("tmux command timed out")??;
            if !output.status.success() {
                bail!("tmux: {}", String::from_utf8_lossy(&output.stderr).trim());
            }
            Ok(String::from_utf8(output.stdout)?.trim_end().to_string())
        }
    }

    #[derive(Serialize, Deserialize)]
    struct Open {
        root: String,
        origin: Option<String>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "status", rename_all = "kebab-case")]
    enum Reply {
        Opened,
        OtherServer,
        Rejected { error: String },
    }

    async fn read<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
        let mut bytes = Vec::new();
        BufReader::new(stream)
            .take(LIMIT)
            .read_until(b'\n', &mut bytes)
            .await?;
        anyhow::ensure!(bytes.last() == Some(&b'\n'), "incomplete tria handoff");
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn write(stream: &mut UnixStream, value: &impl Serialize) -> Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        anyhow::ensure!(bytes.len() <= LIMIT as usize, "tria handoff is too large");
        stream.write_all(&bytes).await?;
        Ok(())
    }

    async fn receive(
        mut stream: UnixStream,
        events: mpsc::UnboundedSender<AppEvent>,
        origin: String,
    ) -> Result<()> {
        let open: Open = read(&mut stream).await?;
        let reply = if open
            .origin
            .as_ref()
            .is_some_and(|wanted| wanted.trim_end_matches('/') != origin.trim_end_matches('/'))
        {
            Reply::OtherServer
        } else if !Path::new(&open.root).is_absolute() {
            Reply::Rejected {
                error: "tria open needs an absolute directory".into(),
            }
        } else {
            let (reply, answer) = oneshot::channel();
            events
                .send(AppEvent::OpenDirectory {
                    root: open.root,
                    reply,
                })
                .map_err(|_| anyhow::anyhow!("tria is shutting down"))?;
            match answer
                .await
                .context("tria closed before opening the directory")?
            {
                Ok(()) => Reply::Opened,
                Err(error) => Reply::Rejected { error },
            }
        };
        write(&mut stream, &reply).await
    }

    /// Owned by the event loop. Normal exit removes both the advertisement and socket.
    pub struct Listener {
        task: JoinHandle<()>,
        socket: PathBuf,
        tmux: Tmux,
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            self.task.abort();
            let _ = fs::remove_file(&self.socket);
            // Another client in the pane may have replaced our advertisement.
            let option = std::process::Command::new("tmux")
                .arg("-S")
                .arg(&self.tmux.socket)
                .args(["show-options", "-pqv", "-t", &self.tmux.pane, OPTION])
                .output();
            if option.is_ok_and(|out| {
                String::from_utf8_lossy(&out.stdout).trim() == self.socket.to_string_lossy()
            }) {
                let _ = std::process::Command::new("tmux")
                    .arg("-S")
                    .arg(&self.tmux.socket)
                    .args(["set-option", "-pu", "-t", &self.tmux.pane, OPTION])
                    .output();
            }
        }
    }

    fn runtime_directory() -> Result<PathBuf> {
        let uid = unsafe { libc::geteuid() };
        // macOS's long per-user temporary path can exceed the Unix socket path limit.
        let directory = dirs::runtime_dir()
            .map(|path| path.join("tria"))
            .unwrap_or_else(|| PathBuf::from(format!("/tmp/tria-{uid}")));
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.into()),
        }
        let metadata = fs::symlink_metadata(&directory)?;
        anyhow::ensure!(
            metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0,
            "tria's socket directory must be private and owned by this user"
        );
        Ok(directory)
    }

    pub async fn listen(
        events: mpsc::UnboundedSender<AppEvent>,
        origin: &str,
    ) -> Result<Option<Listener>> {
        let Some(tmux) = Tmux::from_env() else {
            return Ok(None);
        };
        let directory = runtime_directory()?;
        listen_at(tmux, &directory, events, origin).await.map(Some)
    }

    async fn listen_at(
        tmux: Tmux,
        directory: &Path,
        events: mpsc::UnboundedSender<AppEvent>,
        origin: &str,
    ) -> Result<Listener> {
        let socket = directory.join(format!("{}.sock", crate::commands::new_id()));
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        let origin = origin.to_string();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let events = events.clone();
                let origin = origin.clone();
                tokio::spawn(async move {
                    let _ = timeout(WAIT, receive(stream, events, origin)).await;
                });
            }
        });
        let guard = Listener { task, socket, tmux };
        guard
            .tmux
            .call(&[
                "set-option",
                "-p",
                "-t",
                &guard.tmux.pane,
                OPTION,
                &guard.socket.to_string_lossy(),
            ])
            .await?;
        Ok(guard)
    }

    pub async fn reuse(root: &str, origin: Option<&str>) -> Result<bool> {
        let Some(tmux) = Tmux::from_env() else {
            return Ok(false);
        };
        reuse_at(&tmux, root, origin).await
    }

    async fn reuse_at(tmux: &Tmux, root: &str, origin: Option<&str>) -> Result<bool> {
        let Ok(panes) = tmux
            .call(&[
                "list-panes",
                "-a",
                "-F",
                "#{session_id}\t#{pane_id}\t#{@tria-open-socket}",
            ])
            .await
        else {
            return Ok(false);
        };
        let here = tmux
            .call(&[
                "display-message",
                "-p",
                "-t",
                &tmux.pane,
                "#{session_id}\t#{client_name}",
            ])
            .await?;
        let (session, client) = here.split_once('\t').unwrap_or((&here, ""));
        let mut candidates: Vec<_> = panes
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, '\t');
                let session = parts.next()?;
                let pane = parts.next()?;
                let socket = parts.next()?.trim();
                (!socket.is_empty()).then_some((session, pane, socket))
            })
            .collect();
        candidates.sort_by_key(|(candidate, _, _)| *candidate != session);
        for (_, pane, socket) in candidates {
            let Ok(metadata) = fs::metadata(socket) else {
                continue;
            };
            if !metadata.file_type().is_socket()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
            {
                continue;
            }
            let Ok(Ok(mut stream)) =
                timeout(Duration::from_secs(1), UnixStream::connect(socket)).await
            else {
                continue;
            };
            let request = Open {
                root: root.into(),
                origin: origin.map(str::to_string),
            };
            let reply: Reply = timeout(WAIT, async {
                write(&mut stream, &request).await?;
                read(&mut stream).await
            })
            .await
            .context(
                "existing tria did not acknowledge the directory; it may still open there",
            )??;
            match reply {
                Reply::OtherServer => continue,
                Reply::Rejected { error } => bail!("{error}"),
                Reply::Opened => {
                    let mut args = vec!["switch-client"];
                    if !client.is_empty() {
                        args.extend(["-c", client]);
                    }
                    args.extend(["-t", pane]);
                    tmux.call(&args).await.context(
                        "directory opened in tria, but switching to its tmux pane failed",
                    )?;
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        struct Server {
            tmux: Tmux,
            directory: PathBuf,
            client: Option<std::process::Child>,
        }

        impl Drop for Server {
            fn drop(&mut self) {
                if let Some(client) = &mut self.client {
                    let _ = client.kill();
                    let _ = client.wait();
                }
                let _ = std::process::Command::new("tmux")
                    .arg("-S")
                    .arg(&self.tmux.socket)
                    .arg("kill-server")
                    .output();
                let _ = fs::remove_dir_all(&self.directory);
            }
        }

        impl Server {
            async fn new() -> Option<Self> {
                if std::process::Command::new("tmux")
                    .arg("-V")
                    .output()
                    .is_err()
                {
                    eprintln!("tmux is unavailable; skipping isolated tmux test");
                    return None;
                }
                let directory =
                    PathBuf::from(format!("/tmp/tria-test-{}", crate::commands::new_id()));
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&directory)
                    .unwrap();
                let mut server = Self {
                    tmux: Tmux {
                        socket: directory.join("tmux.sock"),
                        pane: String::new(),
                    },
                    directory,
                    client: None,
                };
                server.tmux.pane = server
                    .tmux
                    .call(&[
                        "new-session",
                        "-d",
                        "-s",
                        "calling",
                        "-P",
                        "-F",
                        "#{pane_id}",
                        "sleep 60",
                    ])
                    .await
                    .unwrap();
                server.client = Some(
                    std::process::Command::new("tmux")
                        .arg("-S")
                        .arg(&server.tmux.socket)
                        .args(["-C", "attach-session", "-t", "calling"])
                        .stdin(Stdio::piped())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .unwrap(),
                );
                for _ in 0..100 {
                    if !server
                        .tmux
                        .call(&["list-clients", "-F", "#{client_name}"])
                        .await
                        .unwrap()
                        .is_empty()
                    {
                        return Some(server);
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                panic!("control client did not attach");
            }

            async fn target(&self) -> Tmux {
                self.tmux
                    .call(&["new-session", "-d", "-s", "target", "sleep 60"])
                    .await
                    .unwrap();
                let window = self
                    .tmux
                    .call(&[
                        "new-window",
                        "-d",
                        "-t",
                        "target",
                        "-P",
                        "-F",
                        "#{window_id}",
                        "sleep 60",
                    ])
                    .await
                    .unwrap();
                let pane = self
                    .tmux
                    .call(&[
                        "split-window",
                        "-d",
                        "-t",
                        &window,
                        "-P",
                        "-F",
                        "#{pane_id}",
                        "sleep 60",
                    ])
                    .await
                    .unwrap();
                Tmux {
                    socket: self.tmux.socket.clone(),
                    pane,
                }
            }
        }

        #[tokio::test]
        async fn a_live_client_receives_the_directory_before_tmux_switches_to_its_pane() {
            let Some(server) = Server::new().await else {
                return;
            };
            assert!(
                !reuse_at(&server.tmux, "/tmp", Some("http://one"))
                    .await
                    .unwrap()
            );
            let target = server.target().await;
            let (events, mut requests) = mpsc::unbounded_channel();
            let listener = listen_at(target.clone(), &server.directory, events, "http://one")
                .await
                .unwrap();
            assert!(
                !reuse_at(&server.tmux, "/tmp", Some("http://two"))
                    .await
                    .unwrap()
            );
            assert!(
                requests.try_recv().is_err(),
                "another server is not sent the directory"
            );

            let caller = server.tmux.clone();
            let rejected =
                tokio::spawn(async move { reuse_at(&caller, "/tmp", Some("http://one")).await });
            let AppEvent::OpenDirectory { reply, .. } =
                timeout(WAIT, requests.recv()).await.unwrap().unwrap()
            else {
                panic!("expected directory event")
            };
            reply.send(Err("unsent draft".into())).unwrap();
            assert!(
                rejected
                    .await
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("unsent draft")
            );
            assert_eq!(
                server
                    .tmux
                    .call(&["list-clients", "-F", "#{session_name}"])
                    .await
                    .unwrap(),
                "calling"
            );

            let caller = server.tmux.clone();
            let opened = tokio::spawn(async move {
                reuse_at(
                    &caller,
                    "/tmp/project with spaces-你好",
                    Some("http://one/"),
                )
                .await
            });
            let AppEvent::OpenDirectory { root, reply } =
                timeout(WAIT, requests.recv()).await.unwrap().unwrap()
            else {
                panic!("expected directory event")
            };
            assert_eq!(root, "/tmp/project with spaces-你好");
            assert_eq!(
                server
                    .tmux
                    .call(&["list-clients", "-F", "#{session_name}"])
                    .await
                    .unwrap(),
                "calling"
            );
            reply.send(Ok(())).unwrap();
            assert!(opened.await.unwrap().unwrap());
            let focused = server
                .tmux
                .call(&["list-clients", "-F", "#{session_name}\t#{pane_id}"])
                .await
                .unwrap();
            assert_eq!(focused, format!("target\t{}", target.pane));

            let socket = listener.socket.clone();
            drop(listener);
            assert!(!socket.exists());
            assert!(
                target
                    .call(&["show-options", "-pqv", "-t", &target.pane, OPTION])
                    .await
                    .unwrap()
                    .is_empty()
            );
            // A crashed instance can leave an advertisement and socket behind.
            let stale = server.directory.join("stale.sock");
            drop(UnixListener::bind(&stale).unwrap());
            fs::set_permissions(&stale, fs::Permissions::from_mode(0o600)).unwrap();
            target
                .call(&[
                    "set-option",
                    "-p",
                    "-t",
                    &target.pane,
                    OPTION,
                    stale.to_str().unwrap(),
                ])
                .await
                .unwrap();
            assert!(!reuse_at(&server.tmux, "/tmp", None).await.unwrap());
        }

        #[tokio::test]
        async fn the_current_session_is_preferred_over_another_session() {
            let Some(server) = Server::new().await else {
                return;
            };
            let distant = server.target().await;
            let (remote_events, mut remote_requests) = mpsc::unbounded_channel();
            let _remote = listen_at(distant, &server.directory, remote_events, "http://one")
                .await
                .unwrap();
            let (events, mut requests) = mpsc::unbounded_channel();
            let _local = listen_at(server.tmux.clone(), &server.directory, events, "http://one")
                .await
                .unwrap();
            let caller = server.tmux.clone();
            let opened =
                tokio::spawn(async move { reuse_at(&caller, "/tmp", Some("http://one")).await });
            let AppEvent::OpenDirectory { reply, .. } =
                timeout(WAIT, requests.recv()).await.unwrap().unwrap()
            else {
                panic!("expected directory event")
            };
            reply.send(Ok(())).unwrap();
            assert!(opened.await.unwrap().unwrap());
            assert!(remote_requests.try_recv().is_err());
            assert_eq!(
                server
                    .tmux
                    .call(&["list-clients", "-F", "#{session_name}"])
                    .await
                    .unwrap(),
                "calling"
            );
        }

        #[tokio::test]
        async fn malformed_or_oversized_requests_do_not_reach_the_ui() {
            let (events, mut requests) = mpsc::unbounded_channel();
            for bytes in [b"not-json\n".to_vec(), vec![b'x'; LIMIT as usize + 1]] {
                let (mut caller, receiver) = UnixStream::pair().unwrap();
                let task = tokio::spawn(receive(receiver, events.clone(), "http://one".into()));
                // The receiver can close as soon as it reaches the size limit.
                let _ = caller.write_all(&bytes).await;
                let _ = caller.shutdown().await;
                assert!(timeout(WAIT, task).await.unwrap().unwrap().is_err());
            }
            assert!(requests.try_recv().is_err());
        }
    }
}

//! Connection supervisor: owns the socket, the shell subscription, and the
//! subscription for the currently open thread. Reconnects with backoff and
//! resumes both streams from their last applied sequence.

use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::{
    auth,
    model::{Id, ServerConfig, ShellItem, ThreadItem},
    rpc::{RpcClient, Subscription},
};

const THREAD_TURN_LIMIT: u32 = 40;
/// How often a thread's stream is quietly asked for again before the trouble is worth
/// reporting. A first failure is ordinary — a thread the server has only just been told
/// to make, a socket that blinked — and asking again is all it takes.
const STREAM_ATTEMPTS: u32 = 3;
const MAX_BACKOFF: Duration = Duration::from_secs(30);

pub enum Request {
    OpenThread(Id),
    CloseThread,
    Dispatch {
        command: Value,
        reply: oneshot::Sender<Result<u64>>,
    },
    /// Load older turns for the open thread (windowed snapshot).
    LoadOlder {
        before_cursor: String,
    },
    /// Attach to a terminal: replaces any current attachment. The server opens the
    /// terminal when it does not exist, and restarts it when its shell has exited.
    Attach {
        thread_id: Id,
        terminal_id: String,
        cwd: String,
        cols: u16,
        rows: u16,
    },
    /// Drop the terminal attachment, which stops the server sending its output.
    Detach,
    /// Watch the checkout at `cwd`, or stop watching when it is `None`.
    WatchVcs {
        cwd: Option<String>,
    },
    /// Re-read the watched checkout, for when something outside the server changed it.
    RefreshVcs,
    /// Any other RPC, for the calls that are not orchestration commands.
    Call {
        tag: String,
        payload: Value,
        reply: oneshot::Sender<Result<Value>>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Connecting,
    Connected,
    Reconnecting { attempt: u32, error: String },
    Failed(String),
}

pub enum Update {
    Status(Status),
    Config(Box<ServerConfig>),
    Shell(ShellItem),
    Thread {
        thread_id: Id,
        item: ThreadItem,
    },
    /// A windowed snapshot of older turns requested with `LoadOlder`.
    OlderPage {
        thread_id: Id,
        snapshot: crate::model::ThreadDetailSnapshot,
    },
    /// The terminal list changed. Absent entirely when the token lacks `terminal:operate`.
    Terminals(crate::model::TerminalEvent),
    /// Output from the attached terminal.
    TerminalStream(crate::model::TerminalStreamEvent),
    /// The watched checkout's state.
    Vcs(crate::model::VcsEvent),
    /// The open thread's stream stopped and asking for it again did not work either,
    /// so nothing is listening to it now. Reopening the thread starts a new one, as does
    /// the next reconnection.
    ThreadStreamError {
        error: String,
    },
    Error(String),
}

/// Handle used by the UI to talk to the supervisor.
#[derive(Clone)]
pub struct Handle {
    tx: mpsc::UnboundedSender<Request>,
}

impl Handle {
    /// A handle with nothing behind it but the queue of requests it was given, for
    /// tests that care about what the UI asked the server for.
    #[cfg(test)]
    pub fn detached() -> (Self, mpsc::UnboundedReceiver<Request>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Handle { tx }, rx)
    }

    pub fn open_thread(&self, thread_id: &str) {
        let _ = self.tx.send(Request::OpenThread(thread_id.to_string()));
    }

    pub fn close_thread(&self) {
        let _ = self.tx.send(Request::CloseThread);
    }

    pub fn load_older(&self, before_cursor: &str) {
        let _ = self.tx.send(Request::LoadOlder {
            before_cursor: before_cursor.to_string(),
        });
    }

    /// Call any RPC and return its raw result.
    pub async fn call(&self, tag: &str, payload: Value) -> Result<Value> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::Call {
                tag: tag.to_string(),
                payload,
                reply,
            })
            .map_err(|_| anyhow!("connection supervisor stopped"))?;
        rx.await
            .map_err(|_| anyhow!("connection supervisor stopped"))?
    }

    /// Attach to a terminal. Output arrives as `Update::TerminalStream`.
    pub fn attach_terminal(
        &self,
        thread_id: &str,
        terminal_id: &str,
        cwd: &str,
        cols: u16,
        rows: u16,
    ) {
        let _ = self.tx.send(Request::Attach {
            thread_id: thread_id.to_string(),
            terminal_id: terminal_id.to_string(),
            cwd: cwd.to_string(),
            cols,
            rows,
        });
    }

    pub fn detach_terminal(&self) {
        let _ = self.tx.send(Request::Detach);
    }

    /// Follow the branch of a checkout. One at a time: this is for the open thread.
    pub fn watch_vcs(&self, cwd: Option<String>) {
        let _ = self.tx.send(Request::WatchVcs { cwd });
    }

    /// Ask the server to re-read the watched checkout. Its own cache can be behind
    /// what is on disk, and the result reaches the watch as an update.
    pub fn refresh_vcs(&self) {
        let _ = self.tx.send(Request::RefreshVcs);
    }

    pub async fn dispatch(&self, command: Value) -> Result<u64> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::Dispatch { command, reply })
            .map_err(|_| anyhow!("connection supervisor stopped"))?;
        rx.await
            .map_err(|_| anyhow!("connection supervisor stopped"))?
    }
}

pub fn spawn(origin: String, token: String) -> (Handle, mpsc::UnboundedReceiver<Update>) {
    let (req_tx, req_rx) = mpsc::unbounded_channel();
    let (upd_tx, upd_rx) = mpsc::unbounded_channel();
    tokio::spawn(run(origin, token, req_rx, upd_tx));
    (Handle { tx: req_tx }, upd_rx)
}

struct OpenThread {
    id: Id,
    /// Last sequence forwarded to the UI, for resume.
    last_sequence: Option<u64>,
    subscription: Option<Subscription>,
    /// How many times in a row the stream has been asked for again without a single
    /// item coming back. Reset by anything the thread says.
    attempts: u32,
}

async fn run(
    origin: String,
    token: String,
    mut requests: mpsc::UnboundedReceiver<Request>,
    updates: mpsc::UnboundedSender<Update>,
) {
    let mut shell_sequence: Option<u64> = None;
    let mut open: Option<OpenThread> = None;
    // Kept across reconnects so the watch resumes with the socket.
    let mut watched_cwd: Option<String> = None;
    let mut attempt: u32 = 0;
    let mut pagination = false;

    loop {
        if attempt == 0 {
            let _ = updates.send(Update::Status(Status::Connecting));
        }
        let client = match connect(&origin, &token).await {
            Ok(client) => client,
            Err(err) => {
                let message = err.to_string();
                if message.contains("401") || message.contains("rejected") {
                    let _ = updates.send(Update::Status(Status::Failed(message)));
                    // Auth failures do not resolve by retrying. Keep serving dispatch
                    // requests with an error so the UI can report it.
                    while let Some(request) = requests.recv().await {
                        if let Request::Dispatch { reply, .. } = request {
                            let _ = reply.send(Err(anyhow!("not connected")));
                        }
                    }
                    return;
                }
                attempt += 1;
                let _ = updates.send(Update::Status(Status::Reconnecting {
                    attempt,
                    error: message,
                }));
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
        };

        // Config first: it tells us whether pagination and completion markers are supported.
        match client
            .call::<ServerConfig>("server.getConfig", json!({}))
            .await
        {
            Ok(config) => {
                pagination = config.thread_snapshot_pagination;
                let _ = updates.send(Update::Config(Box::new(config)));
            }
            Err(err) => {
                let _ = updates.send(Update::Error(format!("server.getConfig: {err}")));
            }
        }

        let mut shell = match subscribe_shell(&client, shell_sequence).await {
            Ok(sub) => sub,
            Err(err) => {
                attempt += 1;
                let _ = updates.send(Update::Status(Status::Reconnecting {
                    attempt,
                    error: err.to_string(),
                }));
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
        };
        // Terminals need the `terminal:operate` scope. A token without it still works;
        // the terminal list is simply never populated.
        let mut terminals = match client
            .subscribe("subscribeTerminalMetadata", json!({}))
            .await
        {
            Ok(sub) => Some(sub),
            Err(err) => {
                tracing::info!(%err, "terminal metadata unavailable");
                None
            }
        };
        // Attachments belong to one socket; a reconnect drops it and the UI re-attaches.
        let mut attached: Option<Subscription> = None;
        let mut vcs: Option<Subscription> = None;
        if let Some(cwd) = watched_cwd.clone() {
            vcs = subscribe_vcs(&client, &cwd).await;
            refresh_vcs(&client, cwd);
        }
        if let Some(open) = open.as_mut() {
            open.subscription = subscribe_thread(&client, &open.id, open.last_sequence, pagination)
                .await
                .ok();
        }
        attempt = 0;
        let _ = updates.send(Update::Status(Status::Connected));

        // Serve until the socket dies.
        let closed = client.closed();
        tokio::pin!(closed);
        loop {
            let thread_next = async {
                match open.as_mut().and_then(|o| o.subscription.as_mut()) {
                    Some(sub) => sub.next().await,
                    None => std::future::pending().await,
                }
            };
            let terminal_next = async {
                match terminals.as_mut() {
                    Some(sub) => sub.next().await,
                    None => std::future::pending().await,
                }
            };
            let attached_next = async {
                match attached.as_mut() {
                    Some(sub) => sub.next().await,
                    None => std::future::pending().await,
                }
            };
            let vcs_next = async {
                match vcs.as_mut() {
                    Some(sub) => sub.next().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = &mut closed => break,
                request = requests.recv() => {
                    let Some(request) = request else { return };
                    match request {
                        Request::OpenThread(id) => {
                            if open.as_ref().is_some_and(|o| o.id == id && o.subscription.is_some()) {
                                continue;
                            }
                            let subscription = subscribe_thread(&client, &id, None, pagination).await.ok();
                            open = Some(OpenThread { id, last_sequence: None, subscription, attempts: 0 });
                        }
                        Request::CloseThread => open = None,
                        Request::Attach { thread_id, terminal_id, cwd, cols, rows } => {
                            let payload = json!({
                                "threadId": thread_id,
                                "terminalId": terminal_id,
                                "cwd": cwd,
                                "cols": cols,
                                "rows": rows,
                                "restartIfNotRunning": true,
                            });
                            match client.subscribe("terminal.attach", payload).await {
                                Ok(sub) => attached = Some(sub),
                                Err(err) => {
                                    attached = None;
                                    let _ = updates.send(Update::Error(err.to_string()));
                                }
                            }
                        }
                        Request::Detach => attached = None,
                        Request::WatchVcs { cwd } => {
                            watched_cwd = cwd.clone();
                            vcs = match cwd {
                                Some(cwd) => {
                                    let sub = subscribe_vcs(&client, &cwd).await;
                                    refresh_vcs(&client, cwd);
                                    sub
                                }
                                None => None,
                            };
                        }
                        Request::RefreshVcs => {
                            if let Some(cwd) = watched_cwd.clone() {
                                refresh_vcs(&client, cwd);
                            }
                        }
                        Request::LoadOlder { before_cursor } => {
                            if let Some(o) = open.as_ref() {
                                let payload = json!({
                                    "threadId": o.id,
                                    "turnLimit": THREAD_TURN_LIMIT,
                                    "beforeCursor": before_cursor,
                                });
                                // Older turns come as a one-shot windowed subscription; take its snapshot only.
                                match client.subscribe("orchestration.subscribeThread", payload).await {
                                    Ok(mut sub) => {
                                        if let Some(Ok(item)) = sub.next().await
                                            && let Ok(ThreadItem::Snapshot { snapshot }) = serde_json::from_value::<ThreadItem>(item)
                                        {
                                            let _ = updates.send(Update::OlderPage { thread_id: o.id.clone(), snapshot });
                                        }
                                    }
                                    Err(err) => { let _ = updates.send(Update::Error(err.to_string())); }
                                }
                            }
                        }
                        Request::Call { tag, payload, reply } => {
                            let _ = reply.send(client.call::<Value>(&tag, payload).await);
                        }
                        Request::Dispatch { command, reply } => {
                            let result = client
                                .call::<Value>("orchestration.dispatchCommand", command)
                                .await
                                .map(|v| v.get("sequence").and_then(Value::as_u64).unwrap_or(0));
                            let _ = reply.send(result);
                        }
                    }
                }
                item = shell.next() => {
                    match item {
                        Some(Ok(value)) => {
                            match serde_json::from_value::<ShellItem>(value) {
                                Ok(item) => {
                                    match &item {
                                        ShellItem::Snapshot { snapshot } => shell_sequence = Some(snapshot.snapshot_sequence),
                                        ShellItem::ProjectUpserted { sequence, .. }
                                        | ShellItem::ProjectRemoved { sequence, .. }
                                        | ShellItem::ThreadUpserted { sequence, .. }
                                        | ShellItem::ThreadRemoved { sequence, .. } => shell_sequence = Some(*sequence),
                                        _ => {}
                                    }
                                    let _ = updates.send(Update::Shell(item));
                                }
                                Err(err) => tracing::warn!(?err, "undecodable shell item"),
                            }
                        }
                        Some(Err(err)) => {
                            let _ = updates.send(Update::Error(format!("shell stream: {err}")));
                            match subscribe_shell(&client, shell_sequence).await {
                                Ok(sub) => shell = sub,
                                Err(_) => break,
                            }
                        }
                        None => {
                            match subscribe_shell(&client, shell_sequence).await {
                                Ok(sub) => shell = sub,
                                Err(_) => break,
                            }
                        }
                    }
                }
                item = vcs_next => {
                    match item {
                        Some(Ok(value)) => match serde_json::from_value::<crate::model::VcsEvent>(value) {
                            Ok(event) => { let _ = updates.send(Update::Vcs(event)); }
                            Err(err) => tracing::warn!(?err, "undecodable vcs event"),
                        },
                        Some(Err(err)) => {
                            tracing::info!(%err, "vcs status stream ended");
                            vcs = None;
                        }
                        None => vcs = None,
                    }
                }
                item = attached_next => {
                    match item {
                        Some(Ok(value)) => match serde_json::from_value::<crate::model::TerminalStreamEvent>(value) {
                            Ok(event) => { let _ = updates.send(Update::TerminalStream(event)); }
                            Err(err) => tracing::warn!(?err, "undecodable terminal stream event"),
                        },
                        Some(Err(err)) => {
                            attached = None;
                            let _ = updates.send(Update::Error(format!("terminal detached: {err}")));
                        }
                        None => attached = None,
                    }
                }
                item = terminal_next => {
                    match item {
                        Some(Ok(value)) => match serde_json::from_value::<crate::model::TerminalEvent>(value) {
                            Ok(event) => { let _ = updates.send(Update::Terminals(event)); }
                            Err(err) => tracing::warn!(?err, "undecodable terminal event"),
                        },
                        // The stream is a convenience; drop it rather than fight for it.
                        Some(Err(err)) => {
                            tracing::warn!(%err, "terminal metadata stream failed");
                            terminals = None;
                        }
                        None => terminals = None,
                    }
                }
                item = thread_next => {
                    let Some(o) = open.as_mut() else { continue };
                    match item {
                        Some(Ok(value)) => {
                            o.attempts = 0;
                            forward_thread_item(&updates, &o.id, value, &mut o.last_sequence);
                        }
                        // A stream that ends or fails is answered the same way, by asking
                        // for it again from where it got to. One that comes back is a
                        // moment of trouble nobody needs told about: it resumes mid-turn
                        // without a mark on the screen. One that will not is reported and
                        // left alone, rather than asked over and over in silence.
                        item => {
                            let failed = match item {
                                Some(Err(err)) => err.to_string(),
                                _ => "the thread's stream ended".to_string(),
                            };
                            o.attempts += 1;
                            if o.attempts >= STREAM_ATTEMPTS {
                                o.subscription = None;
                                let _ = updates.send(Update::ThreadStreamError { error: failed });
                            } else {
                                // Only after the first, which is the one a thread being
                                // made a moment ago answers on its own.
                                if o.attempts > 1 {
                                    tokio::time::sleep(Duration::from_millis(
                                        200 * o.attempts as u64,
                                    ))
                                    .await;
                                }
                                match subscribe_thread(&client, &o.id, o.last_sequence, pagination).await {
                                    Ok(again) => o.subscription = Some(again),
                                    // Asking was refused outright, which the next ask is
                                    // not going to change.
                                    Err(err) => {
                                        o.subscription = None;
                                        let _ = updates.send(Update::ThreadStreamError { error: err.to_string() });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        attempt += 1;
        let _ = updates.send(Update::Status(Status::Reconnecting {
            attempt,
            error: "connection lost".into(),
        }));
        if let Some(o) = open.as_mut() {
            o.subscription = None;
            // A new connection is a fresh start: what the old one could not keep up says
            // nothing about what this one will manage.
            o.attempts = 0;
        }
        tokio::time::sleep(backoff(attempt)).await;
    }
}

fn forward_thread_item(
    updates: &mpsc::UnboundedSender<Update>,
    thread_id: &str,
    value: Value,
    last_sequence: &mut Option<u64>,
) {
    match serde_json::from_value::<ThreadItem>(value) {
        Ok(item) => {
            match &item {
                ThreadItem::Snapshot { snapshot } => {
                    *last_sequence = Some(snapshot.snapshot_sequence)
                }
                ThreadItem::Event { event } => *last_sequence = Some(event.sequence),
                _ => {}
            }
            let _ = updates.send(Update::Thread {
                thread_id: thread_id.to_string(),
                item,
            });
        }
        Err(err) => tracing::warn!(?err, "undecodable thread item"),
    }
}

async fn connect(origin: &str, token: &str) -> Result<RpcClient> {
    let ticket = auth::websocket_ticket(origin, token).await?;
    RpcClient::connect(origin, &ticket).await
}

async fn subscribe_shell(client: &RpcClient, after: Option<u64>) -> Result<Subscription> {
    let mut payload = json!({ "requestCompletionMarker": true });
    if let Some(after) = after {
        payload["afterSequence"] = json!(after);
    }
    client
        .subscribe("orchestration.subscribeShell", payload)
        .await
}

/// Re-read a checkout in the background. The server caches its git status and can be
/// behind the disk; the refreshed result is broadcast to the watch, so the reply here
/// is of no interest.
/// Watch a checkout. Not every directory is one, and the stream says so itself, so a
/// refusal is the UI showing no branch rather than anything to report — but a header
/// that stays empty is a question, and this is where the answer is.
async fn subscribe_vcs(client: &RpcClient, cwd: &str) -> Option<Subscription> {
    match client
        .subscribe("subscribeVcsStatus", json!({ "cwd": cwd }))
        .await
    {
        Ok(sub) => Some(sub),
        Err(err) => {
            tracing::info!(%err, %cwd, "not watching this checkout");
            None
        }
    }
}

fn refresh_vcs(client: &RpcClient, cwd: String) {
    let client = client.clone();
    tokio::spawn(async move {
        let _: Result<Value> = client
            .call("vcs.refreshStatus", json!({ "cwd": cwd }))
            .await;
    });
}

async fn subscribe_thread(
    client: &RpcClient,
    thread_id: &str,
    after: Option<u64>,
    pagination: bool,
) -> Result<Subscription> {
    let mut payload = json!({ "threadId": thread_id, "requestCompletionMarker": true });
    if let Some(after) = after {
        payload["afterSequence"] = json!(after);
    } else if pagination {
        payload["turnLimit"] = json!(THREAD_TURN_LIMIT);
    }
    client
        .subscribe("orchestration.subscribeThread", payload)
        .await
}

fn backoff(attempt: u32) -> Duration {
    let secs = 2u64.saturating_pow(attempt.min(5));
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

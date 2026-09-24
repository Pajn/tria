//! Connection supervisor: owns the socket, the shell subscription, and the
//! subscription for the currently open thread. Reconnects with backoff and
//! resumes both streams from their last applied sequence.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

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
    /// Subscribe to the open thread again from nothing, for when what tria holds of it
    /// no longer follows from its events: a revert drops turns, and the stream says
    /// only that it happened.
    RefreshThread(Id),
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
    /// Follow these checkouts and no others. A whole set rather than one directory:
    /// the sidebar says where every thread's work stands, and threads sit in as many
    /// checkouts as they have worktrees between them.
    WatchVcs {
        cwds: Vec<String>,
    },
    /// Re-read the followed checkouts, for when something outside the server changed
    /// one of them.
    RefreshVcs,
    /// Throw away the connection and make another. What `:reconnect` sends: the way
    /// out of a connection that has settled into refusing, and a way to start again
    /// with one that is up but has stopped being any use.
    Reconnect,
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
    /// A checkout's status, with the directory it describes. Several are followed at
    /// once and the watches are asked for through a queue, so a status can outlive the
    /// watch that asked for it, and only the directory tells one checkout's from
    /// another's.
    Vcs {
        cwd: String,
        event: crate::model::VcsEvent,
    },
    /// The open thread's stream stopped and asking for it again did not work either,
    /// so nothing is listening to it now. Reopening the thread starts a new one, as does
    /// the next reconnection. Named, because a thread left in the meantime is no longer
    /// the one on the screen and its trouble is nobody's to see.
    ThreadStreamError {
        thread_id: Id,
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

    pub fn refresh_thread(&self, thread_id: &str) {
        let _ = self.tx.send(Request::RefreshThread(thread_id.to_string()));
    }

    pub fn close_thread(&self) {
        let _ = self.tx.send(Request::CloseThread);
    }

    /// Drop the connection and make a new one, whatever state the old one was in.
    pub fn reconnect(&self) {
        let _ = self.tx.send(Request::Reconnect);
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

    /// Follow these checkouts and no others. Directories that drop out of the set are
    /// let go, and ones new to it are subscribed to.
    pub fn watch_vcs(&self, cwds: Vec<String>) {
        let _ = self.tx.send(Request::WatchVcs { cwds });
    }

    /// Ask the server to re-read the followed checkouts. Its own cache can be behind
    /// what is on disk, and the results reach the watches as updates.
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
    let mut watched_cwds: Vec<String> = Vec::new();
    // A task per followed checkout, forwarding its statuses. Keyed by the directory,
    // which is the only thing that tells one checkout's news from another's.
    let mut following: HashMap<String, JoinHandle<()>> = HashMap::new();
    let mut attempt: u32 = 0;
    let mut pagination = false;

    loop {
        if attempt == 0 {
            let _ = updates.send(Update::Status(Status::Connecting));
        }
        let mut forced = false;
        let client = match connect(&origin, &token).await {
            Ok(client) => client,
            Err(err) => {
                // Told apart by the status the server sent rather than by what the
                // message happens to read like: a 503 from a server coming up used to
                // be read as a refused token, and the client gave up on a server that
                // was seconds from answering.
                let settled = err
                    .downcast_ref::<auth::HttpStatus>()
                    .filter(|refusal| refusal.is_settled());
                if let Some(refusal) = settled {
                    let _ = updates.send(Update::Status(Status::Failed(refusal.report())));
                    // Nothing here resolves by asking again, so wait to be told to.
                    // Requests still get an answer, because a UI left with none of them
                    // coming back is a UI that has stopped rather than one that is
                    // saying what is wrong.
                    if wait_for_reconnect(&mut requests, &mut open).await {
                        attempt = 0;
                        continue;
                    }
                    return;
                }
                attempt += 1;
                let _ = updates.send(Update::Status(Status::Reconnecting {
                    attempt,
                    error: err.to_string(),
                }));
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
        };

        // Commands go out one after another, in the order they were asked for — a turn
        // on a thread follows the command that made it — but not from this loop: a
        // command the server takes its time over would hold every stream here still
        // until it answered. The worker ends with the connection, once this sender
        // is dropped and what it was given has gone out.
        let dispatches = dispatcher(&client);

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
        // Subscriptions belong to the socket that made them, so the watches are dropped
        // and asked for again on the new one.
        for (_, task) in following.drain() {
            task.abort();
        }
        follow_checkouts(&client, &updates, &mut following, &watched_cwds);
        if let Some(open) = open.as_mut() {
            // A new socket with no stream on it is the same silence as a stream that
            // stopped, and is worth the same word: the conversation stops moving either
            // way, and the reconnection is what looked like the fix.
            match subscribe_thread(&client, &open.id, open.last_sequence, pagination).await {
                Ok(subscription) => open.subscription = Some(subscription),
                Err(err) => {
                    open.subscription = None;
                    let _ = updates.send(Update::ThreadStreamError {
                        thread_id: open.id.clone(),
                        error: err.to_string(),
                    });
                }
            }
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
                        Request::RefreshThread(id) => {
                            if open.as_ref().is_some_and(|o| o.id == id) {
                                let subscription = subscribe_thread(&client, &id, None, pagination).await.ok();
                                open = Some(OpenThread { id, last_sequence: None, subscription, attempts: 0 });
                            }
                        }
                        Request::CloseThread => open = None,
                        Request::Reconnect => {
                            forced = true;
                            break;
                        }
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
                        Request::WatchVcs { cwds } => {
                            watched_cwds = cwds;
                            follow_checkouts(&client, &updates, &mut following, &watched_cwds);
                        }
                        Request::RefreshVcs => {
                            for cwd in &watched_cwds {
                                refresh_vcs(&client, cwd.clone());
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
                        // Answered off this loop: a call can take as long as the work
                        // behind it — removing a worktree is deleting a directory — and
                        // the streams have to keep moving while it does. Calls do not
                        // depend on one another, so they go out side by side.
                        Request::Call { tag, payload, reply } => {
                            let client = client.clone();
                            tokio::spawn(async move {
                                let _ = reply.send(client.call::<Value>(&tag, payload).await);
                            });
                        }
                        Request::Dispatch { command, reply } => {
                            let _ = dispatches.send((command, reply));
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
                                let _ = updates.send(Update::ThreadStreamError {
                                    thread_id: o.id.clone(),
                                    error: failed,
                                });
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
                                        let _ = updates.send(Update::ThreadStreamError {
                                            thread_id: o.id.clone(),
                                            error: err.to_string(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Some(o) = open.as_mut() {
            o.subscription = None;
            // A new connection is a fresh start: what the old one could not keep up says
            // nothing about what this one will manage.
            o.attempts = 0;
        }
        // A connection thrown away on purpose is not a connection that failed: it is
        // asked for again at once, and nothing reports trouble that nobody had.
        if forced {
            attempt = 0;
            continue;
        }
        attempt += 1;
        let _ = updates.send(Update::Status(Status::Reconnecting {
            attempt,
            error: "connection lost".into(),
        }));
        tokio::time::sleep(backoff(attempt)).await;
    }
}

/// Sit out a connection the server has settled into refusing, until something asks for
/// another try. `false` means the UI has gone and there is nothing left to serve.
///
/// Requests are still answered while waiting — with a refusal, but answered. The thread
/// being opened is remembered too, so that a connection which does come back comes back
/// to the thread on the screen rather than to the one that was open when it broke.
async fn wait_for_reconnect(
    requests: &mut mpsc::UnboundedReceiver<Request>,
    open: &mut Option<OpenThread>,
) -> bool {
    while let Some(request) = requests.recv().await {
        match request {
            Request::Reconnect => return true,
            Request::OpenThread(id) => {
                *open = Some(OpenThread {
                    id,
                    last_sequence: None,
                    subscription: None,
                    attempts: 0,
                })
            }
            Request::CloseThread => *open = None,
            Request::Dispatch { reply, .. } => {
                let _ = reply.send(Err(anyhow!("not connected")));
            }
            Request::Call { reply, .. } => {
                let _ = reply.send(Err(anyhow!("not connected")));
            }
            _ => {}
        }
    }
    false
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
/// Follow exactly `cwds`: let go of the checkouts that have left the set, and take up
/// the ones new to it. A task each rather than one arm of the select, because the set
/// changes as threads come and go and a task can simply be dropped.
fn follow_checkouts(
    client: &RpcClient,
    updates: &mpsc::UnboundedSender<Update>,
    following: &mut HashMap<String, JoinHandle<()>>,
    cwds: &[String],
) {
    following.retain(|cwd, task| {
        let keep = cwds.contains(cwd);
        if !keep {
            task.abort();
        }
        keep
    });
    for cwd in cwds {
        if following.contains_key(cwd) {
            continue;
        }
        let client = client.clone();
        let updates = updates.clone();
        let cwd = cwd.clone();
        let task = tokio::spawn({
            let cwd = cwd.clone();
            async move {
                let Some(mut sub) = subscribe_vcs(&client, &cwd).await else {
                    return;
                };
                // The server answers a subscription from its own cache, which can be behind
                // the disk, so the first real reading is asked for.
                refresh_vcs(&client, cwd.clone());
                while let Some(item) = sub.next().await {
                    let value = match item {
                        Ok(value) => value,
                        Err(err) => {
                            tracing::info!(%err, %cwd, "vcs status stream ended");
                            return;
                        }
                    };
                    match serde_json::from_value::<crate::model::VcsEvent>(value) {
                        Ok(event) => {
                            let update = Update::Vcs {
                                cwd: cwd.clone(),
                                event,
                            };
                            if updates.send(update).is_err() {
                                return;
                            }
                        }
                        Err(err) => tracing::warn!(?err, "undecodable vcs event"),
                    }
                }
            }
        });
        following.insert(cwd, task);
    }
}

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

/// A command and where its answer goes.
type Dispatched = (Value, oneshot::Sender<Result<u64>>);

/// Send commands to the server one at a time, in the order they arrive, each once the
/// one before it has been answered.
fn dispatcher(client: &RpcClient) -> mpsc::UnboundedSender<Dispatched> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Dispatched>();
    let client = client.clone();
    tokio::spawn(async move {
        while let Some((command, reply)) = rx.recv().await {
            let result = client
                .call::<Value>("orchestration.dispatchCommand", command)
                .await
                .map(|v| v.get("sequence").and_then(Value::as_u64).unwrap_or(0));
            let _ = reply.send(result);
        }
    });
    tx
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    /// A server that hands each request it reads to the test, and sends back whatever
    /// the test gives it to.
    async fn fake_server() -> (
        String,
        mpsc::UnboundedReceiver<Value>,
        mpsc::UnboundedSender<Value>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (heard, requests) = mpsc::unbounded_channel();
        let (answers, mut answer_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            loop {
                tokio::select! {
                    frame = socket.next() => {
                        let Some(Ok(Message::Text(text))) = frame else { break };
                        let frame: Value = serde_json::from_str(&text).unwrap();
                        if frame["_tag"] == "Request" {
                            let _ = heard.send(frame);
                        }
                    }
                    answer = answer_rx.recv() => {
                        let Some(answer) = answer else { break };
                        socket.send(Message::Text(answer.to_string().into())).await.unwrap();
                    }
                }
            }
        });
        (origin, requests, answers)
    }

    fn answer(request: &Value, sequence: u64) -> Value {
        json!({
            "_tag": "Exit",
            "requestId": request["id"],
            "exit": {"_tag": "Success", "value": {"sequence": sequence}},
        })
    }

    /// Commands leave in the order they were given, each once the one before it has
    /// been answered — a turn must not reach the server ahead of the thread it is on.
    #[tokio::test]
    async fn commands_go_out_one_after_another() {
        let (origin, mut requests, answers) = fake_server().await;
        let client = RpcClient::connect(&origin, "ticket").await.unwrap();
        let dispatches = dispatcher(&client);
        let (first, first_answer) = oneshot::channel();
        let (second, second_answer) = oneshot::channel();
        dispatches.send((json!({"n": 1}), first)).unwrap();
        dispatches.send((json!({"n": 2}), second)).unwrap();

        let one = requests.recv().await.unwrap();
        assert_eq!(one["tag"], "orchestration.dispatchCommand");
        assert_eq!(one["payload"]["n"], 1);
        let early = tokio::time::timeout(Duration::from_millis(100), requests.recv()).await;
        assert!(
            early.is_err(),
            "the second went out before the first was answered"
        );

        answers.send(answer(&one, 7)).unwrap();
        assert_eq!(first_answer.await.unwrap().unwrap(), 7);
        let two = requests.recv().await.unwrap();
        assert_eq!(two["payload"]["n"], 2);
        answers.send(answer(&two, 8)).unwrap();
        assert_eq!(second_answer.await.unwrap().unwrap(), 8);
    }
}

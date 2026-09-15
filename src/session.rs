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
const MAX_BACKOFF: Duration = Duration::from_secs(30);

pub enum Request {
    OpenThread(Id),
    CloseThread,
    Dispatch { command: Value, reply: oneshot::Sender<Result<u64>> },
    /// Load older turns for the open thread (windowed snapshot).
    LoadOlder { before_cursor: String },
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
    Thread { thread_id: Id, item: ThreadItem },
    /// A windowed snapshot of older turns requested with `LoadOlder`.
    OlderPage { thread_id: Id, snapshot: crate::model::ThreadDetailSnapshot },
    /// The open thread's stream failed; the supervisor resubscribes on its own.
    ThreadStreamError { thread_id: Id, error: String },
    Error(String),
}

/// Handle used by the UI to talk to the supervisor.
#[derive(Clone)]
pub struct Handle {
    tx: mpsc::UnboundedSender<Request>,
}

impl Handle {
    pub fn open_thread(&self, thread_id: &str) {
        let _ = self.tx.send(Request::OpenThread(thread_id.to_string()));
    }

    pub fn close_thread(&self) {
        let _ = self.tx.send(Request::CloseThread);
    }

    pub fn load_older(&self, before_cursor: &str) {
        let _ = self.tx.send(Request::LoadOlder { before_cursor: before_cursor.to_string() });
    }

    pub async fn dispatch(&self, command: Value) -> Result<u64> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::Dispatch { command, reply })
            .map_err(|_| anyhow!("connection supervisor stopped"))?;
        rx.await.map_err(|_| anyhow!("connection supervisor stopped"))?
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
}

async fn run(
    origin: String,
    token: String,
    mut requests: mpsc::UnboundedReceiver<Request>,
    updates: mpsc::UnboundedSender<Update>,
) {
    let mut shell_sequence: Option<u64> = None;
    let mut open: Option<OpenThread> = None;
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
                let _ = updates.send(Update::Status(Status::Reconnecting { attempt, error: message }));
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
        };

        // Config first: it tells us whether pagination and completion markers are supported.
        match client.call::<ServerConfig>("server.getConfig", json!({})).await {
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
                let _ = updates.send(Update::Status(Status::Reconnecting { attempt, error: err.to_string() }));
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
        };
        if let Some(open) = open.as_mut() {
            open.subscription = subscribe_thread(&client, &open.id, open.last_sequence, pagination).await.ok();
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
                            open = Some(OpenThread { id, last_sequence: None, subscription });
                        }
                        Request::CloseThread => open = None,
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
                                        if let Some(Ok(item)) = sub.next().await {
                                            if let Ok(ThreadItem::Snapshot { snapshot }) = serde_json::from_value::<ThreadItem>(item) {
                                                let _ = updates.send(Update::OlderPage { thread_id: o.id.clone(), snapshot });
                                            }
                                        }
                                    }
                                    Err(err) => { let _ = updates.send(Update::Error(err.to_string())); }
                                }
                            }
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
                item = thread_next => {
                    let Some(o) = open.as_mut() else { continue };
                    match item {
                        Some(Ok(value)) => forward_thread_item(&updates, &o.id, value, &mut o.last_sequence),
                        Some(Err(err)) => {
                            let _ = updates.send(Update::ThreadStreamError { thread_id: o.id.clone(), error: err.to_string() });
                            o.subscription = subscribe_thread(&client, &o.id, o.last_sequence, pagination).await.ok();
                        }
                        None => {
                            o.subscription = subscribe_thread(&client, &o.id, o.last_sequence, pagination).await.ok();
                        }
                    }
                }
            }
        }

        attempt += 1;
        let _ = updates.send(Update::Status(Status::Reconnecting { attempt, error: "connection lost".into() }));
        if let Some(o) = open.as_mut() {
            o.subscription = None;
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
                ThreadItem::Snapshot { snapshot } => *last_sequence = Some(snapshot.snapshot_sequence),
                ThreadItem::Event { event } => *last_sequence = Some(event.sequence),
                _ => {}
            }
            let _ = updates.send(Update::Thread { thread_id: thread_id.to_string(), item });
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
    client.subscribe("orchestration.subscribeShell", payload).await
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
    client.subscribe("orchestration.subscribeThread", payload).await
}

fn backoff(attempt: u32) -> Duration {
    let secs = 2u64.saturating_pow(attempt.min(5));
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

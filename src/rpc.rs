//! WebSocket RPC client. One task owns the socket, acks every stream chunk
//! immediately, and routes responses to pending calls and subscriptions.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::wire::{self, Exit, FromClient, FromServer};

const PING_INTERVAL: Duration = Duration::from_secs(5);
const PONG_TIMEOUT: Duration = Duration::from_secs(20);

enum Pending {
    Call(oneshot::Sender<Result<Value>>),
    Stream(mpsc::UnboundedSender<Result<Value>>),
}

#[derive(Default)]
struct Shared {
    pending: Mutex<HashMap<u64, Pending>>,
}

/// A connected RPC session. Dropping it closes the socket.
#[derive(Clone)]
pub struct RpcClient {
    outbound: mpsc::UnboundedSender<FromClient>,
    shared: Arc<Shared>,
    next_id: Arc<AtomicU64>,
    closed: Arc<tokio::sync::Notify>,
}

/// Items from a streaming RPC. Ends with `None` on a clean exit, `Some(Err)` on failure.
pub struct Subscription {
    id: u64,
    rx: mpsc::UnboundedReceiver<Result<Value>>,
    outbound: mpsc::UnboundedSender<FromClient>,
}

impl Subscription {
    pub async fn next(&mut self) -> Option<Result<Value>> {
        self.rx.recv().await
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.outbound.send(FromClient::Interrupt {
            request_id: self.id,
        });
    }
}

impl RpcClient {
    pub async fn connect(origin: &str, ticket: &str) -> Result<Self> {
        let mut url = url::Url::parse(origin).context("invalid server origin")?;
        let scheme = match url.scheme() {
            "http" => "ws",
            "https" => "wss",
            other => bail!("unsupported origin scheme {other}"),
        };
        url.set_scheme(scheme)
            .map_err(|_| anyhow!("cannot set websocket scheme"))?;
        url.set_path("/ws");
        url.query_pairs_mut()
            .append_pair("wsTicket", ticket)
            .append_pair("clientSurface", "web")
            .append_pair("clientDeviceType", "desktop")
            .append_pair("clientOs", std::env::consts::OS)
            .append_pair("connectionMethod", "direct")
            .append_pair("clientAppVersion", env!("CARGO_PKG_VERSION"));

        let (socket, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .context("websocket connect failed")?;
        let (mut sink, mut stream) = socket.split();
        let (outbound, mut outbound_rx) = mpsc::unbounded_channel::<FromClient>();
        let shared = Arc::new(Shared::default());
        let closed = Arc::new(tokio::sync::Notify::new());

        let writer_shared = shared.clone();
        let writer_closed = closed.clone();
        tokio::spawn(async move {
            while let Some(message) = outbound_rx.recv().await {
                let text = match serde_json::to_string(&message) {
                    Ok(text) => text,
                    Err(err) => {
                        tracing::error!(?err, "encoding outbound frame");
                        continue;
                    }
                };
                if let Err(err) = sink.send(Message::Text(text.into())).await {
                    tracing::warn!(?err, "websocket send failed");
                    break;
                }
            }
            let _ = sink.close().await;
            fail_all(&writer_shared, "connection closed");
            writer_closed.notify_waiters();
        });

        let reader_outbound = outbound.clone();
        let reader_shared = shared.clone();
        let reader_closed = closed.clone();
        tokio::spawn(async move {
            let mut ping = tokio::time::interval(PING_INTERVAL);
            let mut last_pong = tokio::time::Instant::now();
            loop {
                tokio::select! {
                    _ = ping.tick() => {
                        if last_pong.elapsed() > PONG_TIMEOUT {
                            tracing::warn!("no pong within timeout; closing");
                            break;
                        }
                        if reader_outbound.send(FromClient::Ping).is_err() {
                            break;
                        }
                    }
                    frame = stream.next() => {
                        let Some(frame) = frame else { break };
                        let text = match frame {
                            Ok(Message::Text(text)) => text,
                            Ok(Message::Binary(bytes)) => match String::from_utf8(bytes.to_vec()) {
                                Ok(text) => text.into(),
                                Err(_) => continue,
                            },
                            Ok(Message::Close(_)) => break,
                            Ok(_) => continue,
                            Err(err) => {
                                tracing::warn!(?err, "websocket read failed");
                                break;
                            }
                        };
                        let decoded: Vec<FromServer> = match serde_json::from_str::<Value>(&text) {
                            Ok(Value::Array(items)) => items
                                .into_iter()
                                .filter_map(|item| serde_json::from_value(item).ok())
                                .collect(),
                            Ok(item) => match serde_json::from_value(item) {
                                Ok(message) => vec![message],
                                Err(err) => {
                                    tracing::warn!(?err, frame = %text.chars().take(300).collect::<String>(), "undecodable frame");
                                    continue;
                                }
                            },
                            Err(err) => {
                                tracing::warn!(?err, "non-JSON frame");
                                continue;
                            }
                        };
                        for message in decoded {
                            if let FromServer::Pong = message {
                                last_pong = tokio::time::Instant::now();
                            }
                            handle_message(&reader_shared, &reader_outbound, message);
                        }
                    }
                }
            }
            fail_all(&reader_shared, "connection closed");
            reader_closed.notify_waiters();
        });

        Ok(Self {
            outbound,
            shared,
            next_id: Arc::new(AtomicU64::new(1)),
            closed,
        })
    }

    /// Resolves when the socket has closed for any reason.
    pub async fn closed(&self) {
        self.closed.notified().await;
    }

    pub async fn call<T: DeserializeOwned>(&self, tag: &str, payload: Value) -> Result<T> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.shared
            .pending
            .lock()
            .unwrap()
            .insert(id, Pending::Call(tx));
        self.outbound
            .send(FromClient::Request {
                id,
                tag: tag.to_string(),
                payload,
                headers: vec![],
            })
            .map_err(|_| anyhow!("connection closed"))?;
        let value = rx.await.map_err(|_| anyhow!("connection closed"))??;
        serde_json::from_value(value).with_context(|| format!("decoding {tag} result"))
    }

    pub async fn subscribe(&self, tag: &str, payload: Value) -> Result<Subscription> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        self.shared
            .pending
            .lock()
            .unwrap()
            .insert(id, Pending::Stream(tx));
        self.outbound
            .send(FromClient::Request {
                id,
                tag: tag.to_string(),
                payload,
                headers: vec![],
            })
            .map_err(|_| anyhow!("connection closed"))?;
        Ok(Subscription {
            id,
            rx,
            outbound: self.outbound.clone(),
        })
    }
}

fn handle_message(
    shared: &Shared,
    outbound: &mpsc::UnboundedSender<FromClient>,
    message: FromServer,
) {
    match message {
        FromServer::Chunk { request_id, values } => {
            // Ack first: the server withholds the next chunk until it sees this.
            let _ = outbound.send(FromClient::Ack { request_id });
            let mut pending = shared.pending.lock().unwrap();
            match pending.get(&request_id) {
                Some(Pending::Stream(tx)) => {
                    for value in values {
                        if tx.send(Ok(value)).is_err() {
                            pending.remove(&request_id);
                            break;
                        }
                    }
                }
                Some(Pending::Call(_)) => tracing::warn!(request_id, "chunk for unary call"),
                None => {}
            }
        }
        FromServer::Exit { request_id, exit } => {
            let entry = shared.pending.lock().unwrap().remove(&request_id);
            match (entry, exit) {
                (Some(Pending::Call(tx)), Exit::Success { value }) => {
                    let _ = tx.send(Ok(value));
                }
                (Some(Pending::Call(tx)), Exit::Failure { cause }) => {
                    let _ = tx.send(Err(anyhow!(wire::describe_failure(&cause))));
                }
                (Some(Pending::Stream(_)), Exit::Success { .. }) => {}
                (Some(Pending::Stream(tx)), Exit::Failure { cause }) => {
                    let _ = tx.send(Err(anyhow!(wire::describe_failure(&cause))));
                }
                (None, _) => {}
            }
        }
        FromServer::Defect { defect } => {
            tracing::error!(%defect, "server defect");
            fail_all(shared, &format!("server defect: {defect}"));
        }
        FromServer::ClientProtocolError { error } => {
            tracing::error!(%error, "protocol error");
            fail_all(shared, &format!("protocol error: {error}"));
        }
        FromServer::Pong | FromServer::Request { .. } => {}
    }
}

fn fail_all(shared: &Shared, reason: &str) {
    let mut pending = shared.pending.lock().unwrap();
    for (_, entry) in pending.drain() {
        match entry {
            Pending::Call(tx) => {
                let _ = tx.send(Err(anyhow!(reason.to_string())));
            }
            Pending::Stream(tx) => {
                let _ = tx.send(Err(anyhow!(reason.to_string())));
            }
        }
    }
}

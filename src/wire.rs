//! Effect RPC message envelopes, JSON serialization, one message per WebSocket text frame.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize)]
#[serde(tag = "_tag")]
pub enum FromClient {
    Request {
        id: u64,
        tag: String,
        payload: Value,
        headers: Vec<(String, String)>,
    },
    Ack {
        #[serde(rename = "requestId")]
        request_id: u64,
    },
    Interrupt {
        #[serde(rename = "requestId")]
        request_id: u64,
    },
    Ping,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "_tag")]
pub enum FromServer {
    Chunk {
        #[serde(rename = "requestId")]
        request_id: u64,
        values: Vec<Value>,
    },
    Exit {
        #[serde(rename = "requestId")]
        request_id: u64,
        exit: Exit,
    },
    Defect {
        defect: Value,
    },
    ClientProtocolError {
        error: Value,
    },
    Pong,
    /// Server-initiated request (unused by this client); kept so decoding never fails.
    Request {
        #[serde(default)]
        tag: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "_tag")]
pub enum Exit {
    Success { value: Value },
    Failure { cause: Vec<CauseEntry> },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "_tag")]
pub enum CauseEntry {
    Fail { error: Value },
    Die { defect: Value },
    Interrupt {},
}

/// Human-readable rendering of a failed exit for error messages.
pub fn describe_failure(cause: &[CauseEntry]) -> String {
    cause
        .iter()
        .map(|entry| match entry {
            CauseEntry::Fail { error } => {
                let tag = error.get("_tag").and_then(Value::as_str).unwrap_or("Error");
                let message = error
                    .get("message")
                    .or_else(|| error.get("detail"))
                    .or_else(|| error.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if message.is_empty() { tag.to_string() } else { format!("{tag}: {message}") }
            }
            CauseEntry::Die { defect } => format!("defect: {defect}"),
            CauseEntry::Interrupt {} => "interrupted".to_string(),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

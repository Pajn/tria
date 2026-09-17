//! A subagent's own transcript, read from the file the provider wrote and shaped into a
//! thread the chat renderer already knows how to draw.
//!
//! The file is the record of a conversation held out of sight of the thread: the prompt
//! the agent was given, what it said, and every tool call it made. Rather than grow a
//! second renderer for it, the rows are translated into the messages and tool activities
//! the server would have sent for the same work, so a subagent's transcript folds,
//! searches, yanks, and exports exactly like the conversation around it.
//!
//! The format is the provider's, not the protocol's: one JSON object per line, as
//! Claude's agent sessions record them. A line that does not fit the shape is skipped
//! rather than fatal, so a newer writer degrades to fewer rows instead of no transcript.

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

use crate::{model::ThreadDetailSnapshot, state::ThreadState};

/// What a parsed transcript amounts to, for the line under the view's title.
pub struct Summary {
    pub messages: usize,
    pub tools: usize,
    /// Rows that did not parse or carried nothing renderable.
    pub skipped: usize,
}

/// The file a task's output is written to, beside one whose path is known. The provider
/// writes them all into a single directory named after the session, each named after its
/// task, and it writes them as the task runs — but it only tells the server the path once
/// the task is over, so this is how a run still going is read.
pub fn sibling_path(known: &str, id: &str) -> Option<String> {
    let (dir, _) = known.rsplit_once('/')?;
    (!dir.is_empty()).then(|| format!("{dir}/{id}.output"))
}

/// Parse a transcript into a thread of its own. `id` and `title` name the subagent it
/// belongs to; the thread is never sent to the server, so the rest of the shell is the
/// little the renderer reads.
pub fn parse(text: &str, id: &str, title: &str) -> Result<(ThreadState, Summary)> {
    let mut messages: Vec<Value> = Vec::new();
    let mut activities: Vec<Value> = Vec::new();
    let mut summary = Summary {
        messages: 0,
        tools: 0,
        skipped: 0,
    };
    // Tool calls and their results arrive as separate rows; the call's activity is
    // completed in place when the result turns up.
    let mut tool_index: Vec<(String, usize)> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            summary.skipped += 1;
            continue;
        };
        let at = row
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let uuid = row
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let kind = row.get("type").and_then(Value::as_str).unwrap_or("");
        // Attachments are the harness talking to itself: reminders, environment notes,
        // the prompt snapshot. None of it is the agent's work.
        if kind == "attachment" {
            continue;
        }
        let Some(content) = row.get("message").and_then(|m| m.get("content")) else {
            summary.skipped += 1;
            continue;
        };

        // A plain string is the prompt the subagent was started with.
        if let Some(text) = content.as_str() {
            if text.trim().is_empty() {
                continue;
            }
            summary.messages += 1;
            messages.push(message(&uuid, role_for(kind), text, &at));
            continue;
        }
        let Some(blocks) = content.as_array() else {
            summary.skipped += 1;
            continue;
        };

        let mut said = String::new();
        for block in blocks {
            match block.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        if !said.is_empty() {
                            said.push_str("\n\n");
                        }
                        said.push_str(text);
                    }
                }
                // Reasoning is kept only when the provider left it readable; what it
                // records here is usually an encrypted signature and nothing else.
                "thinking" => {
                    if let Some(thought) = block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .filter(|t| !t.trim().is_empty())
                    {
                        summary.messages += 1;
                        messages.push(message(&format!("{uuid}-thinking"), "system", thought, &at));
                    }
                }
                "tool_use" => {
                    let Some(call_id) = block.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    summary.tools += 1;
                    tool_index.push((call_id.to_string(), activities.len()));
                    activities.push(tool_activity(call_id, block, &at));
                }
                "tool_result" => {
                    let Some(call_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(index) = tool_index
                        .iter()
                        .find(|(id, _)| id == call_id)
                        .map(|(_, index)| *index)
                    else {
                        // A result whose call is not in the file: the transcript was
                        // trimmed at the front, and there is no row to complete.
                        summary.skipped += 1;
                        continue;
                    };
                    complete_tool(&mut activities[index], block);
                }
                _ => {}
            }
        }
        if !said.trim().is_empty() {
            summary.messages += 1;
            messages.push(message(&uuid, role_for(kind), &said, &at));
        }
    }

    if messages.is_empty() && activities.is_empty() {
        bail!("no conversation in the transcript");
    }

    let snapshot = serde_json::from_value::<ThreadDetailSnapshot>(json!({
        "snapshotSequence": 0,
        "thread": {
            "id": format!("agent:{id}"),
            "projectId": "",
            "title": title,
            "modelSelection": { "instanceId": "", "model": "" },
            "messages": messages,
            "activities": activities,
            "proposedPlans": [],
        },
    }))?;
    Ok((ThreadState::from_snapshot(snapshot), summary))
}

/// The opening prompt is an instruction the subagent was handed, not something the
/// person at the keyboard said, so it is labelled for what it is.
fn role_for(kind: &str) -> &'static str {
    match kind {
        "user" => "prompt",
        "system" => "system",
        _ => "assistant",
    }
}

fn message(id: &str, role: &str, text: &str, at: &str) -> Value {
    json!({
        "id": id,
        "role": role,
        "text": text.trim_end(),
        "createdAt": at,
        "updatedAt": at,
    })
}

/// A tool call as the server would have reported it, so the row renders like any other.
/// It starts out completed: a transcript is only read once the call is over, and a call
/// left without a result is marked when its result fails to arrive.
fn tool_activity(call_id: &str, block: &Value, at: &str) -> Value {
    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
    let item_type = classify(name, &input);
    let mut data = Map::new();
    data.insert("toolName".into(), json!(name));
    data.insert("input".into(), input.clone());
    // The fields the chat reads for a one-line summary, filled where the tool has them.
    if let Some(command) = string(&input, &["command", "cmd"]) {
        data.insert("command".into(), json!(command));
    }
    if let Some(description) = string(&input, &["description"]) {
        data.insert("description".into(), json!(description));
    }
    if let Some(path) = string(&input, &["file_path", "path", "notebook_path"]) {
        data.insert("path".into(), json!(path));
        if item_type == "file_change" {
            data.insert("files".into(), json!([{ "path": path }]));
        }
    }
    json!({
        "id": call_id,
        "kind": "tool.completed",
        "tone": "info",
        "summary": title_for(item_type),
        "createdAt": at,
        "payload": {
            "itemType": item_type,
            "toolCallId": call_id,
            "status": "completed",
            "title": title_for(item_type),
            "detail": detail(name, &input, item_type),
            "data": Value::Object(data),
        },
    })
}

/// Hang the result off the call it belongs to, where the expanded row looks for it.
fn complete_tool(activity: &mut Value, block: &Value) {
    let text = result_text(block.get("content"));
    let failed = block
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if failed {
        activity["tone"] = json!("error");
        activity["payload"]["status"] = json!("failed");
    }
    activity["payload"]["data"]["result"] = json!({
        "type": "tool_result",
        "content": text,
    });
}

/// Tool results are a string, or the blocks of one. Anything that is not text — an
/// image, say — is named rather than dropped, so the row does not read as empty.
fn result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                Some(other) => format!("[{other}]"),
                None => String::new(),
            })
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn string(input: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// The server's own classification of a tool name, so a transcript row carries the same
/// icon and heading as the same call would in the thread.
fn classify(name: &str, input: &Value) -> &'static str {
    let name = name.to_lowercase();
    if name == "read"
        && string(input, &["file_path", "path"]).is_some_and(|path| is_image_path(&path))
    {
        return "image_view";
    }
    if name.contains("agent") || name == "task" || name.contains("sub-agent") {
        return "collab_agent_tool_call";
    }
    if name.contains("bash") || name.contains("command") || name.contains("shell") {
        return "command_execution";
    }
    if name.contains("edit")
        || name.contains("write")
        || name.contains("file")
        || name.contains("patch")
        || name.contains("replace")
        || name.contains("create")
        || name.contains("delete")
    {
        return "file_change";
    }
    if name.contains("mcp") {
        return "mcp_tool_call";
    }
    if name.contains("websearch") || name.contains("web search") {
        return "web_search";
    }
    if name.contains("image") {
        return "image_view";
    }
    "dynamic_tool_call"
}

fn is_image_path(path: &str) -> bool {
    let path = path.to_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"]
        .iter()
        .any(|extension| path.ends_with(extension))
}

fn title_for(item_type: &str) -> &'static str {
    match item_type {
        "command_execution" => "Command run",
        "file_change" => "File change",
        "mcp_tool_call" => "MCP tool call",
        "collab_agent_tool_call" => "Subagent task",
        "web_search" => "Web search",
        "image_view" => "Image view",
        _ => "Tool call",
    }
}

/// What the call asked for, in one string: the command, the agent's brief, the path, or
/// the input as it stands. Unlike the thread's own rows this is not cut short, because
/// the file is read whole and the expanded row is where the detail is wanted.
fn detail(name: &str, input: &Value, item_type: &str) -> String {
    if let Some(command) = string(input, &["command", "cmd"]) {
        return format!("{name}: {command}");
    }
    if item_type == "collab_agent_tool_call"
        && let Some(brief) = string(input, &["description", "prompt"])
    {
        return brief;
    }
    match string(
        input,
        &[
            "file_path",
            "path",
            "notebook_path",
            "pattern",
            "query",
            "url",
        ],
    ) {
        Some(value) => format!("{name}: {value}"),
        None => match serde_json::to_string(input) {
            Ok(json) if json != "{}" => format!("{name}: {json}"),
            _ => name.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transcript_is_named_after_its_task_beside_its_siblings() {
        assert_eq!(
            sibling_path("/tmp/agent-runs/a-project/session/tasks/old.output", "new").as_deref(),
            Some("/tmp/agent-runs/a-project/session/tasks/new.output")
        );
        assert_eq!(sibling_path("bare", "new"), None);
    }

    const ROWS: &str = r#"
{"type":"user","uuid":"u1","timestamp":"1","message":{"role":"user","content":"Find the wire protocol"}}
{"type":"attachment","uuid":"x1","timestamp":"2","attachment":{"type":"total_tokens_reminder"}}
{"type":"assistant","uuid":"a1","timestamp":"3","message":{"role":"assistant","content":[{"type":"thinking","thinking":"","signature":"CAI"},{"type":"text","text":"Looking now."},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls /src","description":"List the source"}}]}}
{"type":"user","uuid":"u2","timestamp":"4","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"main.rs\nui.rs"}]}}
{"type":"assistant","uuid":"a2","timestamp":"5","message":{"role":"assistant","content":[{"type":"text","text":"It is in ws.ts."}]}}
"#;

    #[test]
    fn reads_a_conversation_out_of_the_rows() {
        let (state, summary) = parse(ROWS, "a1", "Map the protocol").unwrap();
        assert_eq!(summary.messages, 3);
        assert_eq!(summary.tools, 1);
        assert_eq!(summary.skipped, 0);
        let messages = &state.detail.messages;
        // The brief the subagent was handed is not something the user said.
        assert_eq!(messages[0].role, "prompt");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[2].text, "It is in ws.ts.");
        // Reasoning the provider left encrypted is not a message.
        assert!(!messages.iter().any(|m| m.role == "system"));
    }

    #[test]
    fn a_tool_call_carries_its_result() {
        let (state, _) = parse(ROWS, "a1", "Map the protocol").unwrap();
        let tool = &state.detail.activities[0];
        assert_eq!(tool.kind, "tool.completed");
        assert_eq!(tool.str("itemType"), Some("command_execution"));
        assert_eq!(tool.str("detail"), Some("Bash: ls /src"));
        assert_eq!(tool.payload["data"]["command"], "ls /src");
        assert_eq!(tool.payload["data"]["result"]["content"], "main.rs\nui.rs");
    }

    #[test]
    fn a_failed_call_reads_as_failed() {
        let rows = r#"
{"type":"assistant","uuid":"a1","timestamp":"1","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Write","input":{"file_path":"/tmp/x.rs","content":"fn main() {}"}}]}}
{"type":"user","uuid":"u1","timestamp":"2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":[{"type":"text","text":"read-only file system"}]}]}}
"#;
        let (state, _) = parse(rows, "a1", "Write it").unwrap();
        let tool = &state.detail.activities[0];
        assert_eq!(tool.str("itemType"), Some("file_change"));
        assert_eq!(tool.tone, "error");
        assert_eq!(tool.str("status"), Some("failed"));
        assert_eq!(tool.payload["data"]["files"][0]["path"], "/tmp/x.rs");
        assert_eq!(
            tool.payload["data"]["result"]["content"],
            "read-only file system"
        );
    }

    #[test]
    fn a_broken_line_costs_only_itself() {
        let rows = format!("not json\n{ROWS}");
        let (state, summary) = parse(&rows, "a1", "Map the protocol").unwrap();
        assert_eq!(summary.skipped, 1);
        assert_eq!(state.detail.messages.len(), 3);
    }

    #[test]
    fn an_empty_transcript_is_an_error() {
        assert!(parse("", "a1", "Nothing").is_err());
        assert!(parse("{\"type\":\"attachment\"}", "a1", "Nothing").is_err());
    }
}

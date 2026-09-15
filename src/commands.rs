//! Builders for `orchestration.dispatchCommand` payloads.

use serde_json::{Value, json};

use crate::model::ModelSelection;

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn now_iso() -> String {
    let now = time::OffsetDateTime::now_utc();
    now.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

pub struct NewThread<'a> {
    pub project_id: &'a str,
    pub title: &'a str,
    pub model_selection: &'a ModelSelection,
    pub runtime_mode: &'a str,
    pub interaction_mode: &'a str,
}

/// Start a turn on an existing thread, or create the thread first when `bootstrap` is given.
pub fn turn_start(
    thread_id: &str,
    text: &str,
    model_selection: &ModelSelection,
    runtime_mode: &str,
    interaction_mode: &str,
    bootstrap: Option<NewThread<'_>>,
) -> Value {
    let created_at = now_iso();
    let mut command = json!({
        "type": "thread.turn.start",
        "commandId": new_id(),
        "threadId": thread_id,
        "message": {
            "messageId": new_id(),
            "role": "user",
            "text": text,
            "attachments": [],
        },
        "modelSelection": model_selection,
        "runtimeMode": runtime_mode,
        "interactionMode": interaction_mode,
        "createdAt": created_at,
    });
    if let Some(new_thread) = bootstrap {
        command["bootstrap"] = json!({
            "createThread": {
                "projectId": new_thread.project_id,
                "title": new_thread.title,
                "modelSelection": new_thread.model_selection,
                "runtimeMode": new_thread.runtime_mode,
                "interactionMode": new_thread.interaction_mode,
                "branch": null,
                "worktreePath": null,
                "createdAt": created_at,
            }
        });
    }
    command
}

pub fn turn_interrupt(thread_id: &str, turn_id: Option<&str>) -> Value {
    let mut command = json!({
        "type": "thread.turn.interrupt",
        "commandId": new_id(),
        "threadId": thread_id,
        "createdAt": now_iso(),
    });
    if let Some(turn_id) = turn_id {
        command["turnId"] = json!(turn_id);
    }
    command
}

pub fn approval_respond(thread_id: &str, request_id: &str, decision: &str) -> Value {
    json!({
        "type": "thread.approval.respond",
        "commandId": new_id(),
        "threadId": thread_id,
        "requestId": request_id,
        "decision": decision,
        "createdAt": now_iso(),
    })
}

pub fn user_input_respond(thread_id: &str, request_id: &str, answers: Value) -> Value {
    json!({
        "type": "thread.user-input.respond",
        "commandId": new_id(),
        "threadId": thread_id,
        "requestId": request_id,
        "answers": answers,
        "createdAt": now_iso(),
    })
}

pub fn user_input_dismiss(thread_id: &str, request_id: &str) -> Value {
    json!({
        "type": "thread.user-input.dismiss",
        "commandId": new_id(),
        "threadId": thread_id,
        "requestId": request_id,
        "createdAt": now_iso(),
    })
}

pub fn meta_update_title(thread_id: &str, title: &str) -> Value {
    json!({
        "type": "thread.meta.update",
        "commandId": new_id(),
        "threadId": thread_id,
        "title": title,
    })
}

pub fn meta_regenerate_title(thread_id: &str) -> Value {
    json!({
        "type": "thread.meta.update",
        "commandId": new_id(),
        "threadId": thread_id,
        "regenerateTitle": true,
    })
}

pub fn meta_update_model(thread_id: &str, model_selection: &ModelSelection) -> Value {
    json!({
        "type": "thread.meta.update",
        "commandId": new_id(),
        "threadId": thread_id,
        "modelSelection": model_selection,
    })
}

pub fn simple(kind: &str, thread_id: &str) -> Value {
    json!({
        "type": kind,
        "commandId": new_id(),
        "threadId": thread_id,
    })
}

pub fn unsettle(thread_id: &str) -> Value {
    json!({
        "type": "thread.unsettle",
        "commandId": new_id(),
        "threadId": thread_id,
        "reason": "user",
    })
}

pub fn unsnooze(thread_id: &str) -> Value {
    json!({
        "type": "thread.unsnooze",
        "commandId": new_id(),
        "threadId": thread_id,
        "reason": "user",
    })
}

pub fn runtime_mode_set(thread_id: &str, runtime_mode: &str) -> Value {
    json!({
        "type": "thread.runtime-mode.set",
        "commandId": new_id(),
        "threadId": thread_id,
        "runtimeMode": runtime_mode,
        "createdAt": now_iso(),
    })
}

pub fn interaction_mode_set(thread_id: &str, interaction_mode: &str) -> Value {
    json!({
        "type": "thread.interaction-mode.set",
        "commandId": new_id(),
        "threadId": thread_id,
        "interactionMode": interaction_mode,
        "createdAt": now_iso(),
    })
}

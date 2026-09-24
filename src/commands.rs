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
    /// Set to start the thread in a fresh worktree branched off `base_branch`; the
    /// server creates it and points the thread at it.
    pub worktree: Option<Worktree<'a>>,
    /// The branch and checkout of a thread this one carries on from, to work where it
    /// did. Without them the thread works in the project's own checkout.
    pub branch: Option<&'a str>,
    pub worktree_path: Option<&'a str>,
}

/// Where a worktree for a new thread comes from, and what it is called.
pub struct Worktree<'a> {
    pub project_cwd: &'a str,
    pub base_branch: &'a str,
    /// The branch to make for it. Naming one is what asks for a branch at all: without
    /// it the server checks the base branch out into the worktree instead, which git
    /// refuses the moment that branch is checked out anywhere else — and the project's
    /// own checkout, which is where the base branch was read from, is exactly that.
    pub branch: &'a str,
    /// Fetch and start from `origin/<base branch>` rather than the local one.
    pub start_from_origin: bool,
}

/// The branch a thread's worktree gets. The server names the worktree's directory after
/// it, so it is the thread rather than the message: a name made of whatever was typed
/// is a ref made of whatever was typed, and a message is not a ref.
pub fn worktree_branch(thread_id: &str) -> String {
    let short: String = thread_id.chars().take(8).collect();
    format!("tria/{short}")
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
                "branch": new_thread.branch,
                "worktreePath": new_thread.worktree_path,
                "createdAt": created_at,
            }
        });
        if let Some(worktree) = new_thread.worktree {
            // The server names the branch itself and updates the thread to match.
            command["bootstrap"]["prepareWorktree"] = json!({
                "projectCwd": worktree.project_cwd,
                "baseBranch": worktree.base_branch,
                "branch": worktree.branch,
                "startFromOrigin": worktree.start_from_origin,
            });
        }
    }
    command
}

/// What a turn that builds a proposed plan says. The desktop's words, so that a plan
/// implemented from either reads the same in the thread.
pub const PLAN_PROMPT_PREFIX: &str = "PLEASE IMPLEMENT THIS PLAN:\n";

/// Mark a turn as building a plan, which is what has the server record the plan as
/// implemented.
pub fn implementing(mut command: Value, thread_id: &str, plan_id: &str) -> Value {
    command["sourceProposedPlan"] = json!({ "threadId": thread_id, "planId": plan_id });
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

/// Drop the thread's turns after the first `turn_count`, from the conversation and, with
/// `restore_files`, from the checkout too, by putting back the files as that turn left
/// them. Two commands rather than a flag, so that a server which does not know the
/// history-only one refuses it instead of restoring files nobody asked it to.
pub fn thread_revert(thread_id: &str, turn_count: u32, restore_files: bool) -> Value {
    json!({
        "type": if restore_files { "thread.checkpoint.revert" } else { "thread.conversation.revert" },
        "commandId": new_id(),
        "threadId": thread_id,
        "turnCount": turn_count,
        "createdAt": now_iso(),
    })
}

/// Stop the thread's provider session. The protocol has no stop for one background task,
/// and monitors and backgrounded commands are the session's own processes, so this is
/// what ends them. The conversation is untouched: the next message starts a session again.
pub fn session_stop(thread_id: &str) -> Value {
    json!({
        "type": "thread.session.stop",
        "commandId": new_id(),
        "threadId": thread_id,
        "createdAt": now_iso(),
    })
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

/// Add a project for a directory. The server takes the root as it is given, beyond
/// making it absolute, and refuses a directory it already has an active project for —
/// so the project list is the thing to look in before asking for this. The directory
/// has to be there: tria never asks the server to make one, since a path that is not
/// there is a path somebody has mistyped.
pub fn project_create(project_id: &str, title: &str, workspace_root: &str) -> Value {
    json!({
        "type": "project.create",
        "commandId": new_id(),
        "projectId": project_id,
        "title": title,
        "workspaceRoot": workspace_root,
        "createdAt": now_iso(),
    })
}

/// Rename a project. The title is the server's own, so every client that draws the
/// project — this one, the desktop app — is renaming it for all of them.
pub fn project_rename(project_id: &str, title: &str) -> Value {
    json!({
        "type": "project.meta.update",
        "commandId": new_id(),
        "projectId": project_id,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn selection() -> ModelSelection {
        ModelSelection {
            instance_id: "instance".into(),
            model: "a-model".into(),
            options: vec![],
        }
    }

    #[test]
    fn a_new_thread_can_ask_for_a_worktree() {
        let selection = selection();
        let command = turn_start(
            "thread-1",
            "hello",
            &selection,
            "full-access",
            "default",
            Some(NewThread {
                project_id: "project-1",
                title: "hello",
                model_selection: &selection,
                runtime_mode: "full-access",
                interaction_mode: "default",
                worktree: Some(Worktree {
                    project_cwd: "/src/project",
                    base_branch: "main",
                    branch: "tria/12345678",
                    start_from_origin: true,
                }),
                branch: None,
                worktree_path: None,
            }),
        );
        let bootstrap = &command["bootstrap"];
        assert_eq!(bootstrap["createThread"]["projectId"], "project-1");
        // The thread starts without one; the server fills both in once it has the worktree.
        assert!(bootstrap["createThread"]["worktreePath"].is_null());
        assert_eq!(bootstrap["prepareWorktree"]["projectCwd"], "/src/project");
        assert_eq!(bootstrap["prepareWorktree"]["baseBranch"], "main");
        // Without a branch of its own the server checks the base branch out instead,
        // which fails while the project's checkout has it.
        assert_eq!(bootstrap["prepareWorktree"]["branch"], "tria/12345678");
        assert_eq!(bootstrap["prepareWorktree"]["startFromOrigin"], true);
    }

    #[test]
    fn a_worktree_branch_is_named_after_its_thread() {
        assert_eq!(
            worktree_branch("1f3f5b4d-9421-4c40-81d4-fa96b4179d7e"),
            "tria/1f3f5b4d"
        );
        // Whatever it is given, the result is a name git will take.
        assert_eq!(worktree_branch(""), "tria/");
    }

    #[test]
    fn a_new_thread_in_the_checkout_asks_for_no_worktree() {
        let selection = selection();
        let command = turn_start(
            "thread-1",
            "hello",
            &selection,
            "full-access",
            "default",
            Some(NewThread {
                project_id: "project-1",
                title: "hello",
                model_selection: &selection,
                runtime_mode: "full-access",
                interaction_mode: "default",
                worktree: None,
                branch: None,
                worktree_path: None,
            }),
        );
        assert!(command["bootstrap"]["prepareWorktree"].is_null());
    }
}

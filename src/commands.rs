//! Builders for `orchestration.dispatchCommand` payloads, and for the other requests
//! that change a thread or project: `orchestration.launchThread` and `projects.mutate`.

use serde_json::{Value, json};

use crate::model::{ModelSelection, RollbackTarget};

/// What the server records as where a thread or message came from. The protocol names
/// the desktop and web apps, phones, agents, and itself; a client of the desktop's kind
/// is the nearest of those to this one.
const CREATION_SOURCE: &str = "web";

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
    /// Ask the server for a title in place of `title`, which is only a stand-in.
    pub generate_title: bool,
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
/// is a ref made of whatever was typed, and a message is not a ref. It takes the form
/// the server treats as a placeholder, `t3/` and eight hex digits, which a thread ID
/// starts with: after the first turn the server renames such a branch to one generated
/// from the message, under its configured branch name prefix.
pub fn worktree_branch(thread_id: &str) -> String {
    let short: String = thread_id.chars().take(8).collect();
    format!("t3/{short}")
}

/// Send a message on an existing thread. The server decides what it does with it: starts
/// a run, or steers or queues behind the one already going.
pub fn turn_start(
    thread_id: &str,
    text: &str,
    images: &[crate::attachment::Image],
    model_selection: &ModelSelection,
) -> Value {
    json!({
        "type": "message.dispatch",
        "commandId": new_id(),
        "createdBy": "user",
        "creationSource": CREATION_SOURCE,
        "threadId": thread_id,
        "messageId": new_id(),
        "text": text,
        "attachments": attachments(images),
        "modelSelection": model_selection,
        "deliveryIntent": "auto",
        "dispatchMode": { "type": "start_immediately" },
    })
}

/// The input of `orchestration.launchThread`: make the thread, prepare where it works,
/// and send its first message.
pub fn launch_thread(
    thread_id: &str,
    text: &str,
    images: &[crate::attachment::Image],
    new_thread: NewThread<'_>,
) -> Value {
    let workspace = match (&new_thread.worktree, new_thread.worktree_path) {
        (Some(worktree), _) => json!({
            "type": "worktree",
            "baseRef": worktree.base_branch,
            "branch": worktree.branch,
            "startFromOrigin": worktree.start_from_origin,
        }),
        (None, Some(path)) => json!({ "type": "existing_worktree", "worktreePath": path }),
        (None, None) => json!({ "type": "root" }),
    };
    let mut workspace = workspace;
    if new_thread.worktree.is_none()
        && let Some(branch) = new_thread.branch
    {
        workspace["branch"] = json!(branch);
    }
    json!({
        "commandId": new_id(),
        "creationSource": CREATION_SOURCE,
        "threadId": thread_id,
        "projectId": new_thread.project_id,
        "title": new_thread.title,
        "generateTitle": new_thread.generate_title,
        "modelSelection": new_thread.model_selection,
        "runtimeMode": new_thread.runtime_mode,
        "interactionMode": new_thread.interaction_mode,
        "workspaceStrategy": workspace,
        "initialMessage": {
            "messageId": new_id(),
            "text": text,
            "attachments": attachments(images),
        },
    })
}

fn attachments(images: &[crate::attachment::Image]) -> Value {
    Value::Array(
        images
            .iter()
            .map(crate::attachment::Image::to_json)
            .collect(),
    )
}

/// What a turn that builds a proposed plan says. The desktop's words, so that a plan
/// implemented from either reads the same in the thread.
pub const PLAN_PROMPT_PREFIX: &str = "PLEASE IMPLEMENT THIS PLAN:\n";

/// Mark a turn as building a plan, which is what has the server record the plan as
/// implemented.
pub fn implementing(mut command: Value, thread_id: &str, plan_id: &str) -> Value {
    command["sourcePlanRef"] = json!({ "threadId": thread_id, "planId": plan_id });
    command
}

/// Interrupt a run: the one going, or the one whose background work is still going.
/// Messages queued behind it stay queued.
pub fn turn_interrupt(thread_id: &str, run_id: &str) -> Value {
    json!({
        "type": "run.interrupt",
        "commandId": new_id(),
        "threadId": thread_id,
        "runId": run_id,
        "holdQueue": true,
    })
}

/// Undo the turns after a checkpoint: from the conversation and, with `restore_files`,
/// from the checkout too, by putting back the files as the checkpoint has them.
pub fn thread_revert(thread_id: &str, target: &RollbackTarget, restore_files: bool) -> Value {
    json!({
        "type": "checkpoint.rollback",
        "commandId": new_id(),
        "threadId": thread_id,
        "scopeId": target.scope_id,
        "checkpointId": target.checkpoint_id,
        "restoreFiles": restore_files,
    })
}

/// Let go of one of the thread's provider sessions. The protocol has no stop for one
/// background task, and monitors and backgrounded commands are the session's own
/// processes, so this is what ends them. The conversation is untouched: the next
/// message starts a session again.
pub fn session_stop(thread_id: &str, provider_session_id: &str) -> Value {
    json!({
        "type": "provider-session.detach",
        "commandId": new_id(),
        "threadId": thread_id,
        "providerSessionId": provider_session_id,
        "reason": "client-requested",
    })
}

/// Put a pull request on a thread, named as the link names it.
pub fn pull_request_link(thread_id: &str, link: &PullRequestLink) -> Value {
    json!({
        "type": "thread.pull-request.link",
        "commandId": new_id(),
        "threadId": thread_id,
        "host": link.host,
        "repository": link.repository,
        "number": link.number,
        "url": link.url,
        "source": "manual",
    })
}

/// What a thread's link to a pull request is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestLink {
    pub host: String,
    pub repository: String,
    pub number: u64,
    pub url: String,
}

impl PullRequestLink {
    /// The link a pull request's own address makes, where it is one of the ordinary
    /// `https://host/owner/repository/pull/number` kind.
    pub fn from_url(url: &str) -> Option<Self> {
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))?;
        let mut parts = rest.split('/');
        let host = parts.next()?.to_string();
        let owner = parts.next()?;
        let repository = parts.next()?;
        if parts.next()? != "pull" {
            return None;
        }
        let number = parts.next()?.split(['?', '#']).next()?.parse().ok()?;
        Some(Self {
            host,
            repository: format!("{owner}/{repository}"),
            number,
            url: url.to_string(),
        })
    }
}

/// Take a pull request off a thread. It is named as the link names it: host, repository,
/// and number.
pub fn pull_request_unlink(thread_id: &str, host: &str, repository: &str, number: u64) -> Value {
    json!({
        "type": "thread.pull-request.unlink",
        "commandId": new_id(),
        "threadId": thread_id,
        "host": host,
        "repository": repository,
        "number": number,
    })
}

pub fn approval_respond(thread_id: &str, request_id: &str, decision: &str) -> Value {
    json!({
        "type": "runtime-request.respond",
        "commandId": new_id(),
        "threadId": thread_id,
        "requestId": request_id,
        "decision": decision,
    })
}

pub fn user_input_respond(thread_id: &str, request_id: &str, answers: Value) -> Value {
    json!({
        "type": "runtime-request.respond",
        "commandId": new_id(),
        "threadId": thread_id,
        "requestId": request_id,
        "answers": answers,
    })
}

pub fn user_input_dismiss(thread_id: &str, request_id: &str) -> Value {
    json!({
        "type": "thread.user-input.dismiss",
        "commandId": new_id(),
        "threadId": thread_id,
        "requestId": request_id,
    })
}

pub fn meta_update_title(thread_id: &str, title: &str) -> Value {
    json!({
        "type": "thread.metadata.update",
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
    })
}

/// Rename a project. The title is the server's own, so every client that draws the
/// project — this one, the desktop app — is renaming it for all of them.
pub fn project_rename(project_id: &str, title: &str) -> Value {
    json!({
        "type": "project.update",
        "commandId": new_id(),
        "projectId": project_id,
        "title": title,
    })
}

pub fn meta_regenerate_title(thread_id: &str) -> Value {
    json!({
        "type": "thread.metadata.update",
        "commandId": new_id(),
        "threadId": thread_id,
        "regenerateTitle": true,
    })
}

pub fn meta_update_model(thread_id: &str, model_selection: &ModelSelection) -> Value {
    json!({
        "type": "thread.model-selection.set",
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
    })
}

pub fn interaction_mode_set(thread_id: &str, interaction_mode: &str) -> Value {
    json!({
        "type": "thread.interaction-mode.set",
        "commandId": new_id(),
        "threadId": thread_id,
        "interactionMode": interaction_mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pull_requests_link_is_read_from_its_address() {
        assert_eq!(
            PullRequestLink::from_url("https://github.com/o/r/pull/42#discussion"),
            Some(PullRequestLink {
                host: "github.com".into(),
                repository: "o/r".into(),
                number: 42,
                url: "https://github.com/o/r/pull/42#discussion".into(),
            })
        );
        assert_eq!(
            PullRequestLink::from_url("https://github.com/o/r/issues/42"),
            None
        );
        assert_eq!(PullRequestLink::from_url("42"), None);
    }

    fn selection() -> ModelSelection {
        ModelSelection {
            instance_id: "instance".into(),
            model: "a-model".into(),
            options: vec![],
        }
    }

    fn new_thread<'a>(
        selection: &'a ModelSelection,
        worktree: Option<Worktree<'a>>,
        branch: Option<&'a str>,
    ) -> NewThread<'a> {
        NewThread {
            project_id: "project-1",
            title: "hello",
            generate_title: true,
            model_selection: selection,
            runtime_mode: "full-access",
            interaction_mode: "default",
            worktree,
            branch,
            worktree_path: None,
        }
    }

    #[test]
    fn a_new_thread_can_ask_for_a_worktree() {
        let selection = selection();
        let worktree = Worktree {
            base_branch: "main",
            branch: "tria/12345678",
            start_from_origin: true,
        };
        let input = launch_thread(
            "thread-1",
            "hello",
            &[],
            new_thread(&selection, Some(worktree), None),
        );
        assert_eq!(input["projectId"], "project-1");
        assert_eq!(input["initialMessage"]["text"], "hello");
        let workspace = &input["workspaceStrategy"];
        assert_eq!(workspace["type"], "worktree");
        assert_eq!(workspace["baseRef"], "main");
        // Without a branch of its own the server checks the base branch out instead,
        // which fails while the project's checkout has it.
        assert_eq!(workspace["branch"], "tria/12345678");
        assert_eq!(workspace["startFromOrigin"], true);
    }

    #[test]
    fn a_worktree_branch_is_named_after_its_thread() {
        assert_eq!(
            worktree_branch("1f3f5b4d-9421-4c40-81d4-fa96b4179d7e"),
            "t3/1f3f5b4d"
        );
        // Whatever it is given, the result is a name git will take.
        assert_eq!(worktree_branch(""), "t3/");
    }

    #[test]
    fn a_new_thread_in_the_checkout_asks_for_no_worktree() {
        let selection = selection();
        let input = launch_thread("thread-1", "hello", &[], new_thread(&selection, None, None));
        assert_eq!(input["workspaceStrategy"], json!({ "type": "root" }));
        let input = launch_thread(
            "thread-1",
            "hello",
            &[],
            new_thread(&selection, None, Some("topic")),
        );
        assert_eq!(
            input["workspaceStrategy"],
            json!({ "type": "root", "branch": "topic" })
        );
    }
}

//! Pure reducers for the shell (project and thread list) and one open thread.

use std::collections::HashMap;

use serde_json::Value;

use crate::model::{
    Activity, Event, Id, Message, Project, ProposedPlan, Session, ShellItem, ThreadDetail,
    ThreadDetailSnapshot, ThreadItem, ThreadShell,
};

#[derive(Debug, Default)]
pub struct Shell {
    pub projects: HashMap<Id, Project>,
    pub threads: HashMap<Id, ThreadShell>,
    pub last_sequence: Option<u64>,
    pub synchronized: bool,
    pub loaded: bool,
}

impl Shell {
    pub fn apply(&mut self, item: ShellItem) {
        match item {
            ShellItem::Synchronized => self.synchronized = true,
            ShellItem::Snapshot { snapshot } => {
                self.projects = snapshot.projects.into_iter().map(|p| (p.id.clone(), p)).collect();
                self.threads = snapshot
                    .threads
                    .into_iter()
                    .filter(|t| t.archived_at.is_none())
                    .map(|t| (t.id.clone(), t))
                    .collect();
                self.last_sequence = Some(snapshot.snapshot_sequence);
                self.loaded = true;
            }
            ShellItem::ProjectUpserted { sequence, project } => {
                if self.advance(sequence) {
                    self.projects.insert(project.id.clone(), project);
                }
            }
            ShellItem::ProjectRemoved { sequence, project_id } => {
                if self.advance(sequence) {
                    self.projects.remove(&project_id);
                }
            }
            ShellItem::ThreadUpserted { sequence, thread } => {
                if self.advance(sequence) {
                    if thread.archived_at.is_some() {
                        self.threads.remove(&thread.id);
                    } else {
                        self.threads.insert(thread.id.clone(), thread);
                    }
                }
            }
            ShellItem::ThreadRemoved { sequence, thread_id } => {
                if self.advance(sequence) {
                    self.threads.remove(&thread_id);
                }
            }
            ShellItem::Unknown => {}
        }
    }

    fn advance(&mut self, sequence: u64) -> bool {
        if self.last_sequence.is_some_and(|last| sequence <= last) {
            return false;
        }
        self.last_sequence = Some(sequence);
        true
    }

    /// Threads ordered for the picker: pinned first, then most recently updated.
    pub fn sorted_threads(&self) -> Vec<&ThreadShell> {
        let mut threads: Vec<&ThreadShell> = self.threads.values().collect();
        threads.sort_by(|a, b| {
            b.pinned_at
                .is_some()
                .cmp(&a.pinned_at.is_some())
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });
        threads
    }

    pub fn project_title(&self, project_id: &str) -> &str {
        self.projects.get(project_id).map(|p| p.title.as_str()).unwrap_or("?")
    }
}

#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub request_id: String,
    pub request_kind: String,
    pub detail: Option<String>,
    pub options: Vec<ApprovalOption>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct ApprovalOption {
    pub decision: String,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct PendingUserInput {
    pub request_id: Option<String>,
    pub questions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PlanStep {
    pub step: String,
    pub status: String,
}

#[derive(Debug)]
pub struct ThreadState {
    pub detail: ThreadDetail,
    pub last_sequence: u64,
    pub synchronized: bool,
    pub has_more: bool,
    pub before_cursor: Option<String>,
    /// Bumped on every change so renderers can cache derived output.
    pub revision: u64,
    /// Set when an event could not be applied incrementally and a fresh snapshot is needed.
    pub needs_snapshot: bool,
}

impl ThreadState {
    pub fn from_snapshot(snapshot: ThreadDetailSnapshot) -> Self {
        let (has_more, before_cursor) = match &snapshot.page {
            Some(page) => (page.has_more, page.before_cursor.clone()),
            None => (false, None),
        };
        Self {
            detail: snapshot.thread,
            last_sequence: snapshot.snapshot_sequence,
            synchronized: false,
            has_more,
            before_cursor,
            revision: 1,
            needs_snapshot: false,
        }
    }

    pub fn id(&self) -> &str {
        &self.detail.shell.id
    }

    pub fn apply(&mut self, item: ThreadItem) {
        match item {
            ThreadItem::Synchronized => self.synchronized = true,
            ThreadItem::Snapshot { snapshot } => {
                let synchronized = self.synchronized;
                *self = Self::from_snapshot(snapshot);
                self.synchronized = synchronized;
            }
            ThreadItem::Event { event } => self.apply_event(event),
            ThreadItem::Unknown => {}
        }
    }

    pub fn apply_event(&mut self, event: Event) {
        if event.sequence <= self.last_sequence {
            return;
        }
        self.last_sequence = event.sequence;
        match event.kind.as_str() {
            "thread.message-sent" => {
                let Ok(incoming) = serde_json::from_value::<MessageSent>(event.payload) else { return };
                self.apply_message(incoming);
            }
            "thread.activity-appended" => {
                let Some(activity) = event.payload.get("activity").cloned() else { return };
                let Ok(activity) = serde_json::from_value::<Activity>(activity) else { return };
                if !self.detail.activities.iter().any(|a| a.id == activity.id) {
                    self.detail.activities.push(activity);
                }
            }
            "thread.session-set" => {
                let Some(session) = event.payload.get("session").cloned() else { return };
                if let Ok(session) = serde_json::from_value::<Session>(session) {
                    self.detail.shell.session = Some(session);
                }
            }
            "thread.proposed-plan-upserted" => {
                let Some(plan) = event.payload.get("proposedPlan").cloned() else { return };
                let Ok(plan) = serde_json::from_value::<ProposedPlan>(plan) else { return };
                match self.detail.proposed_plans.iter_mut().find(|p| p.id == plan.id) {
                    Some(existing) => *existing = plan,
                    None => self.detail.proposed_plans.push(plan),
                }
            }
            "thread.reverted" => self.needs_snapshot = true,
            _ => return,
        }
        self.revision += 1;
    }

    fn apply_message(&mut self, incoming: MessageSent) {
        if let Some(existing) = self.detail.messages.iter_mut().find(|m| m.id == incoming.message_id) {
            if incoming.streaming {
                existing.text.push_str(&incoming.text);
            } else if !incoming.text.is_empty() {
                existing.text = incoming.text;
            }
            existing.streaming = incoming.streaming;
            existing.updated_at = incoming.updated_at;
            if incoming.turn_id.is_some() {
                existing.turn_id = incoming.turn_id;
            }
        } else {
            self.detail.messages.push(Message {
                id: incoming.message_id,
                role: incoming.role,
                text: incoming.text,
                turn_id: incoming.turn_id,
                streaming: incoming.streaming,
                created_at: incoming.created_at,
                updated_at: incoming.updated_at,
            });
        }
    }

    /// Update list-level fields from the shell stream (title, model, latest turn).
    pub fn sync_shell(&mut self, shell: &ThreadShell) {
        let mine = &mut self.detail.shell;
        mine.title = shell.title.clone();
        mine.model_selection = shell.model_selection.clone();
        mine.runtime_mode = shell.runtime_mode.clone();
        mine.interaction_mode = shell.interaction_mode.clone();
        mine.latest_turn = shell.latest_turn.clone();
        mine.plan_progress = shell.plan_progress.clone();
        mine.has_pending_approvals = shell.has_pending_approvals;
        mine.has_pending_user_input = shell.has_pending_user_input;
        mine.updated_at = shell.updated_at.clone();
        self.revision += 1;
    }

    pub fn is_running(&self) -> bool {
        self.detail.shell.is_running()
            || self
                .detail
                .shell
                .session
                .as_ref()
                .is_some_and(|s| s.status == "running" || s.status == "starting")
    }

    pub fn pending_approvals(&self) -> Vec<PendingApproval> {
        let mut pending: Vec<PendingApproval> = Vec::new();
        for activity in &self.detail.activities {
            match activity.kind.as_str() {
                "approval.requested" => {
                    let Some(request_id) = activity.str("requestId") else { continue };
                    let options = activity
                        .payload
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|options| {
                            options
                                .iter()
                                .filter_map(|o| {
                                    Some(ApprovalOption {
                                        decision: o.get("decision")?.as_str()?.to_string(),
                                        label: o.get("label")?.as_str()?.to_string(),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    pending.push(PendingApproval {
                        request_id: request_id.to_string(),
                        request_kind: activity
                            .str("requestKind")
                            .or_else(|| activity.str("requestType"))
                            .unwrap_or("approval")
                            .to_string(),
                        detail: activity.str("detail").map(str::to_string),
                        options,
                        created_at: activity.created_at.clone(),
                    });
                }
                "approval.resolved" => {
                    if let Some(request_id) = activity.str("requestId") {
                        pending.retain(|p| p.request_id != request_id);
                    }
                }
                _ => {}
            }
        }
        pending
    }

    pub fn pending_user_input(&self) -> Option<PendingUserInput> {
        let mut pending: Option<PendingUserInput> = None;
        for activity in &self.detail.activities {
            match activity.kind.as_str() {
                "user-input.requested" => {
                    let questions = activity
                        .payload
                        .get("questions")
                        .and_then(Value::as_array)
                        .map(|qs| {
                            qs.iter()
                                .filter_map(|q| q.get("question").and_then(Value::as_str))
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    pending = Some(PendingUserInput {
                        request_id: activity.str("requestId").map(str::to_string),
                        questions,
                    });
                }
                "user-input.resolved" | "user-input.answer-submitted" => pending = None,
                _ => {}
            }
        }
        pending
    }

    /// Latest plan steps for the active turn, if any.
    pub fn active_plan(&self) -> Vec<PlanStep> {
        let Some(turn_id) = self.detail.shell.latest_turn.as_ref().map(|t| &t.turn_id) else {
            return Vec::new();
        };
        self.detail
            .activities
            .iter()
            .rev()
            .find(|a| a.kind == "turn.plan.updated" && a.turn_id.as_ref() == Some(turn_id))
            .and_then(|a| a.payload.get("plan"))
            .and_then(Value::as_array)
            .map(|steps| {
                steps
                    .iter()
                    .filter_map(|s| {
                        Some(PlanStep {
                            step: s.get("step")?.as_str()?.to_string(),
                            status: s.get("status")?.as_str().unwrap_or("pending").to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageSent {
    message_id: Id,
    role: String,
    text: String,
    #[serde(default)]
    turn_id: Option<Id>,
    #[serde(default)]
    streaming: bool,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn thread() -> ThreadState {
        let snapshot: ThreadDetailSnapshot = serde_json::from_value(json!({
            "snapshotSequence": 10,
            "thread": {
                "id": "t1", "projectId": "p1", "title": "Test",
                "modelSelection": {"instanceId": "claudeAgent", "model": "m"},
                "runtimeMode": "full-access", "latestTurn": null, "session": null,
                "messages": [], "activities": []
            }
        }))
        .unwrap();
        ThreadState::from_snapshot(snapshot)
    }

    fn message_event(sequence: u64, text: &str, streaming: bool) -> Event {
        serde_json::from_value(json!({
            "sequence": sequence, "eventId": format!("e{sequence}"), "type": "thread.message-sent",
            "payload": {"threadId": "t1", "messageId": "m1", "role": "assistant", "text": text,
                        "turnId": "turn", "streaming": streaming, "createdAt": "", "updatedAt": ""}
        }))
        .unwrap()
    }

    #[test]
    fn streaming_deltas_append_and_final_replaces() {
        let mut state = thread();
        state.apply_event(message_event(11, "Hel", true));
        state.apply_event(message_event(12, "lo", true));
        assert_eq!(state.detail.messages[0].text, "Hello");
        assert!(state.detail.messages[0].streaming);
        state.apply_event(message_event(13, "Hello!", false));
        assert_eq!(state.detail.messages[0].text, "Hello!");
        assert!(!state.detail.messages[0].streaming);
        state.apply_event(message_event(14, "", false));
        assert_eq!(state.detail.messages[0].text, "Hello!");
    }

    #[test]
    fn stale_sequences_are_ignored() {
        let mut state = thread();
        state.apply_event(message_event(5, "old", true));
        assert!(state.detail.messages.is_empty());
    }

    #[test]
    fn approvals_resolve() {
        let mut state = thread();
        let requested: Event = serde_json::from_value(json!({
            "sequence": 11, "eventId": "a", "type": "thread.activity-appended",
            "payload": {"threadId": "t1", "activity": {"id": "a1", "tone": "approval", "kind": "approval.requested",
                "summary": "s", "payload": {"requestId": "r1", "requestKind": "command", "detail": "rm -rf",
                "options": [{"decision": "accept", "label": "Allow"}]}, "turnId": null, "createdAt": ""}}
        }))
        .unwrap();
        state.apply_event(requested);
        assert_eq!(state.pending_approvals().len(), 1);
        let resolved: Event = serde_json::from_value(json!({
            "sequence": 12, "eventId": "b", "type": "thread.activity-appended",
            "payload": {"threadId": "t1", "activity": {"id": "a2", "tone": "approval", "kind": "approval.resolved",
                "summary": "s", "payload": {"requestId": "r1"}, "turnId": null, "createdAt": ""}}
        }))
        .unwrap();
        state.apply_event(resolved);
        assert!(state.pending_approvals().is_empty());
    }
}

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
                self.projects = snapshot
                    .projects
                    .into_iter()
                    .map(|p| (p.id.clone(), p))
                    .collect();
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
            ShellItem::ProjectRemoved {
                sequence,
                project_id,
            } => {
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
            ShellItem::ThreadRemoved {
                sequence,
                thread_id,
            } => {
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

    /// Threads grouped the way the desktop sidebar shows them.
    pub fn sections(&self, now: &str) -> Sections<'_> {
        let mut sections = Sections::default();
        for thread in self.threads.values() {
            if thread.is_settled() {
                sections.settled.push(thread);
            } else if thread.is_snoozed(now) {
                sections.snoozed.push(thread);
            } else if thread.pinned_at.is_some() {
                sections.pinned.push(thread);
            } else {
                sections.active.push(thread);
            }
        }
        sections.pinned.sort_by(|a, b| {
            a.pin_order_key
                .cmp(&b.pin_order_key)
                .then_with(|| b.pinned_at.cmp(&a.pinned_at))
        });
        // New or un-settled work floats to the top; the server's manual order keys are not
        // interpreted here.
        sections.active.sort_by(|a, b| anchor(b).cmp(anchor(a)));
        sections
            .snoozed
            .sort_by(|a, b| a.snoozed_until.cmp(&b.snoozed_until));
        sections.settled.sort_by(|a, b| {
            b.settled_at
                .cmp(&a.settled_at)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });
        sections
    }

    /// Flat navigation order: pinned, active, then optionally snoozed and settled.
    pub fn sorted_threads(&self, now: &str, include_parked: bool) -> Vec<&ThreadShell> {
        let sections = self.sections(now);
        let mut threads = sections.pinned;
        threads.extend(sections.active);
        if include_parked {
            threads.extend(sections.snoozed);
            threads.extend(sections.settled);
        }
        threads
    }

    pub fn project_title(&self, project_id: &str) -> &str {
        self.projects
            .get(project_id)
            .map(|p| p.title.as_str())
            .unwrap_or("?")
    }
}

#[derive(Debug, Default)]
pub struct Sections<'a> {
    pub pinned: Vec<&'a ThreadShell>,
    pub active: Vec<&'a ThreadShell>,
    pub snoozed: Vec<&'a ThreadShell>,
    pub settled: Vec<&'a ThreadShell>,
}

fn anchor(thread: &ThreadShell) -> &str {
    thread
        .unsettled_at
        .as_deref()
        .filter(|u| *u > thread.created_at.as_str())
        .unwrap_or(thread.created_at.as_str())
}

/// A background task with no reported end, for the `:tasks` view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningTask {
    pub id: String,
    pub title: String,
    /// Provider task type, such as `local_bash`.
    pub task_type: String,
    /// `background` for monitors and backgrounded commands.
    pub agent_kind: String,
    pub started_at: String,
    pub turn_id: Option<String>,
    /// A foreground command that was moved to the background.
    pub backgrounded: bool,
}

#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub request_id: String,
    pub request_kind: String,
    pub detail: Option<String>,
    pub options: Vec<ApprovalOption>,
}

#[derive(Debug, Clone)]
pub struct ApprovalOption {
    pub decision: String,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct PendingUserInput {
    pub request_id: String,
    pub questions: Vec<Question>,
    /// Message-mode questions can be closed without a reply; native callbacks cannot.
    pub dismissible: bool,
}

#[derive(Debug, Clone)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub text: String,
    pub options: Vec<QuestionOption>,
    pub allow_custom: bool,
    pub multi_select: bool,
}

#[derive(Debug, Clone)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
    /// The wire value for this option: `value` when present, otherwise the label.
    pub value: String,
}

fn parse_questions(value: Option<&Value>) -> Vec<Question> {
    let Some(questions) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    questions
        .iter()
        .filter_map(|q| {
            let text = q
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let id = q
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| text.clone());
            if id.is_empty() {
                return None;
            }
            let options: Vec<QuestionOption> = q
                .get("options")
                .and_then(Value::as_array)
                .map(|opts| {
                    opts.iter()
                        .filter_map(|o| {
                            let label = o.get("label").and_then(Value::as_str)?.to_string();
                            let value = o
                                .get("value")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| label.clone());
                            Some(QuestionOption {
                                label,
                                description: o
                                    .get("description")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                value,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let allow_custom = q
                .get("allowCustomAnswer")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if options.is_empty() && !allow_custom {
                return None;
            }
            Some(Question {
                id,
                header: q
                    .get("header")
                    .and_then(Value::as_str)
                    .unwrap_or("Question")
                    .to_string(),
                text,
                options,
                allow_custom,
                multi_select: q
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
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
                let Ok(incoming) = serde_json::from_value::<MessageSent>(event.payload) else {
                    return;
                };
                self.apply_message(incoming);
            }
            "thread.activity-appended" => {
                let Some(activity) = event.payload.get("activity").cloned() else {
                    return;
                };
                let Ok(activity) = serde_json::from_value::<Activity>(activity) else {
                    return;
                };
                if !self.detail.activities.iter().any(|a| a.id == activity.id) {
                    self.detail.activities.push(activity);
                }
            }
            "thread.session-set" => {
                let Some(session) = event.payload.get("session").cloned() else {
                    return;
                };
                if let Ok(session) = serde_json::from_value::<Session>(session) {
                    self.detail.shell.session = Some(session);
                }
            }
            "thread.proposed-plan-upserted" => {
                let Some(plan) = event.payload.get("proposedPlan").cloned() else {
                    return;
                };
                let Ok(plan) = serde_json::from_value::<ProposedPlan>(plan) else {
                    return;
                };
                match self
                    .detail
                    .proposed_plans
                    .iter_mut()
                    .find(|p| p.id == plan.id)
                {
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
        if let Some(existing) = self
            .detail
            .messages
            .iter_mut()
            .find(|m| m.id == incoming.message_id)
        {
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
        mine.has_actionable_proposed_plan = shell.has_actionable_proposed_plan;
        mine.background_liveness = shell.background_liveness.clone();
        mine.session = shell.session.clone();
        mine.branch = shell.branch.clone();
        mine.worktree_path = shell.worktree_path.clone();
        mine.pull_requests = shell.pull_requests.clone();
        mine.branch_pull_request = shell.branch_pull_request.clone();
        mine.settled_at = shell.settled_at.clone();
        mine.settled_override = shell.settled_override.clone();
        mine.unsettled_at = shell.unsettled_at.clone();
        mine.snoozed_until = shell.snoozed_until.clone();
        mine.pinned_at = shell.pinned_at.clone();
        mine.pin_order_key = shell.pin_order_key.clone();
        mine.archived_at = shell.archived_at.clone();
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

    /// Background tasks the agent started that have not reported an end. A task is done
    /// once any of its activities carries a status or an `endedAt`; monitors and
    /// backgrounded commands outlive their turn, so the turn is not used to settle them.
    pub fn running_tasks(&self) -> Vec<RunningTask> {
        let mut open: Vec<RunningTask> = Vec::new();
        for activity in &self.detail.activities {
            if !activity.kind.starts_with("task.") {
                continue;
            }
            let Some(id) = activity.str("taskId") else {
                continue;
            };
            let payload = &activity.payload;
            let ended = activity.kind == "task.completed"
                || payload.get("status").is_some_and(|v| !v.is_null())
                || payload.get("endedAt").is_some_and(|v| !v.is_null());
            if ended {
                open.retain(|task| task.id != id);
                continue;
            }
            match open.iter_mut().find(|task| task.id == id) {
                Some(task) => {
                    if let Some(title) = activity.str("title") {
                        task.title = title.to_string();
                    }
                    task.backgrounded |= payload
                        .get("isBackgrounded")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                }
                None => open.push(RunningTask {
                    id: id.to_string(),
                    title: activity
                        .str("title")
                        .or_else(|| activity.str("detail"))
                        .unwrap_or("background task")
                        .to_string(),
                    task_type: activity.str("taskType").unwrap_or("").to_string(),
                    agent_kind: activity.str("agentKind").unwrap_or("").to_string(),
                    started_at: activity.created_at.clone(),
                    turn_id: activity.turn_id.clone(),
                    backgrounded: payload
                        .get("isBackgrounded")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                }),
            }
        }
        open
    }

    pub fn pending_approvals(&self) -> Vec<PendingApproval> {
        let mut pending: Vec<PendingApproval> = Vec::new();
        for activity in &self.detail.activities {
            match activity.kind.as_str() {
                "approval.requested" => {
                    let Some(request_id) = activity.str("requestId") else {
                        continue;
                    };
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
        let mut pending: Vec<PendingUserInput> = Vec::new();
        for activity in &self.detail.activities {
            match activity.kind.as_str() {
                "user-input.requested" => {
                    let Some(request_id) = activity.str("requestId") else {
                        continue;
                    };
                    let questions = parse_questions(activity.payload.get("questions"));
                    if questions.is_empty() {
                        continue;
                    }
                    pending.retain(|p| p.request_id != request_id);
                    pending.push(PendingUserInput {
                        request_id: request_id.to_string(),
                        questions,
                        dismissible: activity.str("responseMode") == Some("message"),
                    });
                }
                "user-input.resolved" => {
                    if let Some(request_id) = activity.str("requestId") {
                        pending.retain(|p| p.request_id != request_id);
                    }
                }
                "provider.user-input.respond.failed" => {
                    let detail = activity.str("detail").unwrap_or("").to_lowercase();
                    if (detail.contains("stale pending") || detail.contains("unknown pending"))
                        && let Some(request_id) = activity.str("requestId")
                    {
                        pending.retain(|p| p.request_id != request_id);
                    }
                }
                _ => {}
            }
        }
        pending.into_iter().next()
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

    fn task_event(sequence: u64, kind: &str, payload: serde_json::Value) -> Event {
        serde_json::from_value(json!({
            "sequence": sequence, "eventId": format!("e{sequence}"),
            "type": "thread.activity-appended",
            "payload": {"threadId": "t1", "activity": {
                "id": format!("a{sequence}"), "tone": "info", "kind": kind,
                "summary": "", "payload": payload, "turnId": "turn",
                "createdAt": "2026-01-01T00:00:00Z"
            }}
        }))
        .unwrap()
    }

    #[test]
    fn running_tasks_track_unfinished_background_work() {
        let mut state = thread();
        state.apply_event(task_event(
            11,
            "task.started",
            json!({"taskId": "t-1", "title": "watch build", "taskType": "local_bash",
                   "agentKind": "background"}),
        ));
        state.apply_event(task_event(
            12,
            "task.started",
            json!({"taskId": "t-2", "title": "run tests", "taskType": "local_bash",
                   "agentKind": "background"}),
        ));
        let running = state.running_tasks();
        assert_eq!(running.len(), 2);
        assert_eq!(running[0].title, "watch build");
        assert!(!running[0].backgrounded);

        // An update without an end keeps it running and can refine the title.
        state.apply_event(task_event(
            13,
            "task.updated",
            json!({"taskId": "t-1", "title": "watch build (rebuilding)", "isBackgrounded": true}),
        ));
        let running = state.running_tasks();
        assert_eq!(running.len(), 2);
        assert_eq!(running[0].title, "watch build (rebuilding)");
        assert!(running[0].backgrounded);

        // An update carrying a status and an end settles it, as does task.completed.
        state.apply_event(task_event(
            14,
            "task.updated",
            json!({"taskId": "t-1", "status": "completed", "endedAt": "2026-01-01T00:05:00Z"}),
        ));
        state.apply_event(task_event(
            15,
            "task.completed",
            json!({"taskId": "t-2", "status": "failed"}),
        ));
        assert!(state.running_tasks().is_empty());
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

#[cfg(test)]
mod question_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_questions_with_defaults() {
        let questions = parse_questions(Some(&json!([
            {"id": "Which color?", "header": "Color", "question": "Which color?",
             "options": [{"label": "Red", "description": "warm"}, {"label": "Blue", "description": "", "value": "b"}]},
            {"question": "Anything else?", "options": [], "allowCustomAnswer": true, "multiSelect": true},
            {"question": "dropped", "options": [], "allowCustomAnswer": false}
        ])));
        assert_eq!(questions.len(), 2);
        assert_eq!(questions[0].options[0].value, "Red");
        assert_eq!(questions[0].options[1].value, "b");
        assert!(questions[0].allow_custom);
        assert_eq!(questions[1].id, "Anything else?");
        assert!(questions[1].multi_select);
    }
}

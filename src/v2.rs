//! Orchestration protocol 2: what the server sends, and the reading of it the rest of
//! tria draws from.
//!
//! The server keeps a thread as a projection — runs, turn items, runtime requests,
//! plans, checkpoints — and streams it as upserts of those entities. The timeline and the
//! panes read an older model: messages, activities, proposed plans, and checkpoints keyed
//! by turn. This module holds the projection, applies the stream to it, and derives that
//! model from it. A run is what the model calls a turn.

use std::collections::HashSet;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::model::{
    Activity, Attachment, Checkpoint, Event, LatestTurn, Message, ModelSelection, Project,
    ProposedPlan, PullRequest, Session, ShellItem, ShellSnapshot, ThreadDetail, ThreadDetailPage,
    ThreadDetailSnapshot, ThreadItem, ThreadShell,
};

/// The protocol this client speaks. The server refuses a socket that names any other.
pub const PROTOCOL: u32 = 2;
/// Where the socket URL names the protocol.
pub const PROTOCOL_QUERY: &str = "orchestrationProtocol";
/// Where an HTTP request to the orchestration API names it.
pub const PROTOCOL_HEADER: &str = "x-t3-orchestration-protocol";

// ── Shell ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShellThread {
    id: String,
    project_id: String,
    title: String,
    model_selection: ModelSelection,
    #[serde(default)]
    runtime_mode: Option<String>,
    #[serde(default)]
    interaction_mode: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    worktree_path: Option<String>,
    #[serde(default)]
    pull_requests: Vec<PullRequest>,
    #[serde(default)]
    branch_pull_request: Option<crate::model::BranchPullRequest>,
    #[serde(default)]
    latest_run_id: Option<String>,
    #[serde(default)]
    latest_run_requested_at: Option<String>,
    #[serde(default)]
    latest_run_started_at: Option<String>,
    #[serde(default)]
    latest_run_completed_at: Option<String>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    pending_runtime_request: Option<PendingRequestSummary>,
    #[serde(default)]
    latest_user_message_at: Option<String>,
    #[serde(default)]
    has_actionable_proposed_plan: bool,
    #[serde(default)]
    pending_background_tasks: Vec<crate::model::BackgroundTask>,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    archived_at: Option<String>,
    #[serde(default)]
    lineage: Option<Lineage>,
    #[serde(default)]
    settled_override: Option<String>,
    #[serde(default)]
    settled_at: Option<String>,
    #[serde(default)]
    unsettled_at: Option<String>,
    #[serde(default)]
    snoozed_until: Option<String>,
    #[serde(default)]
    pinned_at: Option<String>,
    #[serde(default)]
    pin_order_key: Option<String>,
}

/// Where a thread came from: forked off another, or run by another's provider as a subagent.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lineage {
    #[serde(default)]
    relationship_to_parent: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingRequestSummary {
    kind: String,
}

/// Whether a run in this state is still going, queued and waiting included.
fn run_is_live(status: &str) -> bool {
    matches!(
        status,
        "preparing" | "queued" | "starting" | "running" | "waiting"
    )
}

/// A run's state as a turn's: `running`, `completed`, `interrupted`, or `error`.
fn turn_state(status: &str) -> &'static str {
    match status {
        s if run_is_live(s) => "running",
        "failed" => "error",
        "interrupted" | "cancelled" => "interrupted",
        _ => "completed",
    }
}

/// A runtime request the user answers with a decision, as against a question.
fn is_approval_kind(kind: &str) -> bool {
    matches!(
        kind,
        "command" | "file-read" | "file-change" | "mcp-elicitation" | "permission"
    )
}

impl From<ShellThread> for ThreadShell {
    fn from(thread: ShellThread) -> Self {
        // A rolled-back run is gone from the thread, and is no turn of it.
        let latest_run = thread
            .latest_run_id
            .filter(|_| thread.status != "rolled_back");
        let latest_turn = latest_run.map(|turn_id| LatestTurn {
            turn_id,
            state: turn_state(&thread.status).to_string(),
            requested_at: thread.latest_run_requested_at.unwrap_or_default(),
            started_at: thread.latest_run_started_at,
            completed_at: thread.latest_run_completed_at,
        });
        let session = Some(Session {
            status: match thread.status.as_str() {
                s if run_is_live(s) => "running",
                "failed" => "error",
                "interrupted" => "interrupted",
                _ => "ready",
            }
            .to_string(),
            provider_name: None,
            active_turn_id: None,
            last_error: thread.last_error,
        });
        let pending = thread
            .pending_runtime_request
            .as_ref()
            .map(|r| r.kind.as_str());
        let background_liveness = if thread.pending_background_tasks.is_empty() {
            None
        } else if thread
            .pending_background_tasks
            .iter()
            .all(|task| task.kind == "monitor")
        {
            Some("monitoring".to_string())
        } else {
            Some("working".to_string())
        };
        ThreadShell {
            id: thread.id,
            project_id: thread.project_id,
            title: thread.title,
            model_selection: thread.model_selection,
            runtime_mode: thread
                .runtime_mode
                .unwrap_or_else(|| "full-access".to_string()),
            interaction_mode: thread
                .interaction_mode
                .unwrap_or_else(|| "default".to_string()),
            branch: thread.branch,
            worktree_path: thread.worktree_path,
            pull_requests: thread.pull_requests,
            branch_pull_request: thread.branch_pull_request,
            latest_turn,
            created_at: thread.created_at,
            updated_at: thread.updated_at,
            archived_at: thread.archived_at,
            subagent: thread
                .lineage
                .and_then(|lineage| lineage.relationship_to_parent)
                .is_some_and(|relationship| relationship == "subagent"),
            pinned_at: thread.pinned_at,
            pin_order_key: thread.pin_order_key,
            settled_at: thread.settled_at,
            settled_override: thread.settled_override,
            unsettled_at: thread.unsettled_at,
            snoozed_until: thread.snoozed_until,
            has_actionable_proposed_plan: thread.has_actionable_proposed_plan,
            background_liveness,
            background_tasks: thread.pending_background_tasks,
            session,
            latest_user_message_at: thread.latest_user_message_at,
            has_pending_approvals: pending.is_some_and(is_approval_kind),
            has_pending_user_input: pending == Some("user_input"),
            plan_progress: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireShellSnapshot {
    snapshot_sequence: u64,
    #[serde(default)]
    projects: Vec<Project>,
    #[serde(default)]
    threads: Vec<ShellThread>,
    #[serde(default)]
    archived_threads: Vec<ShellThread>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind")]
#[allow(clippy::large_enum_variant)]
enum WireShellItem {
    #[serde(rename = "synchronized")]
    Synchronized,
    #[serde(rename = "snapshot")]
    Snapshot {
        snapshot: WireShellSnapshot,
        /// Present on a refresh of the projects' repository identities, which carries
        /// no threads and replaces nothing.
        #[serde(default, rename = "resolvedRepositoryIdentityRoots")]
        enrichment: Option<Value>,
    },
    #[serde(rename = "project.updated")]
    ProjectUpdated { sequence: u64, project: Project },
    #[serde(rename = "project.removed")]
    ProjectRemoved {
        sequence: u64,
        #[serde(rename = "projectId")]
        project_id: String,
    },
    #[serde(rename = "thread.updated")]
    ThreadUpdated { sequence: u64, thread: ShellThread },
    #[serde(rename = "thread.removed")]
    ThreadRemoved {
        sequence: u64,
        #[serde(rename = "threadId")]
        thread_id: String,
    },
    #[serde(other)]
    Unknown,
}

/// Read one item of `orchestration.subscribeShell`.
pub fn shell_item(value: Value) -> Result<ShellItem> {
    Ok(match serde_json::from_value::<WireShellItem>(value)? {
        WireShellItem::Synchronized => ShellItem::Synchronized,
        WireShellItem::Snapshot {
            enrichment: Some(_),
            ..
        } => ShellItem::Unknown,
        WireShellItem::Snapshot { snapshot, .. } => ShellItem::Snapshot {
            snapshot: ShellSnapshot {
                snapshot_sequence: snapshot.snapshot_sequence,
                projects: snapshot.projects,
                threads: snapshot
                    .threads
                    .into_iter()
                    .chain(snapshot.archived_threads)
                    .map(ThreadShell::from)
                    .collect(),
            },
        },
        WireShellItem::ProjectUpdated { sequence, project } => {
            ShellItem::ProjectUpserted { sequence, project }
        }
        WireShellItem::ProjectRemoved {
            sequence,
            project_id,
        } => ShellItem::ProjectRemoved {
            sequence,
            project_id,
        },
        WireShellItem::ThreadUpdated { sequence, thread } => ShellItem::ThreadUpserted {
            sequence,
            thread: thread.into(),
        },
        WireShellItem::ThreadRemoved {
            sequence,
            thread_id,
        } => ShellItem::ThreadRemoved {
            sequence,
            thread_id,
        },
        WireShellItem::Unknown => ShellItem::Unknown,
    })
}

// ── Thread projection ────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: String,
    #[serde(default)]
    pub ordinal: u64,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub requested_at: String,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub work_started_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    pub id: String,
    pub run_id: String,
    #[serde(default)]
    pub root_node_id: Option<String>,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeRequest {
    pub id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub decision: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub id: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderSession {
    pub id: String,
}

/// A rollback the server gave up on, with why.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackFailure {
    pub request_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppThread {
    #[serde(default)]
    rollback_failure: Option<RollbackFailure>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointRecord {
    pub id: String,
    pub scope_id: String,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub parent_checkpoint_id: Option<String>,
    #[serde(default)]
    pub app_run_ordinal: Option<u32>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub captured_at: String,
}

/// One entry of a thread's timeline. The fields every type shares are read out; the
/// rest stay as the server sent them, to be read by type.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnItem {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub ordinal: u64,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub updated_at: String,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

impl TurnItem {
    fn get(&self, key: &str) -> Option<&Value> {
        self.rest.get(key).filter(|value| !value.is_null())
    }

    fn str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }
}

/// A timeline row: an item and where it sits. Rows of a forked thread can come from
/// the thread it was forked from.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    #[serde(default)]
    pub visibility: String,
    pub item: TurnItem,
}

/// The parts of a thread's projection this client reads.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Projection {
    #[serde(default)]
    thread: AppThread,
    #[serde(default)]
    pub runs: Vec<Run>,
    #[serde(default)]
    pub attempts: Vec<Attempt>,
    #[serde(default)]
    pub provider_sessions: Vec<ProviderSession>,
    #[serde(default)]
    pub runtime_requests: Vec<RuntimeRequest>,
    #[serde(default)]
    pub plans: Vec<Plan>,
    #[serde(default)]
    pub checkpoints: Vec<CheckpointRecord>,
    #[serde(default, rename = "visibleTurnItems")]
    pub rows: Vec<Row>,
    /// Runs an interrupt was asked for in. Only these keep the result of the interrupt
    /// a superseded attempt left behind.
    #[serde(skip)]
    interrupted_runs: HashSet<String>,
    /// The snapshot was a window onto the newest history rather than all of it.
    #[serde(skip)]
    partial: bool,
    /// The newest item the server had when the window was cut.
    #[serde(skip)]
    latest_local_ordinal: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotTurnItem {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    run_id: Option<String>,
}

fn upsert<T>(items: &mut Vec<T>, item: T, id: impl Fn(&T) -> &str) {
    match items.iter_mut().find(|existing| id(existing) == id(&item)) {
        Some(existing) => *existing = item,
        None => items.push(item),
    }
}

impl Projection {
    /// Read a snapshot's projection, given whether it is a window.
    fn from_snapshot(
        projection: Value,
        partial: bool,
        latest_local_ordinal: Option<u64>,
    ) -> Result<Self> {
        let interrupted_runs = projection
            .get("turnItems")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| serde_json::from_value::<SnapshotTurnItem>(item.clone()).ok())
            .filter(|item| item.kind == "run_interrupt_request")
            .filter_map(|item| item.run_id)
            .collect();
        let mut out: Self = serde_json::from_value(projection).context("thread projection")?;
        out.interrupted_runs = interrupted_runs;
        out.partial = partial;
        out.latest_local_ordinal = latest_local_ordinal;
        Ok(out)
    }

    fn run(&self, id: &str) -> Option<&Run> {
        self.runs.iter().find(|run| run.id == id)
    }

    /// Whether a run was undone by a rollback.
    pub fn is_rolled_back(&self, id: &str) -> bool {
        self.run(id).is_some_and(|run| run.status == "rolled_back")
    }

    /// Whether an item belongs in the timeline. A rolled-back run's items are gone; a
    /// queued message whose run was cancelled never reached the provider; and an
    /// interrupt's result left by an attempt that was replaced is noise, unless the
    /// interrupt was asked for.
    fn is_visible(&self, item: &TurnItem) -> bool {
        let status = item
            .run_id
            .as_deref()
            .and_then(|id| self.run(id))
            .map(|run| run.status.as_str());
        if status == Some("rolled_back") {
            return false;
        }
        if status == Some("cancelled")
            && item.kind == "user_message"
            && item.str("inputIntent") == Some("queued_turn")
        {
            return false;
        }
        if item.kind == "run_interrupt_result"
            && let (Some(run_id), Some(node_id)) = (&item.run_id, &item.node_id)
        {
            let superseded = self.attempts.iter().any(|attempt| {
                attempt.status == "superseded"
                    && &attempt.run_id == run_id
                    && attempt.root_node_id.as_ref() == Some(node_id)
            });
            if superseded && !self.interrupted_runs.contains(run_id) {
                return false;
            }
        }
        true
    }

    /// Drop the local rows whose items no longer belong, after a run or attempt changed.
    fn recheck_rows(&mut self) {
        let rows = std::mem::take(&mut self.rows);
        self.rows = rows
            .into_iter()
            .filter(|row| row.visibility != "local" || self.is_visible(&row.item))
            .collect();
    }

    /// Whether an item the window does not hold is older history, which arrives with
    /// the page it is on rather than now.
    fn is_outside_window(&self, item: &TurnItem) -> bool {
        if !self.partial {
            return false;
        }
        if self
            .latest_local_ordinal
            .is_some_and(|latest| item.ordinal <= latest)
        {
            return true;
        }
        self.rows
            .iter()
            .filter(|row| row.visibility == "local")
            .map(|row| row.item.ordinal)
            .min()
            .is_some_and(|oldest| item.ordinal < oldest)
    }

    fn upsert_item(&mut self, item: TurnItem) {
        if item.kind == "run_interrupt_request"
            && let Some(run_id) = &item.run_id
        {
            self.interrupted_runs.insert(run_id.clone());
            self.recheck_rows();
        }
        let index = self.rows.iter().position(|row| row.item.id == item.id);
        if !self.is_visible(&item) {
            if let Some(index) = index {
                self.rows.remove(index);
            }
            return;
        }
        if let Some(index) = index {
            self.rows.remove(index);
        } else if self.is_outside_window(&item) {
            return;
        }
        // Among the thread's own rows, by ordinal and then id, the order the server
        // keeps them in.
        let at = self
            .rows
            .iter()
            .position(|row| {
                row.visibility == "local"
                    && (row.item.ordinal > item.ordinal
                        || (row.item.ordinal == item.ordinal && row.item.id > item.id))
            })
            .unwrap_or(self.rows.len());
        self.rows.insert(
            at,
            Row {
                visibility: "local".to_string(),
                item,
            },
        );
    }

    /// Apply one event of the thread's stream. Says whether anything drawn from the
    /// projection could have changed.
    pub fn apply(&mut self, kind: &str, payload: Value) -> bool {
        fn read<T: serde::de::DeserializeOwned>(payload: Value) -> Option<T> {
            serde_json::from_value(payload)
                .map_err(|err| tracing::warn!(%err, "undecodable thread event payload"))
                .ok()
        }
        match kind {
            kind if kind.starts_with("thread.") => {
                let Some(thread) = read::<AppThread>(payload) else {
                    return false;
                };
                self.thread = thread;
            }
            "provider-session.attached" | "provider-session.updated" => {
                let Some(session) = read::<ProviderSession>(payload) else {
                    return false;
                };
                upsert(&mut self.provider_sessions, session, |session| &session.id);
            }
            "provider-session.detached" => {
                let Some(id) = payload.get("providerSessionId").and_then(Value::as_str) else {
                    return false;
                };
                self.provider_sessions.retain(|session| session.id != id);
            }
            "run.created" | "run.updated" => {
                let Some(run) = read::<Run>(payload) else {
                    return false;
                };
                upsert(&mut self.runs, run, |run| &run.id);
                self.recheck_rows();
            }
            "run-attempt.created" | "run-attempt.updated" => {
                let Some(attempt) = read::<Attempt>(payload) else {
                    return false;
                };
                upsert(&mut self.attempts, attempt, |attempt| &attempt.id);
                self.recheck_rows();
            }
            "runtime-request.updated" => {
                let Some(request) = read::<RuntimeRequest>(payload) else {
                    return false;
                };
                upsert(&mut self.runtime_requests, request, |request| &request.id);
            }
            "plan.updated" => {
                let Some(plan) = read::<Plan>(payload) else {
                    return false;
                };
                upsert(&mut self.plans, plan, |plan| &plan.id);
            }
            "checkpoint.captured" => {
                let Some(checkpoint) = read::<CheckpointRecord>(payload) else {
                    return false;
                };
                upsert(&mut self.checkpoints, checkpoint, |checkpoint| {
                    &checkpoint.id
                });
            }
            "turn-item.updated" => {
                let Some(item) = read::<TurnItem>(payload) else {
                    return false;
                };
                self.upsert_item(item);
            }
            _ => return false,
        }
        true
    }

    /// Put a page of older history in front of what is loaded, leaving out rows that
    /// are here already.
    pub fn prepend(&mut self, rows: Vec<Row>) {
        let held: HashSet<&str> = self.rows.iter().map(|row| row.item.id.as_str()).collect();
        let mut older: Vec<Row> = rows
            .into_iter()
            .filter(|row| !held.contains(row.item.id.as_str()) && self.is_visible(&row.item))
            .collect();
        older.append(&mut self.rows);
        self.rows = older;
    }

    /// The thread as the timeline reads it. `shell` is the thread's list entry, which
    /// carries what the projection does not, such as its title and pull requests.
    /// The thread's newest run that still stands, as its latest turn.
    pub fn latest_turn(&self) -> Option<LatestTurn> {
        let run = self
            .runs
            .iter()
            .filter(|run| run.status != "rolled_back")
            .max_by_key(|run| run.ordinal)?;
        Some(LatestTurn {
            turn_id: run.id.clone(),
            state: turn_state(&run.status).to_string(),
            requested_at: run.requested_at.clone(),
            started_at: run.work_started_at.clone().or(run.started_at.clone()),
            completed_at: run.completed_at.clone(),
        })
    }

    pub fn detail(&self, shell: &ThreadShell) -> ThreadDetail {
        let mut shell = shell.clone();
        if let Some(turn) = self.latest_turn() {
            shell.latest_turn = Some(turn);
        }
        let mut detail = ThreadDetail {
            shell,
            messages: Vec::new(),
            activities: Vec::new(),
            proposed_plans: Vec::new(),
            checkpoints: self.turn_checkpoints(),
        };
        // The timeline orders by time. Items share milliseconds and providers report
        // some late, so each is stamped no earlier than the one before it, which keeps
        // the order the server gave them.
        let mut clock = String::new();
        for row in &self.rows {
            let item = &row.item;
            let at = item.started_at.as_deref().unwrap_or(&item.updated_at);
            if at > clock.as_str() {
                clock = at.to_string();
            }
            self.derive(item, &clock, &mut detail);
        }
        if let Some(failure) = &self.thread.rollback_failure {
            detail.activities.push(Activity {
                id: format!("rollback-failure:{}", failure.request_id),
                tone: "error".to_string(),
                kind: "checkpoint.revert.failed".to_string(),
                summary: "Rollback failed".to_string(),
                payload: json!({ "detail": failure.message }),
                turn_id: None,
                sequence: None,
                created_at: clock,
            });
        }
        detail
    }

    fn derive(&self, item: &TurnItem, at: &str, detail: &mut ThreadDetail) {
        let activity = |kind: &str, tone: &str, summary: String, payload: Value| Activity {
            id: item.id.clone(),
            tone: tone.to_string(),
            kind: kind.to_string(),
            summary,
            payload,
            turn_id: item.run_id.clone(),
            sequence: None,
            created_at: at.to_string(),
        };
        match item.kind.as_str() {
            "user_message" | "assistant_message" => detail.messages.push(Message {
                id: item.str("messageId").unwrap_or(&item.id).to_string(),
                role: if item.kind == "user_message" {
                    "user"
                } else {
                    "assistant"
                }
                .to_string(),
                text: item.str("text").unwrap_or("").to_string(),
                attachments: item
                    .get("attachments")
                    .and_then(|value| serde_json::from_value::<Vec<Attachment>>(value.clone()).ok())
                    .unwrap_or_default(),
                turn_id: item.run_id.clone(),
                streaming: item.get("streaming").and_then(Value::as_bool) == Some(true),
                created_at: at.to_string(),
                updated_at: item.updated_at.clone(),
            }),
            "proposed_plan" => {
                let id = item.str("planId").unwrap_or(&item.id);
                let status = self
                    .plans
                    .iter()
                    .find(|plan| plan.id == id)
                    .map(|plan| plan.status.as_str());
                detail.proposed_plans.push(ProposedPlan {
                    id: id.to_string(),
                    turn_id: item.run_id.clone(),
                    plan_markdown: item.str("markdown").unwrap_or("").to_string(),
                    // Only an active plan is still waiting to be built.
                    implemented_at: match status {
                        Some("completed" | "superseded") => Some(item.updated_at.clone()),
                        _ => None,
                    },
                    created_at: at.to_string(),
                });
            }
            "todo_list" => {
                let steps: Vec<Value> = item
                    .get("steps")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|step| {
                        json!({
                            "step": step.get("text").and_then(Value::as_str).unwrap_or(""),
                            "status": match step.get("status").and_then(Value::as_str) {
                                Some("running") => "inProgress",
                                Some("completed") => "completed",
                                _ => "pending",
                            },
                        })
                    })
                    .collect();
                detail.activities.push(activity(
                    "turn.plan.updated",
                    "info",
                    "Plan updated".to_string(),
                    json!({ "plan": steps, "explanation": item.get("explanation") }),
                ));
            }
            "command_execution" | "file_change" | "file_search" | "web_search" | "dynamic_tool" => {
                let (item_type, title, data) = tool(item);
                let summary = tool_detail(item);
                // Providers title a call with what it was given, `Read /a/file`, and the
                // row draws that as its detail already.
                let title = match &summary {
                    Some(said) if title.len() > said.len() && title.ends_with(said.as_str()) => {
                        title[..title.len() - said.len()].trim_end().to_string()
                    }
                    _ => title,
                };
                let done = !matches!(
                    item.status.as_str(),
                    "pending" | "running" | "waiting" | "idle"
                );
                detail.activities.push(activity(
                    if done {
                        "tool.completed"
                    } else {
                        "tool.started"
                    },
                    "tool",
                    title.clone(),
                    json!({
                        "itemType": item_type,
                        "toolCallId": item.id,
                        // What the provider calls it, which is the id of the background
                        // task it started, if it started one.
                        "nativeId": item
                            .get("nativeItemRef")
                            .and_then(|native| native.get("nativeId"))
                            .and_then(Value::as_str),
                        "outputOmitted": item.get("outputOmitted").and_then(Value::as_bool) == Some(true),
                        "updatedAt": item.updated_at,
                        "status": tool_status(&item.status),
                        "title": title,
                        "detail": summary,
                        "data": data,
                    }),
                ));
            }
            "approval_request" => {
                let request_id = item.str("requestId").unwrap_or(&item.id);
                let kind = item.str("requestKind").unwrap_or("approval");
                detail.activities.push(activity(
                    "approval.requested",
                    "approval",
                    item.title
                        .clone()
                        .unwrap_or_else(|| format!("Approve {kind}")),
                    json!({
                        "requestId": request_id,
                        "requestKind": kind,
                        "detail": item.str("prompt"),
                        "options": item.get("options"),
                    }),
                ));
                if let Some(request) = self.settled_request(request_id) {
                    let mut resolved = activity(
                        "approval.resolved",
                        "approval",
                        String::new(),
                        json!({
                            "requestId": request_id,
                            "decision": request.decision.as_deref().unwrap_or(&request.status),
                        }),
                    );
                    resolved.id = format!("{}:resolved", item.id);
                    detail.activities.push(resolved);
                }
            }
            "user_input_request" => {
                let request_id = item.str("requestId").unwrap_or(&item.id);
                detail.activities.push(activity(
                    "user-input.requested",
                    "approval",
                    "Agent asked a question".to_string(),
                    json!({
                        "requestId": request_id,
                        "questions": item.get("questions"),
                        "responseMode": item.str("responseMode"),
                    }),
                ));
                if self.settled_request(request_id).is_some() || item.status == "completed" {
                    let mut resolved = activity(
                        "user-input.resolved",
                        "approval",
                        String::new(),
                        json!({ "requestId": request_id }),
                    );
                    resolved.id = format!("{}:resolved", item.id);
                    detail.activities.push(resolved);
                }
            }
            "subagent" => {
                let prompt = item.str("prompt").unwrap_or("");
                let title = item
                    .title
                    .clone()
                    .filter(|title| !title.is_empty())
                    .unwrap_or_else(|| prompt.lines().next().unwrap_or("").to_string());
                let done = matches!(
                    item.status.as_str(),
                    "completed" | "failed" | "cancelled" | "interrupted"
                );
                let mut payload = json!({
                    "taskId": item.str("subagentId").unwrap_or(&item.id),
                    "agentKind": "agent",
                    "title": title,
                    "description": prompt,
                    "summary": item.str("result").or(item.str("progress")),
                    "childThreadId": item.str("childThreadId"),
                });
                if done {
                    payload["status"] = json!(tool_status(&item.status));
                    payload["endedAt"] = json!(item.completed_at.as_deref().unwrap_or(at));
                }
                detail.activities.push(activity(
                    if done {
                        "task.completed"
                    } else {
                        "task.started"
                    },
                    "info",
                    title,
                    payload,
                ));
            }
            "error" => {
                let failure = item.get("failure");
                let message = failure
                    .and_then(|failure| failure.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("The provider failed");
                detail.activities.push(activity(
                    "runtime.error",
                    "error",
                    first_line(message).to_string(),
                    json!({ "message": message }),
                ));
            }
            "notification" => {
                let summary = item.str("summary").unwrap_or("Notification");
                detail.activities.push(activity(
                    "runtime.note",
                    "info",
                    summary.to_string(),
                    // The item a background task's news is about: the call that started it.
                    json!({ "message": item.str("detail"), "parentItemId": item.str("parentItemId") }),
                ));
            }
            "system_notice" | "run_interrupt_request" | "run_interrupt_result" => {
                let message = item.str("message").unwrap_or("");
                detail.activities.push(activity(
                    "runtime.note",
                    "info",
                    first_line(message).to_string(),
                    json!({ "message": message }),
                ));
            }
            "compaction" => detail.activities.push(activity(
                "context-compaction",
                "info",
                "Context compacted".to_string(),
                json!({ "summary": item.str("summary") }),
            )),
            "handoff" => {
                let to = item.str("toModel").or(item.str("toProviderInstanceId"));
                detail.activities.push(activity(
                    "runtime.note",
                    "info",
                    match to {
                        Some(to) => format!("Handed off to {to}"),
                        None => "Handed off".to_string(),
                    },
                    json!({ "message": item.str("summary") }),
                ));
            }
            // Reasoning, checkpoints, forks, and secret requests have no row of their own.
            _ => {}
        }
    }

    /// The request, once it is no longer waiting on an answer.
    fn settled_request(&self, id: &str) -> Option<&RuntimeRequest> {
        self.runtime_requests
            .iter()
            .find(|request| request.id == id && request.status != "pending")
    }

    /// One checkpoint per run that left one, as the turn's end. The checkpoint before
    /// it — the one rolling back to undoes the turn — goes with it.
    fn turn_checkpoints(&self) -> Vec<Checkpoint> {
        self.checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.status == "ready")
            .filter_map(|checkpoint| {
                let run_id = checkpoint.run_id.as_ref()?;
                let run = self.run(run_id)?;
                if run.status == "rolled_back" {
                    return None;
                }
                Some(Checkpoint {
                    turn_id: run_id.clone(),
                    completed_at: run
                        .completed_at
                        .clone()
                        .unwrap_or_else(|| checkpoint.captured_at.clone()),
                    checkpoint_turn_count: checkpoint.app_run_ordinal,
                    rollback: checkpoint.parent_checkpoint_id.clone().map(|parent| {
                        crate::model::RollbackTarget {
                            scope_id: checkpoint.scope_id.clone(),
                            checkpoint_id: parent,
                        }
                    }),
                })
            })
            .collect()
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

/// A turn item's status as a tool row's.
fn tool_status(status: &str) -> &'static str {
    match status {
        "completed" => "completed",
        "failed" => "failed",
        "cancelled" | "interrupted" => "declined",
        _ => "inProgress",
    }
}

/// What a tool call was given, read for a one-line summary: the path, pattern, or
/// command it names, whichever comes first.
fn input_summary(input: &Value) -> Option<String> {
    match input {
        Value::String(text) => Some(text.clone()),
        Value::Object(fields) => [
            "file_path",
            "path",
            "pattern",
            "command",
            "url",
            "query",
            "description",
            "prompt",
        ]
        .iter()
        .find_map(|key| fields.get(*key).and_then(Value::as_str))
        .map(str::to_string)
        .or_else(|| (!fields.is_empty()).then(|| input.to_string())),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

fn tool_detail(item: &TurnItem) -> Option<String> {
    match item.kind.as_str() {
        "command_execution" => item.str("input").map(str::to_string),
        "file_change" => item.str("fileName").map(str::to_string),
        "file_search" => item.str("pattern").map(str::to_string),
        "web_search" => item
            .get("patterns")
            .and_then(Value::as_array)
            .map(|patterns| {
                patterns
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            }),
        _ => item.get("input").and_then(input_summary),
    }
}

/// A tool item as a tool row: its type as the timeline names it, its title, and the
/// `data` the timeline reads its command, files, and output from.
fn tool(item: &TurnItem) -> (&'static str, String, Value) {
    let named = |fallback: &str| {
        item.title
            .clone()
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| fallback.to_string())
    };
    match item.kind.as_str() {
        "command_execution" => (
            "command_execution",
            named("Ran command"),
            json!({
                "command": item.str("input"),
                "rawOutput": { "content": item.str("output") },
                "exitCode": item.get("exitCode"),
            }),
        ),
        "file_change" => {
            let path = item.str("fileName").unwrap_or("");
            (
                "file_change",
                named("File change"),
                json!({
                    "path": path,
                    "files": [{ "path": path, "diff": item.str("diffStr") }],
                }),
            )
        }
        "file_search" => {
            let found = item.get("results").and_then(Value::as_array).map(Vec::len);
            (
                "file_search",
                named("Searched files"),
                json!({ "rawOutput": { "totalFiles": found } }),
            )
        }
        "web_search" => ("web_search", named("Web search"), json!({})),
        _ => {
            let tool_name = item.str("toolName").unwrap_or("Tool");
            let output = item.get("output").map(|output| match output {
                Value::Object(fields) if fields.contains_key("content") => {
                    fields["content"].clone()
                }
                other => other.clone(),
            });
            if let Some(path) = item.str("viewedImagePath") {
                return (
                    "image_view",
                    named("Image view"),
                    json!({ "imagePath": path, "toolName": tool_name, "result": { "content": output } }),
                );
            }
            (
                "dynamic_tool_call",
                named(tool_name),
                json!({
                    "toolName": tool_name,
                    "input": item.get("input"),
                    "result": { "content": output },
                }),
            )
        }
    }
}

// ── Thread stream ────────────────────────────────────────────────────────

/// Read one item of `orchestration.subscribeThread`. `shell` is the thread's list
/// entry, for the parts of a snapshot the projection does not carry.
pub fn thread_item(value: Value, shell: Option<&ThreadShell>) -> Result<ThreadItem> {
    let kind = value.get("kind").and_then(Value::as_str).unwrap_or("");
    Ok(match kind {
        "synchronized" => ThreadItem::Synchronized,
        "snapshot" => ThreadItem::Snapshot {
            snapshot: snapshot(value, shell)?,
        },
        "event" => {
            let sequence = value
                .get("sequence")
                .and_then(Value::as_u64)
                .context("event without a sequence")?;
            let event = value.get("event").cloned().unwrap_or(Value::Null);
            ThreadItem::Event {
                event: Event {
                    sequence,
                    event_id: event
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    kind: event
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    occurred_at: event
                        .get("occurredAt")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    payload: event.get("payload").cloned().unwrap_or(Value::Null),
                },
            }
        }
        _ => ThreadItem::Unknown,
    })
}

fn snapshot(mut value: Value, shell: Option<&ThreadShell>) -> Result<ThreadDetailSnapshot> {
    let sequence = value
        .get("snapshotSequence")
        .and_then(Value::as_u64)
        .context("snapshot without a sequence")?;
    let has_more = value
        .get("hasMoreHistory")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let cursor = value
        .get("historyCursor")
        .and_then(Value::as_str)
        .map(str::to_string);
    let latest = value.get("latestLocalTurnOrdinal").and_then(Value::as_u64);
    let projection = value
        .get_mut("projection")
        .map(Value::take)
        .context("snapshot without a projection")?;
    let thread = projection.get("thread").cloned().unwrap_or(Value::Null);
    let projection = Projection::from_snapshot(projection, has_more || cursor.is_some(), latest)?;
    let shell = match shell {
        Some(shell) => shell.clone(),
        None => app_thread(thread)?,
    };
    Ok(ThreadDetailSnapshot {
        snapshot_sequence: sequence,
        thread: projection.detail(&shell),
        page: Some(ThreadDetailPage {
            before_cursor: cursor,
            has_more,
        }),
        projection: Some(Box::new(projection)),
    })
}

/// A thread's list entry made from its projection, for a thread the list has not
/// caught up with: one just created, say.
fn app_thread(thread: Value) -> Result<ThreadShell> {
    let mut thread = thread;
    if let Some(fields) = thread.as_object_mut() {
        fields.entry("status").or_insert(json!("idle"));
    }
    Ok(serde_json::from_value::<ShellThread>(thread)
        .context("thread in projection")?
        .into())
}

/// A page of older history, from `GET /api/orchestration/threads/:id/history`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPage {
    #[serde(default)]
    pub items: Vec<Row>,
    #[serde(default)]
    pub next_cursor: Option<String>,
    #[serde(default)]
    pub has_more_history: bool,
}

/// Fetch the page of history before `cursor`.
pub async fn history_page(
    origin: &str,
    token: &str,
    thread_id: &str,
    cursor: &str,
) -> Result<HistoryPage> {
    let mut url = url::Url::parse(origin).context("invalid server origin")?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("server origin cannot take a path"))?
        .extend(["api", "orchestration", "threads", thread_id, "history"]);
    url.query_pairs_mut().append_pair("cursor", cursor);
    let response = reqwest::Client::new()
        .get(url)
        .bearer_auth(token)
        .header(PROTOCOL_HEADER, PROTOCOL.to_string())
        .send()
        .await
        .context("history request failed")?
        .error_for_status()
        .context("history request refused")?;
    response.json().await.context("history page")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell_thread(extra: Value) -> Value {
        let mut thread = json!({
            "id": "thread", "projectId": "project", "title": "A thread",
            "modelSelection": { "instanceId": "codex", "model": "gpt" },
            "runtimeMode": "full-access", "interactionMode": "default",
            "branch": null, "worktreePath": null,
            "latestRunId": null, "status": "idle", "pendingRuntimeRequest": null,
            "hasActionableProposedPlan": false,
            "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z",
            "archivedAt": null, "settledOverride": null, "settledAt": null, "deletedAt": null,
        });
        for (key, value) in extra.as_object().unwrap() {
            thread[key] = value.clone();
        }
        thread
    }

    fn item(id: &str, ordinal: u64, kind: &str, extra: Value) -> Value {
        let mut item = json!({
            "id": id, "threadId": "thread", "runId": "run-1", "nodeId": "node",
            "ordinal": ordinal, "status": "completed", "title": null,
            "startedAt": format!("2026-01-01T00:00:{ordinal:02}.000Z"),
            "completedAt": null, "updatedAt": "2026-01-01T00:01:00.000Z",
            "type": kind,
        });
        for (key, value) in extra.as_object().unwrap() {
            item[key] = value.clone();
        }
        item
    }

    fn row(item: Value) -> Value {
        json!({ "position": 0, "visibility": "local", "sourceThreadId": "thread",
            "sourceItemId": item["id"], "item": item })
    }

    fn snapshot_item(rows: Vec<Value>, runs: Vec<Value>) -> Value {
        json!({
            "kind": "snapshot", "snapshotSequence": 7,
            "projection": {
                "thread": shell_thread(json!({})),
                "runs": runs, "attempts": [], "runtimeRequests": [], "plans": [],
                "checkpoints": [], "turnItems": [], "visibleTurnItems": rows,
            },
        })
    }

    fn open(rows: Vec<Value>, runs: Vec<Value>) -> ThreadDetailSnapshot {
        match thread_item(snapshot_item(rows, runs), None).unwrap() {
            ThreadItem::Snapshot { snapshot } => snapshot,
            other => panic!("not a snapshot: {other:?}"),
        }
    }

    fn run(status: &str) -> Value {
        json!({ "id": "run-1", "ordinal": 1, "status": status,
            "requestedAt": "2026-01-01T00:00:00.000Z", "startedAt": "2026-01-01T00:00:01.000Z",
            "completedAt": null })
    }

    #[test]
    fn shell_items_read_the_renamed_kinds() {
        let item = shell_item(json!({
            "kind": "thread.updated", "sequence": 4, "location": "active",
            "thread": shell_thread(json!({
                "latestRunId": "run-1", "status": "running",
                "latestRunRequestedAt": "2026-01-01T00:00:00.000Z",
                "pendingRuntimeRequest": { "id": "r", "kind": "command", "createdAt": "x" },
            })),
        }))
        .unwrap();
        let ShellItem::ThreadUpserted { sequence, thread } = item else {
            panic!("not a thread update");
        };
        assert_eq!(sequence, 4);
        assert!(thread.is_running());
        assert!(thread.has_pending_approvals);
        assert!(!thread.has_pending_user_input);
        assert_eq!(thread.latest_turn.unwrap().turn_id, "run-1");
    }

    #[test]
    fn a_shell_snapshot_holds_archived_threads_too() {
        let item = shell_item(json!({
            "kind": "snapshot",
            "snapshot": {
                "schemaVersion": 1, "snapshotSequence": 3, "projects": [],
                "threads": [shell_thread(json!({}))],
                "archivedThreads": [shell_thread(json!({ "id": "old", "archivedAt": "2026-01-02T00:00:00.000Z" }))],
            },
        }))
        .unwrap();
        let ShellItem::Snapshot { snapshot } = item else {
            panic!("not a snapshot");
        };
        assert_eq!(snapshot.threads.len(), 2);
        assert!(snapshot.threads[1].archived_at.is_some());
    }

    #[test]
    fn a_subagent_thread_is_marked_and_a_fork_is_not() {
        let thread = |relationship: &str| -> ThreadShell {
            let item = shell_item(json!({
                "kind": "thread.updated", "sequence": 1,
                "thread": shell_thread(json!({ "lineage": {
                    "rootThreadId": "root", "parentThreadId": "root",
                    "relationshipToParent": relationship,
                } })),
            }))
            .unwrap();
            let ShellItem::ThreadUpserted { thread, .. } = item else {
                panic!("not a thread update");
            };
            thread
        };
        assert!(thread("subagent").subagent);
        assert!(!thread("fork").subagent);
    }

    #[test]
    fn an_enrichment_snapshot_replaces_nothing() {
        let item = shell_item(json!({
            "kind": "snapshot", "resolvedRepositoryIdentityRoots": ["/work"],
            "snapshot": { "schemaVersion": 1, "snapshotSequence": 3, "projects": [],
                "threads": [], "archivedThreads": [] },
        }))
        .unwrap();
        assert!(matches!(item, ShellItem::Unknown));
    }

    /// A subagent names the thread the server keeps of its work, which is how it is read.
    #[test]
    fn a_subagent_names_its_thread() {
        let snapshot = open(
            vec![row(item(
                "s",
                1,
                "subagent",
                json!({ "subagentId": "a1", "origin": "provider_native", "driver": "claude",
                    "providerInstanceId": "claude", "childThreadId": "child",
                    "prompt": "Look into it", "result": null, "status": "running" }),
            ))],
            vec![run("running")],
        );
        let agents = crate::state::ThreadState::from_snapshot(snapshot).subagents();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].child_thread_id.as_deref(), Some("child"));
    }

    #[test]
    fn messages_and_tools_come_out_in_the_order_the_server_keeps() {
        let snapshot = open(
            vec![
                row(item(
                    "u",
                    1,
                    "user_message",
                    json!({ "messageId": "m-u", "text": "hi", "attachments": [], "inputIntent": "turn_start" }),
                )),
                row(item(
                    "cmd",
                    2,
                    "command_execution",
                    json!({ "input": "ls", "output": "a\nb" }),
                )),
                // Reported as starting before the command, but it comes after it.
                row(item(
                    "a",
                    3,
                    "assistant_message",
                    json!({ "messageId": "m-a", "text": "two", "streaming": false, "startedAt": "2026-01-01T00:00:00.500Z" }),
                )),
            ],
            vec![run("completed")],
        );
        let detail = &snapshot.thread;
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(detail.messages[0].id, "m-u");
        assert_eq!(detail.messages[0].turn_id.as_deref(), Some("run-1"));
        let tool = &detail.activities[0];
        assert_eq!(tool.kind, "tool.completed");
        assert_eq!(tool.payload["itemType"], "command_execution");
        assert_eq!(tool.payload["data"]["command"], "ls");
        assert_eq!(tool.payload["data"]["rawOutput"]["content"], "a\nb");
        assert!(detail.messages[1].created_at >= tool.created_at);
        assert_eq!(
            detail.shell.latest_turn.as_ref().unwrap().state,
            "completed"
        );
    }

    #[test]
    fn a_tool_titled_with_its_input_says_it_once() {
        let snapshot = open(
            vec![row(item(
                "read",
                1,
                "dynamic_tool",
                json!({ "toolName": "Read",
                "title": "Read /work/Cargo.toml", "input": { "file_path": "/work/Cargo.toml" } }),
            ))],
            vec![run("completed")],
        );
        let tool = &snapshot.thread.activities[0];
        assert_eq!(tool.payload["title"], "Read");
        assert_eq!(tool.payload["detail"], "/work/Cargo.toml");
    }

    #[test]
    fn streamed_items_replace_themselves_and_new_ones_append() {
        let snapshot = open(
            vec![row(item(
                "u",
                1,
                "user_message",
                json!({ "messageId": "m-u", "text": "hi", "attachments": [] }),
            ))],
            vec![run("running")],
        );
        let mut projection = *snapshot.projection.unwrap();
        let streaming = item(
            "a",
            2,
            "assistant_message",
            json!({ "messageId": "m-a", "text": "he", "streaming": true, "status": "running" }),
        );
        assert!(projection.apply("turn-item.updated", streaming));
        let done = item(
            "a",
            2,
            "assistant_message",
            json!({ "messageId": "m-a", "text": "hello", "streaming": false }),
        );
        assert!(projection.apply("turn-item.updated", done));
        let detail = projection.detail(&snapshot.thread.shell);
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(detail.messages[1].text, "hello");
        assert!(!detail.messages[1].streaming);
    }

    #[test]
    fn a_rolled_back_run_is_no_turn_of_the_thread() {
        let item = shell_item(json!({
            "kind": "thread.updated", "sequence": 4, "location": "active",
            "thread": shell_thread(json!({ "latestRunId": "run-2", "status": "rolled_back",
                "latestRunCompletedAt": "2026-01-01T00:00:00.000Z" })),
        }))
        .unwrap();
        let ShellItem::ThreadUpserted { thread, .. } = item else {
            panic!("not a thread update");
        };
        assert!(thread.latest_turn.is_none());
    }

    #[test]
    fn a_rolled_back_run_takes_its_items_with_it() {
        let snapshot = open(
            vec![row(item(
                "u",
                1,
                "user_message",
                json!({ "messageId": "m-u", "text": "hi", "attachments": [] }),
            ))],
            vec![run("completed")],
        );
        let mut projection = *snapshot.projection.unwrap();
        projection.apply("run.updated", run("rolled_back"));
        assert!(
            projection
                .detail(&snapshot.thread.shell)
                .messages
                .is_empty()
        );
    }

    #[test]
    fn approvals_resolve_when_their_request_does() {
        let snapshot = open(
            vec![row(item(
                "ap",
                1,
                "approval_request",
                json!({ "requestId": "req", "requestKind": "command", "prompt": "rm -rf build", "status": "waiting" }),
            ))],
            vec![run("waiting")],
        );
        let mut projection = *snapshot.projection.unwrap();
        let kinds = |projection: &Projection| {
            projection
                .detail(&snapshot.thread.shell)
                .activities
                .iter()
                .map(|activity| activity.kind.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(kinds(&projection), ["approval.requested"]);
        projection.apply(
            "runtime-request.updated",
            json!({ "id": "req", "kind": "command", "status": "resolved", "decision": "accept",
                "responseCapability": { "type": "message" }, "createdAt": "x", "resolvedAt": "y" }),
        );
        assert_eq!(
            kinds(&projection),
            ["approval.requested", "approval.resolved"]
        );
    }

    #[test]
    fn a_window_leaves_older_items_to_their_page() {
        let mut value = snapshot_item(
            vec![row(item(
                "late",
                10,
                "assistant_message",
                json!({ "messageId": "m", "text": "x", "streaming": false }),
            ))],
            vec![run("completed")],
        );
        value["hasMoreHistory"] = json!(true);
        value["historyCursor"] = json!("cursor");
        value["latestLocalTurnOrdinal"] = json!(10);
        let ThreadItem::Snapshot { snapshot } = thread_item(value, None).unwrap() else {
            panic!("not a snapshot");
        };
        assert_eq!(
            snapshot.page.as_ref().unwrap().before_cursor.as_deref(),
            Some("cursor")
        );
        let mut projection = *snapshot.projection.unwrap();
        projection.apply(
            "turn-item.updated",
            item(
                "early",
                2,
                "assistant_message",
                json!({ "messageId": "e", "text": "old", "streaming": false }),
            ),
        );
        assert_eq!(projection.rows.len(), 1);
        projection.apply(
            "turn-item.updated",
            item(
                "new",
                11,
                "assistant_message",
                json!({ "messageId": "n", "text": "new", "streaming": false }),
            ),
        );
        assert_eq!(projection.rows.len(), 2);
        let page: Vec<Row> = serde_json::from_value(json!([row(item(
            "early",
            2,
            "assistant_message",
            json!({ "messageId": "e", "text": "old", "streaming": false })
        ))]))
        .unwrap();
        projection.prepend(page);
        let ids: Vec<_> = projection
            .rows
            .iter()
            .map(|row| row.item.id.as_str())
            .collect();
        assert_eq!(ids, ["early", "late", "new"]);
    }

    #[test]
    fn turns_end_at_their_checkpoint_and_roll_back_to_the_one_before() {
        let mut value = snapshot_item(vec![], vec![run("completed")]);
        value["projection"]["checkpoints"] = json!([
            { "id": "c0", "scopeId": "root", "runId": null, "parentCheckpointId": null,
              "ordinalWithinScope": 0, "appRunOrdinal": null, "status": "ready", "capturedAt": "a" },
            { "id": "c1", "scopeId": "root", "runId": "run-1", "parentCheckpointId": "c0",
              "ordinalWithinScope": 1, "appRunOrdinal": 1, "status": "ready", "capturedAt": "b" },
        ]);
        let ThreadItem::Snapshot { snapshot } = thread_item(value, None).unwrap() else {
            panic!("not a snapshot");
        };
        let checkpoints = &snapshot.thread.checkpoints;
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].turn_id, "run-1");
        assert_eq!(checkpoints[0].checkpoint_turn_count, Some(1));
        let target = checkpoints[0].rollback.as_ref().unwrap();
        assert_eq!(
            (target.scope_id.as_str(), target.checkpoint_id.as_str()),
            ("root", "c0")
        );
    }

    #[test]
    fn a_failed_rollback_is_said_in_the_thread() {
        let snapshot = open(vec![], vec![run("completed")]);
        let mut projection = *snapshot.projection.unwrap();
        let mut thread = shell_thread(json!({}));
        thread["rollbackFailure"] = json!({ "requestId": "cmd", "message": "dirty tree" });
        assert!(projection.apply("thread.metadata-updated", thread));
        let detail = projection.detail(&snapshot.thread.shell);
        let failure = detail.activities.last().unwrap();
        assert_eq!(failure.kind, "checkpoint.revert.failed");
        assert_eq!(failure.str("detail"), Some("dirty tree"));
    }

    #[test]
    fn provider_sessions_come_and_go() {
        let mut projection = Projection::default();
        projection.apply(
            "provider-session.attached",
            json!({ "id": "s1", "status": "ready" }),
        );
        projection.apply(
            "provider-session.attached",
            json!({ "id": "s2", "status": "ready" }),
        );
        projection.apply(
            "provider-session.detached",
            json!({ "providerSessionId": "s1" }),
        );
        let ids: Vec<_> = projection
            .provider_sessions
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(ids, ["s2"]);
    }

    #[test]
    fn unknown_thread_events_are_skipped() {
        let item = thread_item(
            json!({ "kind": "event", "sequence": 9, "event": { "id": "e", "threadId": "thread",
                "type": "something.new", "occurredAt": "x", "payload": {} } }),
            None,
        )
        .unwrap();
        let ThreadItem::Event { event } = item else {
            panic!("not an event");
        };
        let mut projection = Projection::default();
        assert!(!projection.apply(&event.kind, event.payload));
    }
}

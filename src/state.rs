//! Pure reducers for the shell (project and thread list) and one open thread.

use std::{
    collections::{HashMap, HashSet},
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::Value;

use crate::{
    model::{
        Event, Id, Project, ShellItem, ThreadDetail, ThreadDetailSnapshot, ThreadItem, ThreadShell,
    },
    subagent::Subagent,
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

/// A rewind worked out, before anything is sent for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Rewind {
    /// How many turns the thread keeps.
    pub turn_count: u32,
    /// The checkpoint those turns end at, which is what is rolled back to.
    pub target: crate::model::RollbackTarget,
    /// What you sent in the turn that goes, to write again.
    pub text: String,
    /// Those messages, which are gone once the rewind has been through.
    pub message_ids: Vec<Id>,
    /// Finished turns after that one, which go with it.
    pub later_turns: usize,
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
pub enum AttachmentImage {
    Loading,
    Unavailable,
    Ready(String),
}

#[derive(Debug)]
pub struct ThreadState {
    /// Local payloads are an overlay; server events remain authoritative.
    pub recovered_tools: HashMap<String, crate::recovery::Recovery>,
    /// Downloaded message pictures. In-flight and failed downloads are retained too,
    /// so a stream delta does not ask for the same attachment again.
    pub attachment_images: HashMap<Id, AttachmentImage>,
    /// Exact turn timing seen on the shell stream, retained as the next turn starts.
    pub turn_timings: HashMap<String, crate::model::LatestTurn>,
    pub detail: ThreadDetail,
    pub last_sequence: u64,
    pub synchronized: bool,
    pub has_more: bool,
    pub before_cursor: Option<String>,
    /// Moved on by every change, so what is laid out from the state can be kept until it
    /// moves. No two states share one: see `REVISIONS`.
    pub revision: u64,
    /// What `detail` is derived from, and what the stream's events apply to.
    pub projection: Option<Box<crate::v2::Projection>>,
}

/// Every revision any thread state has had, counted once for all of them. A state built
/// afresh — a thread opened again, or a snapshot taken in place of what was there — then
/// never repeats a revision an earlier one had, so what was laid out from the old state is
/// never taken for the new one.
static REVISIONS: AtomicU64 = AtomicU64::new(1);

fn next_revision() -> u64 {
    REVISIONS.fetch_add(1, Ordering::Relaxed)
}

/// Context worth summarising. Below this there is not enough behind the thread for a
/// summary to buy anything back.
const RESUME_TOKENS: u64 = 100_000;

/// And a thread touched more recently than this is one somebody is still in the middle
/// of, rather than one they are coming back to.
const RESUME_IDLE_MINUTES: i64 = 70;

/// A message asking for the context to be compacted, which is `/compact` and nothing
/// else — the same reading the server gives it, since a message with anything more in it
/// is a message.
fn is_compact_command(message: &crate::model::Message) -> bool {
    message.role == "user" && message.text.trim().eq_ignore_ascii_case("/compact")
}

impl ThreadState {
    pub fn from_snapshot(snapshot: ThreadDetailSnapshot) -> Self {
        let (has_more, before_cursor) = match &snapshot.page {
            Some(page) => (page.has_more, page.before_cursor.clone()),
            None => (false, None),
        };
        Self {
            turn_timings: snapshot
                .thread
                .shell
                .latest_turn
                .iter()
                .map(|turn| (turn.turn_id.clone(), turn.clone()))
                .collect(),
            detail: snapshot.thread,
            projection: snapshot.projection,
            recovered_tools: HashMap::new(),
            attachment_images: HashMap::new(),
            last_sequence: snapshot.snapshot_sequence,
            synchronized: false,
            has_more,
            before_cursor,
            revision: next_revision(),
        }
    }

    /// Move the revision on, after a change to anything drawn from the state.
    pub fn touch(&mut self) {
        self.revision = next_revision();
    }

    pub fn id(&self) -> &str {
        &self.detail.shell.id
    }

    pub fn apply(&mut self, item: ThreadItem) {
        match item {
            ThreadItem::Synchronized => self.synchronized = true,
            ThreadItem::Snapshot { snapshot } => {
                let synchronized = self.synchronized;
                let timings = std::mem::take(&mut self.turn_timings);
                let same_thread = self.id() == snapshot.thread.shell.id;
                let images = std::mem::take(&mut self.attachment_images);
                let shell = self.detail.shell.clone();
                *self = Self::from_snapshot(snapshot);
                if same_thread {
                    // The list's entry carries what the projection does not; the turn
                    // is the projection's own, and newer.
                    let turn = self.detail.shell.latest_turn.take();
                    self.detail.shell = shell;
                    if turn.is_some() {
                        self.detail.shell.latest_turn = turn;
                    }
                    let ids: HashSet<_> = self
                        .detail
                        .messages
                        .iter()
                        .flat_map(|message| &message.attachments)
                        .map(|attachment| attachment.id.as_str())
                        .collect();
                    self.attachment_images = images
                        .into_iter()
                        .filter(|(id, _)| ids.contains(id.as_str()))
                        .collect();
                }
                for (id, timing) in timings {
                    if self.detail.checkpoints.iter().any(|c| c.turn_id == id) {
                        self.turn_timings.entry(id).or_insert(timing);
                    }
                }
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
        let Some(projection) = self.projection.as_mut() else {
            return;
        };
        if projection.apply(&event.kind, event.payload) {
            self.detail = projection.detail(&self.detail.shell);
            self.turn_timings
                .retain(|id, _| !projection.is_rolled_back(id));
            self.touch();
        }
    }

    /// Update list-level fields from the shell stream (title, model, latest turn).
    pub fn sync_shell(&mut self, shell: &ThreadShell) {
        // The open thread's own runs say more than the list does, and sooner.
        let latest_turn = match &self.projection {
            Some(projection) => projection.latest_turn(),
            None => shell.latest_turn.clone(),
        };
        if let Some(turn) = &latest_turn {
            self.turn_timings.insert(turn.turn_id.clone(), turn.clone());
        }
        let mine = &mut self.detail.shell;
        mine.title = shell.title.clone();
        mine.model_selection = shell.model_selection.clone();
        mine.runtime_mode = shell.runtime_mode.clone();
        mine.interaction_mode = shell.interaction_mode.clone();
        mine.latest_turn = latest_turn;
        mine.plan_progress = shell.plan_progress.clone();
        mine.has_pending_approvals = shell.has_pending_approvals;
        mine.has_pending_user_input = shell.has_pending_user_input;
        mine.has_actionable_proposed_plan = shell.has_actionable_proposed_plan;
        mine.background_liveness = shell.background_liveness.clone();
        mine.background_tasks = shell.background_tasks.clone();
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
        self.touch();
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

    /// What the server says is still alive in this thread's session after the turn
    /// settled: `working` for agent work, `monitoring` for a watch loop, nothing when
    /// there is none. It is the server's own registry of live tasks rather than a fold
    /// of the activities, so it knows about work that started before the history this
    /// client loaded — which, for a watcher left running for hours, is most of them.
    pub fn background_liveness(&self) -> Option<&str> {
        self.detail.shell.background_liveness.as_deref()
    }

    /// The subagents this thread has run, oldest first.
    ///
    /// They live in the same `task.*` activities as the background tasks and are told
    /// apart by the `agentKind` the server stamps on each one.
    pub fn subagents(&self) -> Vec<Subagent> {
        crate::subagent::fold(&self.detail.activities, self.session_live())
    }

    /// Whether the provider session that would own a running subagent is still there. A
    /// session that stopped, errored, or was interrupted took every process it started
    /// with it, however the rows that would have settled them ended up lost.
    fn session_live(&self) -> bool {
        self.detail.shell.session.as_ref().is_some_and(|session| {
            matches!(
                session.status.as_str(),
                "idle" | "starting" | "running" | "ready"
            )
        })
    }

    /// Background work the agent started that is still going: monitors and
    /// backgrounded commands, but not subagents, which have rows of their own. The
    /// server keeps the list, so it holds work started before the history this client
    /// loaded, and loses each task as it ends or its session goes.
    pub fn running_tasks(&self) -> Vec<RunningTask> {
        self.detail
            .shell
            .background_tasks
            .iter()
            .filter(|task| task.kind != "subagent")
            .map(|task| RunningTask {
                id: task.task_id.clone(),
                title: task
                    .description
                    .clone()
                    .unwrap_or_else(|| "background task".to_string()),
                task_type: task.kind.clone(),
                agent_kind: "background".to_string(),
                started_at: String::new(),
                turn_id: None,
                backgrounded: task.kind == "command",
            })
            .collect()
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

    /// The plan waiting to be built: the latest one, while nothing has built it yet.
    pub fn actionable_plan(&self) -> Option<&crate::model::ProposedPlan> {
        self.detail
            .proposed_plans
            .iter()
            .max_by(|a, b| a.created_at.cmp(&b.created_at))
            .filter(|plan| plan.implemented_at.is_none())
    }

    /// What rewinding to before one of your messages would do. The turn the message
    /// belongs to goes, and every turn after it; what comes back is everything you sent
    /// that turn, the prompt and the messages that steered it, since a steer is part of
    /// the turn it joined and cannot be kept without it.
    ///
    /// A user message carries no turn of its own, so it is placed by time: it belongs
    /// to the first turn that finished after it was sent. That holds for a steer, which
    /// arrives mid-turn, and for a turn that ended before it said anything.
    pub fn rewind_before(&self, message_id: &str) -> Result<Rewind, &'static str> {
        let message = self
            .detail
            .messages
            .iter()
            .find(|m| m.id == message_id && m.role == "user")
            .ok_or("that is not a message of yours")?;
        let turns = self.counted_turns();
        let at = turns
            .iter()
            .position(|(_, completed, _)| *completed >= message.created_at.as_str())
            .ok_or("that message's turn has not finished")?;
        self.rewind_to(&turns, at)
    }

    /// The same, for the last turn that finished.
    pub fn rewind_last(&self) -> Result<Rewind, &'static str> {
        let turns = self.counted_turns();
        if turns.is_empty() {
            return Err("no finished turn to rewind");
        }
        self.rewind_to(&turns, turns.len() - 1)
    }

    /// Finished turns in order, by their count and when they finished.
    fn counted_turns(&self) -> Vec<(u32, &str, Option<&crate::model::RollbackTarget>)> {
        let mut turns: Vec<_> = self
            .detail
            .checkpoints
            .iter()
            .filter_map(|c| {
                Some((
                    c.checkpoint_turn_count?,
                    c.completed_at.as_str(),
                    c.rollback.as_ref(),
                ))
            })
            .collect();
        turns.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        turns
    }

    fn rewind_to(
        &self,
        turns: &[(u32, &str, Option<&crate::model::RollbackTarget>)],
        at: usize,
    ) -> Result<Rewind, &'static str> {
        let (count, completed, target) = turns[at];
        let target = target.ok_or("that turn left nothing to roll back to")?;
        let after = at.checked_sub(1).map(|before| turns[before].1);
        let (message_ids, texts): (Vec<Id>, Vec<&str>) = self
            .detail
            .messages
            .iter()
            .filter(|m| {
                m.role == "user"
                    && m.created_at.as_str() <= completed
                    && after.is_none_or(|after| m.created_at.as_str() > after)
            })
            .map(|m| (m.id.clone(), m.text.as_str()))
            .unzip();
        if message_ids.is_empty() {
            return Err("the messages of that turn are not loaded");
        }
        Ok(Rewind {
            turn_count: count.saturating_sub(1),
            target: target.clone(),
            text: texts.join("\n\n"),
            message_ids,
            later_turns: turns.len() - at - 1,
        })
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
    /// Whether the running turn is the context being compacted rather than an answer
    /// being written.
    ///
    /// Nothing on the wire says so. `/compact` is sent as an ordinary message and the
    /// turn it starts looks like any other, so what marks one is the message itself: the
    /// last `/compact` anybody sent, once it is the message the running turn was asked
    /// for. What ends it is the `context-compaction` activity that names the request, or
    /// the turn ending, which takes the whole line with it.
    pub fn is_compacting(&self) -> bool {
        let Some(asked) = self
            .detail
            .messages
            .iter()
            .rev()
            .find(|m| is_compact_command(m))
        else {
            return false;
        };
        let started = match self.detail.shell.latest_turn.as_ref() {
            // Sent after the last turn was asked for, or the turn it asked for itself.
            Some(turn) => {
                asked.created_at > turn.requested_at
                    || (turn.state == "running" && asked.created_at == turn.requested_at)
            }
            None => true,
        };
        started
            && !self.detail.activities.iter().any(|activity| {
                activity.kind == "context-compaction"
                    && activity.str("requestId") == Some(&asked.id)
            })
    }

    /// What the thread has spent its context window on, as the provider last counted
    /// it, and when the count was made.
    fn context_window(&self) -> Option<(u64, &str)> {
        self.detail.activities.iter().rev().find_map(|activity| {
            if activity.kind != "context-window.updated" {
                return None;
            }
            let used = activity.payload.get("usedTokens")?.as_u64()?;
            Some((used, activity.created_at.as_str()))
        })
    }

    /// How much context the thread is carrying, when picking it up again would be
    /// better done with less of it: a long thread, left alone long enough that whoever
    /// comes back to it is starting something rather than continuing it.
    ///
    /// `None` for a thread that is still being answered or is waiting on an answer,
    /// where compacting is not what anybody wants next.
    pub fn resume_with_less(&self, now: &str) -> Option<u64> {
        if self.is_running() {
            return None;
        }
        let (used, counted) = self.context_window()?;
        if used < RESUME_TOKENS {
            return None;
        }
        let parse = |text: &str| {
            time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()
        };
        let (counted, now) = (parse(counted)?, parse(now)?);
        if (now - counted).whole_minutes() < RESUME_IDLE_MINUTES {
            return None;
        }
        // Read last: it is the dearest of these to work out, and the cheap tests have
        // already ruled out every thread but the few this is for.
        self.pending_user_input().is_none().then_some(used)
    }

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

    #[test]
    fn snapshots_keep_downloads_for_attachments_still_in_the_thread() {
        let snapshot = || {
            serde_json::from_value::<ThreadDetailSnapshot>(json!({
            "snapshotSequence": 1,
            "thread": {
                "id": "t1", "projectId": "p1", "title": "Test",
                "modelSelection": {"instanceId": "codex", "model": "m"},
                "messages": [{"id": "m1", "role": "user", "text": "", "attachments": [{
                    "type": "image", "id": "image-1", "name": "Screenshot.png", "mimeType": "image/png"
                }]}], "activities": []
            }
        })).unwrap()
        };
        let mut state = ThreadState::from_snapshot(snapshot());
        state
            .attachment_images
            .insert("image-1".into(), AttachmentImage::Ready("bytes".into()));
        state
            .attachment_images
            .insert("removed".into(), AttachmentImage::Loading);
        state.apply(ThreadItem::Snapshot {
            snapshot: snapshot(),
        });
        assert!(
            matches!(state.attachment_images.get("image-1"), Some(AttachmentImage::Ready(data)) if data == "bytes")
        );
        assert!(!state.attachment_images.contains_key("removed"));
        let mut changed = snapshot();
        changed.thread.messages.clear();
        state.apply(ThreadItem::Snapshot { snapshot: changed });
        assert!(state.attachment_images.is_empty());
    }

    /// What is laid out from a thread is kept for as long as its revision stands, so a
    /// snapshot taken in place of the state, or the thread opened again, never comes back
    /// with a revision the state had before.
    #[test]
    fn a_state_built_afresh_never_repeats_a_revision() {
        let mut open = thread();
        let mut seen = vec![open.revision];
        open.sync_shell(&open.detail.shell.clone());
        seen.push(open.revision);
        let again = thread();
        open.apply(ThreadItem::Snapshot {
            snapshot: serde_json::from_value(json!({
                "snapshotSequence": 11,
                "thread": {
                    "id": "t1", "projectId": "p1", "title": "Test",
                    "modelSelection": {"instanceId": "claudeAgent", "model": "m"},
                    "messages": [], "activities": []
                }
            }))
            .unwrap(),
        });
        assert!(!seen.contains(&open.revision), "{seen:?} {}", open.revision);
        assert!(!seen.contains(&again.revision));
        assert_ne!(open.revision, again.revision);
    }

    /// A thread whose last message asked for the context to be compacted, in the shape
    /// the server sends one.
    fn compacting(text: &str, requested_at: &str, state: &str, settled: bool) -> ThreadState {
        let snapshot: ThreadDetailSnapshot = serde_json::from_value(json!({
            "snapshotSequence": 10,
            "thread": {
                "id": "t1", "projectId": "p1", "title": "Test",
                "modelSelection": {"instanceId": "claudeAgent", "model": "m"},
                "runtimeMode": "full-access", "session": {"status": "running"},
                "latestTurn": {
                    "turnId": "turn", "state": state, "requestedAt": requested_at
                },
                "messages": [{
                    "id": "m1", "role": "user", "text": text,
                    "createdAt": "2026-01-01T00:00:01Z"
                }],
                "activities": if settled {
                    json!([{
                        "id": "a1", "kind": "context-compaction", "tone": "info",
                        "summary": "", "payload": {"requestId": "m1"},
                        "createdAt": "2026-01-01T00:00:02Z"
                    }])
                } else {
                    json!([])
                }
            }
        }))
        .unwrap();
        ThreadState::from_snapshot(snapshot)
    }

    /// `/compact` is sent as a message and the turn it starts looks like any other, so a
    /// thread that is compacting has to be read from the message and the activity that
    /// answers it — otherwise it is drawn as though an answer were being written.
    #[test]
    fn a_thread_compacting_its_context_is_not_a_thread_answering() {
        // Asked for after the last turn started: the turn to come is the compaction.
        assert!(compacting("/compact", "2026-01-01T00:00:00Z", "completed", false).is_compacting());
        // Or the turn it asked for itself, while it runs.
        assert!(compacting("/compact", "2026-01-01T00:00:01Z", "running", false).is_compacting());
        // Once the compaction has been recorded the thread is answering again.
        assert!(!compacting("/compact", "2026-01-01T00:00:00Z", "completed", true).is_compacting());
        // A turn asked for after the message is somebody carrying on.
        assert!(!compacting("/compact", "2026-01-01T00:00:09Z", "running", false).is_compacting());
        // And a message with anything else in it is a message.
        assert!(
            !compacting("/compact please", "2026-01-01T00:00:00Z", "running", false)
                .is_compacting()
        );
    }

    /// A thread that has counted its context window, in the shape the server sends one.
    fn carrying(used: u64, counted: &str, running: bool) -> ThreadState {
        let snapshot: ThreadDetailSnapshot = serde_json::from_value(json!({
            "snapshotSequence": 10,
            "thread": {
                "id": "t1", "projectId": "p1", "title": "Test",
                "modelSelection": {"instanceId": "claudeAgent", "model": "m"},
                "runtimeMode": "full-access",
                "session": {"status": if running { "running" } else { "idle" }},
                "latestTurn": null, "messages": [],
                "activities": [{
                    "id": "a1", "kind": "context-window.updated", "tone": "info",
                    "summary": "", "payload": {"usedTokens": used, "maxTokens": 200_000},
                    "createdAt": counted
                }]
            }
        }))
        .unwrap();
        ThreadState::from_snapshot(snapshot)
    }

    /// Coming back to a long thread after a while, starting again with less behind it is
    /// worth being told about — but only then, and only when there is nothing else the
    /// thread is in the middle of.
    #[test]
    fn a_thread_come_back_to_after_a_long_gap_is_carrying_its_context() {
        let now = "2026-01-01T12:00:00Z";
        assert_eq!(
            carrying(120_000, "2026-01-01T10:00:00Z", false).resume_with_less(now),
            Some(120_000)
        );
        // A gap short enough that somebody is still in the middle of the thread.
        assert_eq!(
            carrying(120_000, "2026-01-01T11:30:00Z", false).resume_with_less(now),
            None
        );
        // Not enough behind it for a summary to buy anything.
        assert_eq!(
            carrying(40_000, "2026-01-01T10:00:00Z", false).resume_with_less(now),
            None
        );
        // And a thread that is answering is not one to compact.
        assert_eq!(
            carrying(120_000, "2026-01-01T10:00:00Z", true).resume_with_less(now),
            None
        );
        // A thread that has never counted its context has nothing to say.
        assert_eq!(thread().resume_with_less(now), None);
    }

    /// A thread opened from the server's snapshot, at sequence 10, with a run going.
    fn opened() -> ThreadState {
        let item = crate::v2::thread_item(
            json!({
                "kind": "snapshot", "snapshotSequence": 10,
                "projection": {
                    "thread": {
                        "id": "t1", "projectId": "p1", "title": "Test",
                        "modelSelection": {"instanceId": "claudeAgent", "model": "m"},
                    },
                    "runs": [{"id": "run", "ordinal": 1, "status": "running",
                        "requestedAt": "2026-01-01T00:00:00Z"}],
                    "visibleTurnItems": [],
                },
            }),
            None,
        )
        .unwrap();
        let ThreadItem::Snapshot { snapshot } = item else {
            panic!("not a snapshot");
        };
        ThreadState::from_snapshot(snapshot)
    }

    fn item_event(sequence: u64, item: serde_json::Value) -> Event {
        let mut item = item;
        for (key, value) in [
            ("threadId", json!("t1")),
            ("runId", json!("run")),
            ("ordinal", json!(sequence)),
            ("startedAt", json!("2026-01-01T00:00:01Z")),
            ("updatedAt", json!("2026-01-01T00:00:01Z")),
        ] {
            item.as_object_mut().unwrap().entry(key).or_insert(value);
        }
        Event {
            sequence,
            event_id: format!("e{sequence}"),
            kind: "turn-item.updated".into(),
            occurred_at: String::new(),
            payload: item,
        }
    }

    fn message_event(sequence: u64, text: &str, streaming: bool) -> Event {
        item_event(
            sequence,
            json!({"id": "i1", "type": "assistant_message", "messageId": "m1",
                "status": if streaming { "running" } else { "completed" },
                "text": text, "streaming": streaming}),
        )
    }

    #[test]
    fn a_message_comes_with_its_attachments() {
        let mut state = opened();
        state.apply_event(item_event(
            11,
            json!({"id": "i1", "type": "user_message", "messageId": "m1", "status": "completed",
                "inputIntent": "turn_start", "text": "See this screenshot.",
                "attachments": [{"type": "image", "id": "image-1", "name": "Screenshot.png",
                    "mimeType": "image/png", "sizeBytes": 37407}]}),
        ));
        assert_eq!(state.detail.messages[0].attachments[0].id, "image-1");
        assert!(state.detail.messages[0].attachments[0].is_image());
    }

    /// Background work is the server's list of it, which it keeps on the thread's entry.
    #[test]
    fn running_tasks_are_the_background_work_the_server_lists() {
        let mut state = opened();
        let mut shell = state.detail.shell.clone();
        shell.background_tasks = serde_json::from_value(json!([
            {"taskId": "t-1", "description": "watch build", "kind": "monitor"},
            {"taskId": "t-2", "description": "run tests", "kind": "command"},
            {"taskId": "t-3", "description": "review", "kind": "subagent"},
        ]))
        .unwrap();
        state.sync_shell(&shell);
        let running = state.running_tasks();
        let titles: Vec<_> = running.iter().map(|task| task.title.as_str()).collect();
        assert_eq!(
            titles,
            ["watch build", "run tests"],
            "subagents have rows of their own"
        );
        assert!(!running[0].backgrounded);
        assert!(running[1].backgrounded);

        shell.background_tasks.clear();
        state.sync_shell(&shell);
        assert!(state.running_tasks().is_empty());
    }

    #[test]
    fn a_streamed_message_is_replaced_whole() {
        let mut state = opened();
        state.apply_event(message_event(11, "Hel", true));
        assert_eq!(state.detail.messages[0].text, "Hel");
        assert!(state.detail.messages[0].streaming);
        state.apply_event(message_event(12, "Hello!", false));
        assert_eq!(state.detail.messages.len(), 1);
        assert_eq!(state.detail.messages[0].text, "Hello!");
        assert!(!state.detail.messages[0].streaming);
    }

    #[test]
    fn stale_sequences_are_ignored() {
        let mut state = opened();
        state.apply_event(message_event(5, "old", true));
        assert!(state.detail.messages.is_empty());
    }

    #[test]
    fn approvals_resolve() {
        let mut state = opened();
        state.apply_event(item_event(
            11,
            json!({"id": "a1", "type": "approval_request", "status": "waiting", "requestId": "r1",
                "requestKind": "command", "prompt": "rm -rf",
                "options": [{"decision": "accept", "label": "Allow"}]}),
        ));
        let pending = state.pending_approvals();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].detail.as_deref(), Some("rm -rf"));
        assert_eq!(pending[0].options[0].label, "Allow");
        state.apply_event(Event {
            sequence: 12,
            event_id: "e12".into(),
            kind: "runtime-request.updated".into(),
            occurred_at: String::new(),
            payload: json!({"id": "r1", "status": "resolved", "decision": "accept"}),
        });
        assert!(state.pending_approvals().is_empty());
    }
}

//! The subagents a thread ran, folded out of its `task.*` activities.
//!
//! The server stamps every task with an `agentKind` as it ingests it: `agent` for a
//! subagent the model delegated work to, `background` for a monitor or a backgrounded
//! command. Only the former belongs here; the latter is ordinary background work and
//! stays in `:tasks`.
//!
//! The fold is tolerant because the rows are not: a task's start can age out of
//! retention, a completion can arrive before the update that settles it, and the same
//! task id can be reactivated for a second run. So a completion may create a subagent,
//! a late start only fills metadata in, and terminal timestamps are written once.

use serde_json::Value;

use crate::model::Activity;

/// The server's own bound on task summaries; applied again here because progress and
/// error strings arrive unbounded.
const SUMMARY_LIMIT: usize = 180;
/// How much of the recent-activity trail to keep for the selected subagent.
const RECENT_LIMIT: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pending,
    Running,
    Waiting,
    Idle,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl Status {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "waiting" => Self::Waiting,
            "idle" => Self::Idle,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "interrupted" => Self::Interrupted,
            _ => return None,
        })
    }

    /// What `task.completed` reports, which is a smaller vocabulary than a status patch.
    fn from_completion(value: &str) -> Self {
        match value {
            "failed" => Self::Failed,
            "stopped" => Self::Interrupted,
            _ => Self::Completed,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }

    /// Whether it may still want attention. Idle is settled but resumable, so it is not
    /// active; waiting is, because it is waiting on the user.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Pending | Self::Running | Self::Waiting)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pending | Self::Running | Self::Waiting => "working",
            Self::Idle => "idle · resumable",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled | Self::Interrupted => "stopped",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub total_tokens: u64,
    pub tool_uses: Option<u64>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subagent {
    /// The provider's task id, which is also the roster's identity across runs.
    pub id: String,
    pub title: String,
    /// The agent definition it runs as, such as `Explore`.
    pub role: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub status: Status,
    /// How many times this identity has run; above one, it was reactivated.
    pub activations: u32,
    pub usage: Option<Usage>,
    /// What it is doing now, as the provider last described it.
    pub progress: Option<String>,
    pub last_tool: Option<String>,
    /// The report it returned, as the server's 180-character summary.
    pub result: Option<String>,
    pub error: Option<String>,
    /// Where the full transcript lives on the machine that ran it.
    pub output_file: Option<String>,
    pub recent: Vec<String>,
    pub first_seen_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub updated_at: String,
}

impl Subagent {
    /// The one line worth reading: what it is doing while it runs, how it came out once
    /// it has settled. A report arrives as prose and is cut to its opening line, which
    /// is where an agent puts its answer.
    pub fn activity(&self) -> Option<String> {
        let tool = self.last_tool.as_ref().map(|tool| format!("▸ {tool}"));
        let ordered = if self.status.is_active() {
            [&self.progress, &tool, &self.result, &self.error]
        } else {
            [&self.error, &self.result, &self.progress, &tool]
        };
        ordered
            .into_iter()
            .flatten()
            .find_map(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
            .map(str::to_string)
    }

    /// `opus-5 · high`, with the vendor prefix and date suffix off the model id.
    pub fn model_label(&self) -> Option<String> {
        let model = self.model.as_deref()?;
        let compact = model
            .strip_prefix("claude-")
            .unwrap_or(model)
            .trim_end_matches("-latest");
        let compact = match compact.rsplit_once('-') {
            Some((head, tail)) if tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()) => {
                head
            }
            _ => compact,
        };
        Some(match &self.effort {
            Some(effort) => format!("{compact} · {effort}"),
            None => compact.to_string(),
        })
    }
}

/// Every subagent the thread has run, in the order they were first seen.
///
/// `session_live` false means the provider session is gone, and with it every process
/// that could have finished a run: agents still reading as working were orphaned by the
/// session's death rather than left running.
pub fn fold(activities: &[Activity], session_live: bool) -> Vec<Subagent> {
    let mut agents: Vec<Subagent> = Vec::new();

    for activity in activities {
        let Some(kind) = activity.kind.strip_prefix("task.") else {
            continue;
        };
        let Some(task_id) = activity.str("taskId") else {
            continue;
        };
        let at = activity.created_at.as_str();
        // Membership is decided once, on the first row for a task id. Terminal rows
        // often carry nothing but the id and a status, so re-judging them would drop
        // the tasks they settle.
        let known = agents.iter().any(|agent| agent.id == task_id);
        if !known {
            if activity.str("agentKind") != Some("agent") {
                continue;
            }
            agents.push(new_agent(task_id, activity, at));
        }
        let index = agents.iter().position(|a| a.id == task_id).unwrap_or(0);
        let agent = &mut agents[index];
        fill_metadata(agent, activity);

        match kind {
            "started" => {
                // A start that arrives after the run settled is a late delivery: it
                // fills metadata in and nothing more. Only an explicit status reopens a
                // run, so a task first seen through its own completion stays settled.
                if agent.activations == 0 && !agent.status.is_terminal() {
                    agent.activations = 1;
                    agent.status = Status::Running;
                    if agent.started_at.is_none() {
                        agent.started_at = Some(at.to_string());
                    }
                } else if agent.status == Status::Idle {
                    apply_status(agent, Status::Running, at);
                }
            }
            "progress" => {
                if agent.activations == 0 {
                    agent.activations = 1;
                }
                let explicit = activity.str("status").and_then(Status::parse);
                match explicit {
                    Some(status) => apply_status(agent, status, at),
                    // A usage-only tick is a snapshot, not a sign of life: it must not
                    // drag a settled or idle run back to working.
                    None => {
                        let snapshot = activity
                            .payload
                            .get("usageSnapshot")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        if !snapshot && !agent.status.is_terminal() && agent.status != Status::Idle
                        {
                            apply_status(agent, Status::Running, at);
                        }
                    }
                }
                // `summary` is the provider's own progress note; `detail` is the step
                // it is on, and is the only one Claude sends.
                let note = activity
                    .str("summary")
                    .or_else(|| activity.str("detail"))
                    .filter(|note| *note != agent.title);
                if let Some(note) = note {
                    agent.progress = Some(bounded(note));
                    push_recent(agent, bounded(note));
                }
                if let Some(tool) = activity.str("lastToolName") {
                    agent.last_tool = Some(tool.to_string());
                    if note.is_none() {
                        push_recent(agent, format!("▸ {tool}"));
                    }
                }
                if let Some(error) = activity.str("error") {
                    agent.error = Some(bounded(error));
                }
                merge_usage(agent, activity);
            }
            "updated" => {
                if agent.activations == 0 {
                    agent.activations = 1;
                }
                if let Some(detail) = activity.str("detail").filter(|d| *d != agent.title) {
                    agent.progress = Some(bounded(detail));
                }
                let was_terminal = agent.status.is_terminal();
                if let Some(status) = activity.str("status").and_then(Status::parse) {
                    apply_status(agent, status, at);
                }
                if let Some(error) = activity.str("error") {
                    agent.error = Some(bounded(error));
                }
                // The provider's own end time beats the moment the row was ingested,
                // for the transition that actually settled the run.
                if let Some(ended) = activity.str("endedAt")
                    && !was_terminal
                    && agent.status.is_terminal()
                {
                    agent.completed_at = Some(ended.to_string());
                }
            }
            "completed" => {
                if agent.activations == 0 {
                    agent.activations = 1;
                }
                let summary = activity
                    .str("summary")
                    .or_else(|| activity.str("detail"))
                    .map(bounded);
                // Claude often settles a run with `task.updated` and only then sends the
                // completion carrying the report. Status and timestamps are already
                // frozen, but the result and the final usage still belong to it.
                if agent.status.is_terminal() {
                    if let Some(summary) = summary {
                        let slot = if agent.status == Status::Failed {
                            &mut agent.error
                        } else {
                            &mut agent.result
                        };
                        slot.get_or_insert(summary);
                    }
                } else {
                    let status = Status::from_completion(activity.str("status").unwrap_or(""));
                    apply_status(agent, status, at);
                    if let Some(summary) = summary {
                        if status == Status::Failed {
                            agent.error.get_or_insert(summary);
                        } else {
                            agent.result = Some(summary);
                        }
                    }
                }
                merge_usage(agent, activity);
            }
            _ => continue,
        }
        agent.updated_at = at.to_string();
    }

    if !session_live {
        for agent in agents.iter_mut().filter(|a| a.status.is_active()) {
            agent.status = Status::Interrupted;
            if agent.completed_at.is_none() {
                agent.completed_at = Some(agent.updated_at.clone());
            }
        }
    }
    agents
}

fn new_agent(id: &str, activity: &Activity, at: &str) -> Subagent {
    Subagent {
        id: id.to_string(),
        title: activity
            .str("title")
            .or_else(|| activity.str("detail"))
            .map(first_line)
            .filter(|title| !title.is_empty())
            .unwrap_or(id)
            .to_string(),
        role: activity.str("role").map(str::to_string),
        model: activity.str("model").map(str::to_string),
        effort: activity.str("effort").map(str::to_string),
        status: Status::Pending,
        activations: 0,
        usage: None,
        progress: None,
        last_tool: None,
        result: None,
        error: None,
        output_file: None,
        recent: Vec::new(),
        first_seen_at: at.to_string(),
        started_at: None,
        completed_at: None,
        updated_at: at.to_string(),
    }
}

/// Fills in what a row knows. Never blanks what is already known: the thin rows carry
/// less than the thick ones, and arrive in any order.
fn fill_metadata(agent: &mut Subagent, activity: &Activity) {
    if let Some(title) = activity
        .str("title")
        .map(first_line)
        .filter(|t| !t.is_empty())
    {
        agent.title = title.to_string();
    }
    if let Some(role) = activity.str("role") {
        agent.role = Some(role.to_string());
    }
    if let Some(model) = activity.str("model") {
        agent.model = Some(model.to_string());
    }
    if let Some(effort) = activity.str("effort") {
        agent.effort = Some(effort.to_string());
    }
    if let Some(file) = activity.str("outputFile") {
        agent.output_file = Some(file.to_string());
    }
}

fn apply_status(agent: &mut Subagent, status: Status, at: &str) {
    let was_terminal = agent.status.is_terminal();
    // Duplicate terminal rows are idempotent: the first one wins and the timestamps
    // stay where it put them.
    if was_terminal && status.is_terminal() {
        return;
    }
    if (was_terminal || agent.status == Status::Idle)
        && matches!(status, Status::Running | Status::Pending)
    {
        // The same identity, running again: the previous run's outcome would otherwise
        // sit under a live row.
        agent.activations += 1;
        agent.result = None;
        agent.error = None;
        agent.completed_at = None;
        if status == Status::Running {
            agent.started_at = Some(at.to_string());
        }
    }
    if status == Status::Running && agent.started_at.is_none() {
        agent.started_at = Some(at.to_string());
    }
    if status.is_terminal() && agent.completed_at.is_none() {
        agent.completed_at = Some(at.to_string());
    }
    agent.status = status;
}

/// Claude reports usage cumulatively per task, so a field-wise maximum is idempotent
/// under repeated and late frames, and a terminal row carrying only a total cannot wipe
/// a breakdown that arrived earlier.
fn merge_usage(agent: &mut Subagent, activity: &Activity) {
    let Some(typed) = activity.payload.get("typedUsage") else {
        return;
    };
    let number = |key: &str| typed.get(key).and_then(Value::as_u64);
    let Some(total_tokens) = number("totalTokens") else {
        return;
    };
    let incoming = Usage {
        total_tokens,
        tool_uses: number("toolUses"),
        duration_ms: number("durationMs"),
    };
    match &mut agent.usage {
        None => agent.usage = Some(incoming),
        Some(current) => {
            let pick = |a: Option<u64>, b: Option<u64>| match (a, b) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (value, None) | (None, value) => value,
            };
            current.total_tokens = current.total_tokens.max(incoming.total_tokens);
            current.tool_uses = pick(current.tool_uses, incoming.tool_uses);
            current.duration_ms = pick(current.duration_ms, incoming.duration_ms);
        }
    }
}

fn push_recent(agent: &mut Subagent, note: String) {
    if agent.recent.last() == Some(&note) {
        return;
    }
    agent.recent.push(note);
    if agent.recent.len() > RECENT_LIMIT {
        agent.recent.remove(0);
    }
}

/// Titles and steps are drawn on one line, and the provider does not promise one.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

fn bounded(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= SUMMARY_LIMIT {
        return text.to_string();
    }
    let mut out: String = text.chars().take(SUMMARY_LIMIT - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn activity(kind: &str, at: &str, payload: serde_json::Value) -> Activity {
        serde_json::from_value(json!({
            "id": format!("{kind}-{at}"),
            "kind": kind,
            "tone": "info",
            "summary": "",
            "payload": payload,
            "createdAt": at,
        }))
        .unwrap()
    }

    fn agent_payload(id: &str) -> serde_json::Value {
        json!({
            "taskId": id,
            "agentKind": "agent",
            "taskType": "local_agent",
            "title": "Review the branch diff",
            "role": "general-purpose",
            "model": "claude-opus-5",
            "effort": "high",
        })
    }

    #[test]
    fn background_tasks_are_not_subagents() {
        let rows = vec![activity(
            "task.started",
            "1",
            json!({"taskId": "b1", "agentKind": "background", "taskType": "local_bash", "title": "Run the checks"}),
        )];
        assert!(fold(&rows, true).is_empty());
    }

    #[test]
    fn folds_a_run_from_start_to_report() {
        let rows = vec![
            activity("task.started", "1", agent_payload("a1")),
            activity(
                "task.progress",
                "2",
                json!({"taskId": "a1", "detail": "Running Read context-header", "lastToolName": "Bash",
                       "typedUsage": {"totalTokens": 1000, "toolUses": 4}}),
            ),
            activity(
                "task.updated",
                "3",
                json!({"taskId": "a1", "status": "completed", "endedAt": "3.5"}),
            ),
            activity(
                "task.completed",
                "4",
                json!({"taskId": "a1", "status": "completed", "summary": "Found three issues",
                       "outputFile": "/tmp/tasks/a1.output",
                       "typedUsage": {"totalTokens": 2000, "toolUses": 9}}),
            ),
        ];
        let agents = fold(&rows, true);
        assert_eq!(agents.len(), 1);
        let agent = &agents[0];
        assert_eq!(agent.status, Status::Completed);
        assert_eq!(agent.role.as_deref(), Some("general-purpose"));
        // The provider's own end time wins over the moment the row was ingested.
        assert_eq!(agent.completed_at.as_deref(), Some("3.5"));
        // The completion enriches a run its own update had already settled.
        assert_eq!(agent.result.as_deref(), Some("Found three issues"));
        assert_eq!(agent.output_file.as_deref(), Some("/tmp/tasks/a1.output"));
        assert_eq!(agent.usage.as_ref().unwrap().total_tokens, 2000);
        assert_eq!(agent.usage.as_ref().unwrap().tool_uses, Some(9));
        assert_eq!(agent.model_label().as_deref(), Some("opus-5 · high"));
    }

    #[test]
    fn a_completion_alone_makes_a_subagent() {
        let rows = vec![activity(
            "task.completed",
            "9",
            json!({"taskId": "a1", "agentKind": "agent", "status": "failed", "title": "Map the protocol",
                   "summary": "ran out of context"}),
        )];
        let agents = fold(&rows, true);
        assert_eq!(agents[0].status, Status::Failed);
        assert_eq!(agents[0].error.as_deref(), Some("ran out of context"));
        assert_eq!(agents[0].activations, 1);
    }

    #[test]
    fn a_late_start_does_not_reopen_a_settled_run() {
        let rows = vec![
            activity(
                "task.completed",
                "9",
                json!({"taskId": "a1", "agentKind": "agent", "status": "completed", "summary": "done"}),
            ),
            activity("task.started", "1", agent_payload("a1")),
        ];
        let agents = fold(&rows, true);
        assert_eq!(agents[0].status, Status::Completed);
        // Metadata the late row carried is still taken.
        assert_eq!(agents[0].role.as_deref(), Some("general-purpose"));
    }

    #[test]
    fn running_again_clears_the_last_run() {
        let rows = vec![
            activity("task.started", "1", agent_payload("a1")),
            activity(
                "task.completed",
                "2",
                json!({"taskId": "a1", "status": "completed", "summary": "first report"}),
            ),
            activity(
                "task.progress",
                "3",
                json!({"taskId": "a1", "status": "running"}),
            ),
        ];
        let agents = fold(&rows, true);
        assert_eq!(agents[0].status, Status::Running);
        assert_eq!(agents[0].activations, 2);
        assert_eq!(agents[0].result, None);
    }

    #[test]
    fn a_usage_snapshot_does_not_revive_a_settled_run() {
        let rows = vec![
            activity("task.started", "1", agent_payload("a1")),
            activity(
                "task.completed",
                "2",
                json!({"taskId": "a1", "status": "completed", "summary": "done"}),
            ),
            activity(
                "task.progress",
                "3",
                json!({"taskId": "a1", "usageSnapshot": true, "typedUsage": {"totalTokens": 5}}),
            ),
        ];
        assert_eq!(fold(&rows, true)[0].status, Status::Completed);
    }

    #[test]
    fn a_dead_session_leaves_nothing_working() {
        let rows = vec![activity("task.started", "1", agent_payload("a1"))];
        assert_eq!(fold(&rows, true)[0].status, Status::Running);
        let orphaned = fold(&rows, false);
        assert_eq!(orphaned[0].status, Status::Interrupted);
        assert_eq!(orphaned[0].completed_at.as_deref(), Some("1"));
    }

    #[test]
    fn the_activity_line_leads_with_progress_then_outcome() {
        let mut agent = fold(&[activity("task.started", "1", agent_payload("a1"))], true)
            .pop()
            .unwrap();
        agent.last_tool = Some("Bash".into());
        assert_eq!(agent.activity().as_deref(), Some("▸ Bash"));
        agent.progress = Some("Reading the diff".into());
        assert_eq!(agent.activity().as_deref(), Some("Reading the diff"));
        agent.status = Status::Failed;
        agent.error = Some("timed out".into());
        assert_eq!(agent.activity().as_deref(), Some("timed out"));
        // A report is prose; the row is one line.
        agent.status = Status::Completed;
        agent.error = None;
        agent.result = Some("Done.\n\n## Findings\n\nThree.".into());
        assert_eq!(agent.activity().as_deref(), Some("Done."));
    }
}

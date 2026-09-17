//! Derives renderable blocks from a thread: user and assistant messages,
//! collapsed tool groups, plan cards, and error lines, in chronological order.

use std::collections::HashSet;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use serde_json::Value;

use crate::{model::Activity, state::ThreadState};

pub const USER_MARK: &str = "▌";
const STREAM_CURSOR: &str = "▍";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BlockKey {
    Message(String),
    Work(String),
    Plan(String),
    Working,
}

pub struct Block {
    pub key: BlockKey,
    pub text: Text<'static>,
    /// Toggleable regions, in text-line indices before wrapping. Work groups list their
    /// header and every tool row, foldable or not.
    pub rows: Vec<Region>,
    /// Plain-text renderings keyed for export to an editor: the block itself under its
    /// primary key (`msg:<id>`, the work group key, `plan:<id>`) and, for work groups, each
    /// tool row under its expand key.
    pub exports: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
struct WorkEntry {
    key: String,
    turn_id: Option<String>,
    icon: &'static str,
    title: String,
    /// One-line summary shown on the collapsed row.
    detail: Option<String>,
    /// Everything the server sent about the call, for the expanded row. The server
    /// projects tool payloads down to a summary before they reach the wire: the input is
    /// cut at 180 characters and the output is its first meaningful line.
    input: Option<String>,
    output: Option<String>,
    files: Vec<String>,
    status: String,
    tone: String,
}

impl WorkEntry {
    fn has_more(&self) -> bool {
        self.input
            .as_deref()
            .is_some_and(|i| Some(i) != self.detail.as_deref())
            || self.output.is_some()
            || !self.files.is_empty()
    }
}

/// Expand key of one tool row inside a work group.
fn row_key(group: &str, entry: &WorkEntry) -> String {
    format!("{group}/{}", entry.key)
}

enum Item<'a> {
    Message(&'a crate::model::Message),
    Activity(&'a Activity),
    Plan(&'a crate::model::ProposedPlan),
}

fn item_time<'a>(item: &'a Item<'a>) -> &'a str {
    match item {
        Item::Message(m) => &m.created_at,
        Item::Activity(a) => &a.created_at,
        Item::Plan(p) => &p.created_at,
    }
}

/// Build the ordered blocks for a thread. `expanded` holds work-group keys shown in full.
pub fn build(
    thread: &ThreadState,
    expanded: &HashSet<String>,
    expand_all: bool,
    width: u16,
) -> Vec<Block> {
    let detail = &thread.detail;
    let mut items: Vec<Item<'_>> =
        Vec::with_capacity(detail.messages.len() + detail.activities.len());
    items.extend(detail.messages.iter().map(Item::Message));
    items.extend(
        detail
            .activities
            .iter()
            .filter(|a| is_visible_activity(a))
            .map(Item::Activity),
    );
    items.extend(detail.proposed_plans.iter().map(Item::Plan));
    items.sort_by(|a, b| item_time(a).cmp(item_time(b)));

    let active_turn: Option<&str> = if thread.is_running() {
        detail
            .shell
            .latest_turn
            .as_ref()
            .map(|t| t.turn_id.as_str())
    } else {
        None
    };
    let mut blocks: Vec<Block> = Vec::new();
    let mut pending_work: Vec<WorkEntry> = Vec::new();
    let mut work_group_index = 0usize;
    let mut work_anchor: Option<String> = None;

    let flush_work = |blocks: &mut Vec<Block>,
                      pending: &mut Vec<WorkEntry>,
                      anchor: &mut Option<String>,
                      index: &mut usize| {
        if pending.is_empty() {
            return;
        }
        let key = anchor.take().unwrap_or_else(|| format!("work-{index}"));
        *index += 1;
        let is_expanded = expand_all || expanded.contains(&key);
        // Providers do not always emit a completion for every parallel call. Once the
        // turn that owned an entry is over, "in progress" can only be stale.
        for entry in pending.iter_mut() {
            if entry.status == "inProgress" && entry.turn_id.as_deref() != active_turn {
                entry.status = "completed".to_string();
            }
        }
        let (text, rows) = render_work(pending, is_expanded, expanded, &key, width);
        let mut exports = vec![(key.clone(), export_group(pending))];
        exports.extend(
            pending
                .iter()
                .map(|entry| (row_key(&key, entry), export_entry(entry))),
        );
        blocks.push(Block {
            key: BlockKey::Work(key),
            text,
            rows,
            exports,
        });
        pending.clear();
    };

    for item in items {
        match item {
            Item::Message(message) => {
                flush_work(
                    &mut blocks,
                    &mut pending_work,
                    &mut work_anchor,
                    &mut work_group_index,
                );
                let role = match message.role.as_str() {
                    "user" => "you",
                    other => other,
                };
                let text = match message.role.as_str() {
                    // A subagent transcript opens with the brief it was given, which is
                    // the agent's instruction rather than anything the user typed.
                    "user" | "prompt" => render_user(&message.text, role),
                    "system" => render_system(&message.text),
                    _ => render_assistant(&message.text, message.streaming),
                };
                blocks.push(Block {
                    key: BlockKey::Message(message.id.clone()),
                    text,
                    rows: Vec::new(),
                    exports: vec![(
                        format!("msg:{}", message.id),
                        format!("## {role}\n\n{}\n", message.text.trim_end()),
                    )],
                });
            }
            Item::Activity(activity) => {
                if work_anchor.is_none() {
                    work_anchor = Some(format!("work-{}", activity.id));
                }
                merge_work(&mut pending_work, activity);
            }
            Item::Plan(plan) => {
                flush_work(
                    &mut blocks,
                    &mut pending_work,
                    &mut work_anchor,
                    &mut work_group_index,
                );
                blocks.push(Block {
                    key: BlockKey::Plan(plan.id.clone()),
                    text: render_plan(plan),
                    rows: Vec::new(),
                    exports: vec![(
                        format!("plan:{}", plan.id),
                        format!("## proposed plan\n\n{}\n", plan.plan_markdown.trim_end()),
                    )],
                });
            }
        }
    }
    flush_work(
        &mut blocks,
        &mut pending_work,
        &mut work_anchor,
        &mut work_group_index,
    );

    if thread.is_running() {
        blocks.push(Block {
            key: BlockKey::Working,
            text: render_working(thread),
            rows: Vec::new(),
            exports: Vec::new(),
        });
    }
    blocks
}

fn is_visible_activity(activity: &Activity) -> bool {
    matches!(
        activity.kind.as_str(),
        "tool.started"
            | "tool.updated"
            | "tool.completed"
            | "tool.denied"
            | "task.started"
            | "task.progress"
            | "task.updated"
            | "task.completed"
            | "approval.requested"
            | "approval.resolved"
            | "user-input.requested"
            | "runtime.error"
            | "runtime.warning"
            | "runtime.note"
            | "context-compaction"
    ) || activity.kind.starts_with("provider.")
}

fn merge_work(pending: &mut Vec<WorkEntry>, activity: &Activity) {
    let mut entry = match activity.kind.as_str() {
        "tool.started" | "tool.updated" | "tool.completed" | "tool.denied" => {
            let key = activity
                .str("toolCallId")
                .or_else(|| activity.str("toolUseId"))
                .map(str::to_string)
                .unwrap_or_else(|| activity.id.clone());
            let item_type = activity.str("itemType").unwrap_or("");
            let status = if activity.kind == "tool.denied" {
                "declined".to_string()
            } else {
                activity.str("status").unwrap_or("inProgress").to_string()
            };
            let (icon, title) = tool_presentation(item_type, activity);
            WorkEntry {
                key,
                turn_id: activity.turn_id.clone(),
                icon,
                title,
                detail: tool_detail(item_type, activity),
                input: tool_input(item_type, activity),
                output: tool_output(activity),
                files: tool_files(activity),
                status,
                tone: activity.tone.clone(),
            }
        }
        "task.started" | "task.progress" | "task.updated" | "task.completed" => {
            let key = activity
                .str("taskId")
                .map(|id| format!("task:{id}"))
                .unwrap_or_else(|| activity.id.clone());
            // The server stamps which of the two a task is as it ingests it: work the
            // model delegated to a subagent, or a monitor or command left running in the
            // background. They read differently, so they are not drawn the same.
            let is_agent = activity.str("agentKind") == Some("agent");
            let title = activity.str("title").unwrap_or("task");
            // A refinement row without a status says nothing about how the task is
            // going; an empty status leaves the one already on the row alone.
            let status = activity
                .str("status")
                .unwrap_or(match activity.kind.as_str() {
                    "task.completed" => "completed",
                    "task.started" => "inProgress",
                    _ => "",
                });
            // Where it has got to: the step the provider named, falling back to the role
            // it runs as so a row is never bare.
            let step = activity
                .str("summary")
                .or_else(|| activity.str("detail"))
                .map(first_line)
                .filter(|step| !step.is_empty() && *step != title);
            let detail = match (activity.str("role").filter(|_| is_agent), step) {
                (Some(role), Some(step)) => Some(format!("{role} · {step}")),
                (Some(role), None) => Some(role.to_string()),
                (None, step) => step.map(str::to_string),
            };
            WorkEntry {
                key,
                turn_id: activity.turn_id.clone(),
                icon: if is_agent { "⤷" } else { "⚙" },
                title: if is_agent {
                    format!("agent: {title}")
                } else {
                    format!("task: {title}")
                },
                detail,
                input: None,
                // The report it came back with, as far as the server kept it.
                output: (activity.kind == "task.completed")
                    .then(|| activity.str("summary"))
                    .flatten()
                    .map(str::to_string),
                files: Vec::new(),
                status: status.to_string(),
                tone: "info".into(),
            }
        }
        "approval.requested" => WorkEntry {
            key: format!(
                "approval:{}",
                activity.str("requestId").unwrap_or(&activity.id)
            ),
            turn_id: activity.turn_id.clone(),
            icon: "?",
            title: activity.summary.clone(),
            detail: activity.str("detail").map(str::to_string),
            input: None,
            output: None,
            files: Vec::new(),
            status: "inProgress".into(),
            tone: "approval".into(),
        },
        "approval.resolved" => WorkEntry {
            key: format!(
                "approval:{}",
                activity.str("requestId").unwrap_or(&activity.id)
            ),
            turn_id: activity.turn_id.clone(),
            icon: "?",
            title: format!(
                "Approval {}",
                activity.str("decision").unwrap_or("resolved")
            ),
            detail: None,
            input: None,
            output: None,
            files: Vec::new(),
            status: "completed".into(),
            tone: "approval".into(),
        },
        "user-input.requested" => WorkEntry {
            key: activity.id.clone(),
            turn_id: activity.turn_id.clone(),
            icon: "?",
            title: "Agent asked a question".into(),
            detail: None,
            input: None,
            output: None,
            files: Vec::new(),
            status: "completed".into(),
            tone: "approval".into(),
        },
        "context-compaction" => WorkEntry {
            key: activity.id.clone(),
            turn_id: activity.turn_id.clone(),
            icon: "⇣",
            title: "Context compacted".into(),
            detail: None,
            input: None,
            output: None,
            files: Vec::new(),
            status: "completed".into(),
            tone: "info".into(),
        },
        _ => WorkEntry {
            key: activity.id.clone(),
            turn_id: activity.turn_id.clone(),
            icon: if activity.tone == "error" {
                "✗"
            } else {
                "·"
            },
            title: activity.summary.clone(),
            detail: activity
                .str("message")
                .or_else(|| activity.str("detail"))
                .map(str::to_string),
            input: None,
            output: None,
            files: Vec::new(),
            status: if activity.tone == "error" {
                "failed".into()
            } else {
                "completed".into()
            },
            tone: activity.tone.clone(),
        },
    };
    match pending.iter_mut().find(|e| e.key == entry.key) {
        Some(existing) => {
            // Later lifecycle events refine title and detail, but never blank them.
            if !entry.status.is_empty() {
                existing.status = entry.status;
            }
            if !entry.title.is_empty() {
                existing.title = entry.title;
            }
            if entry.detail.is_some() {
                existing.detail = entry.detail;
            }
            if entry.input.is_some() {
                existing.input = entry.input;
            }
            if entry.output.is_some() {
                existing.output = entry.output;
            }
            if !entry.files.is_empty() {
                existing.files = entry.files;
            }
            existing.icon = entry.icon;
        }
        None => {
            // The first row for a call decides how it reads, so it cannot leave the
            // status open the way a later one can.
            if entry.status.is_empty() {
                entry.status = "inProgress".to_string();
            }
            pending.push(entry)
        }
    }
}

fn tool_presentation(item_type: &str, activity: &Activity) -> (&'static str, String) {
    let data = &activity.payload["data"];
    let title = activity.str("title").unwrap_or("").to_string();
    match item_type {
        "command_execution" => (
            "$",
            data.get("description")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or(title),
        ),
        "file_change" => ("✎", title),
        "web_search" => ("⌕", title),
        "image_view" => ("▣", title),
        "collab_agent_tool_call" => ("⤷", title),
        "mcp_tool_call" | "dynamic_tool_call" => ("⚙", title),
        _ => (
            "·",
            if title.is_empty() {
                activity.summary.clone()
            } else {
                title
            },
        ),
    }
}

fn tool_detail(item_type: &str, activity: &Activity) -> Option<String> {
    let data = &activity.payload["data"];
    let detail = match item_type {
        "command_execution" => data
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_string),
        "file_change" => data
            .get("path")
            .or_else(|| data.get("filePath"))
            .or_else(|| data.get("file_path"))
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    };
    detail
        .or_else(|| activity.str("detail").map(str::to_string))
        .map(|d| first_line(&d).to_string())
        .filter(|d| !d.is_empty())
}

/// The full input the server kept: the command, else the provider's detail string.
fn tool_input(item_type: &str, activity: &Activity) -> Option<String> {
    let data = &activity.payload["data"];
    let command = if item_type == "command_execution" {
        data.get("command").and_then(Value::as_str)
    } else {
        None
    };
    command
        .or_else(|| activity.str("detail"))
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string)
}

/// The output summary the server kept: a first line, a line count, or a file count.
fn tool_output(activity: &Activity) -> Option<String> {
    let data = &activity.payload["data"];
    let raw = data.get("rawOutput");
    if let Some(content) = raw
        .and_then(|r| r.get("content"))
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())
    {
        return Some(content.trim().to_string());
    }
    if let Some(total) = raw
        .and_then(|r| r.get("totalFiles"))
        .and_then(Value::as_u64)
    {
        let truncated = raw
            .and_then(|r| r.get("truncated"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return Some(format!(
            "{total} file{}{}",
            if total == 1 { "" } else { "s" },
            if truncated { " (truncated)" } else { "" }
        ));
    }
    data.get("result")
        .and_then(|r| r.get("content"))
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())
        .map(|c| c.trim().to_string())
}

fn tool_files(activity: &Activity) -> Vec<String> {
    activity.payload["data"]
        .get("files")
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(|f| f.get("path").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

fn status_style(entry: &WorkEntry) -> Style {
    match (entry.tone.as_str(), entry.status.as_str()) {
        ("error", _) | (_, "failed") => Style::default().fg(Color::Red),
        (_, "declined") => Style::default().fg(Color::Yellow),
        ("approval", _) => Style::default().fg(Color::Yellow),
        (_, "inProgress") => Style::default().fg(Color::Cyan),
        _ => Style::default().fg(Color::DarkGray),
    }
}

/// A region of a block that a key folds: the text lines it covers, the key, and whether
/// folding it shows anything. A tool row the server kept no input or output for is a
/// region all the same — it owns its line, so a click on it is not a click on the group.
#[derive(Debug, Clone)]
pub struct Region {
    pub first: usize,
    pub end: usize,
    pub key: String,
    pub foldable: bool,
}

type Rows = Vec<Region>;

/// Plain text for one tool call: title, status, the input the server kept, the output
/// summary, and changed files.
fn export_entry(entry: &WorkEntry) -> String {
    let mut out = format!("{} {}", entry.icon, entry.title);
    if entry.status != "completed" {
        out.push_str(&format!(" ({})", entry.status));
    }
    out.push('\n');
    if let Some(input) = &entry.input {
        out.push('\n');
        out.push_str(input.trim_end());
        out.push('\n');
    } else if let Some(detail) = &entry.detail {
        out.push('\n');
        out.push_str(detail);
        out.push('\n');
    }
    if let Some(output) = &entry.output {
        out.push_str("\n→ ");
        out.push_str(output.trim_end());
        out.push('\n');
    }
    if !entry.files.is_empty() {
        out.push('\n');
        for file in &entry.files {
            out.push_str("✎ ");
            out.push_str(file);
            out.push('\n');
        }
    }
    out
}

fn export_group(entries: &[WorkEntry]) -> String {
    let mut out = format!(
        "## {} tool call{}\n",
        entries.len(),
        if entries.len() == 1 { "" } else { "s" }
    );
    for entry in entries {
        out.push('\n');
        out.push_str(&export_entry(entry));
    }
    out
}

fn render_work(
    entries: &[WorkEntry],
    expanded: bool,
    expanded_keys: &HashSet<String>,
    group_key: &str,
    width: u16,
) -> (Text<'static>, Rows) {
    let running = entries.iter().filter(|e| e.status == "inProgress").count();
    let failed = entries
        .iter()
        .filter(|e| e.status == "failed" || e.tone == "error")
        .count();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut rows: Rows = Vec::new();
    let mut header = vec![
        Span::styled(
            if expanded { "▾ " } else { "▸ " },
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!(
                "{} tool call{}",
                entries.len(),
                if entries.len() == 1 { "" } else { "s" }
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if running > 0 {
        header.push(Span::styled(
            format!("  {running} running"),
            Style::default().fg(Color::Cyan),
        ));
    }
    if failed > 0 {
        header.push(Span::styled(
            format!("  {failed} failed"),
            Style::default().fg(Color::Red),
        ));
    }
    if !expanded && let Some(last) = entries.last() {
        let mut summary = format!("  {} {}", last.icon, last.title);
        if let Some(detail) = &last.detail {
            summary.push_str(&format!(": {detail}"));
        }
        header.push(Span::styled(
            truncate(&summary, width.saturating_sub(24) as usize),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }
    lines.push(Line::from(header));
    rows.push(Region {
        first: 0,
        end: 1,
        key: group_key.to_string(),
        foldable: true,
    });
    if !expanded {
        return (Text::from(lines), rows);
    }
    let dim = Style::default().fg(Color::DarkGray);
    for entry in entries {
        let key = row_key(group_key, entry);
        let open = entry.has_more() && expanded_keys.contains(&key);
        let marker = if !entry.has_more() {
            "  "
        } else if open {
            "▾ "
        } else {
            "▸ "
        };
        let first = lines.len();
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(marker, dim),
            Span::styled(format!("{} ", entry.icon), status_style(entry)),
            Span::styled(
                entry.title.clone(),
                status_style(entry).add_modifier(Modifier::BOLD),
            ),
        ];
        if !open && let Some(detail) = &entry.detail {
            spans.push(Span::styled(
                format!(
                    "  {}",
                    truncate(
                        detail,
                        width.saturating_sub(entry.title.len() as u16 + 10) as usize
                    )
                ),
                dim,
            ));
        }
        lines.push(Line::from(spans));
        if open {
            if let Some(input) = &entry.input {
                for line in input.lines() {
                    lines.push(Line::from(vec![
                        Span::raw("      "),
                        Span::styled(line.to_string(), Style::default().fg(Color::Gray)),
                    ]));
                }
            }
            if let Some(output) = &entry.output {
                for (i, line) in output.lines().enumerate() {
                    lines.push(Line::from(vec![
                        Span::styled(if i == 0 { "      → " } else { "        " }, dim),
                        Span::styled(line.to_string(), Style::default()),
                    ]));
                }
            }
            for file in &entry.files {
                lines.push(Line::from(vec![
                    Span::styled("      ✎ ", dim),
                    Span::styled(file.clone(), Style::default().fg(Color::Gray)),
                ]));
            }
            if entry.status != "completed" {
                lines.push(Line::from(vec![
                    Span::raw("      "),
                    Span::styled(entry.status.clone(), status_style(entry)),
                ]));
            }
        }
        rows.push(Region {
            first,
            end: lines.len(),
            key,
            foldable: entry.has_more(),
        });
    }
    (Text::from(lines), rows)
}

fn truncate(text: &str, max: usize) -> String {
    if max < 4 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= max {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

fn render_user(text: &str, label: &str) -> Text<'static> {
    let style = Style::default().fg(Color::Green);
    let mut lines = vec![Line::from(vec![
        Span::styled(USER_MARK, style),
        Span::styled(format!(" {label}"), style.add_modifier(Modifier::BOLD)),
    ])];
    for line in text.lines() {
        lines.push(Line::from(vec![
            Span::styled(USER_MARK, style),
            Span::raw(format!(" {line}")),
        ]));
    }
    if text.is_empty() {
        lines.push(Line::from(Span::styled(USER_MARK, style)));
    }
    lines.push(Line::default());
    Text::from(lines)
}

fn render_system(text: &str) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = text
        .lines()
        .map(|l| {
            Line::from(Span::styled(
                l.to_string(),
                Style::default().fg(Color::DarkGray).italic(),
            ))
        })
        .collect();
    lines.push(Line::default());
    Text::from(lines)
}

fn render_assistant(text: &str, streaming: bool) -> Text<'static> {
    let mut rendered = markdown(text);
    if streaming {
        let cursor = Span::styled(STREAM_CURSOR, Style::default().fg(Color::Magenta));
        match rendered.lines.last_mut() {
            Some(last) => last.spans.push(cursor),
            None => rendered.lines.push(Line::from(cursor)),
        }
    }
    rendered.lines.push(Line::default());
    rendered
}

fn render_plan(plan: &crate::model::ProposedPlan) -> Text<'static> {
    let style = Style::default().fg(Color::Blue);
    let mut lines = vec![Line::from(vec![
        Span::styled("▌ ", style),
        Span::styled("Proposed plan", style.add_modifier(Modifier::BOLD)),
        Span::styled(
            if plan.implemented_at.is_some() {
                "  (implemented)"
            } else {
                ""
            },
            Style::default().fg(Color::DarkGray),
        ),
    ])];
    for mut line in markdown(&plan.plan_markdown).lines {
        line.spans.insert(0, Span::styled("▌ ", style));
        lines.push(line);
    }
    lines.push(Line::default());
    Text::from(lines)
}

fn render_working(thread: &ThreadState) -> Text<'static> {
    let style = Style::default().fg(Color::Magenta);
    let mut lines = Vec::new();
    let steps = thread.active_plan();
    if steps.is_empty() {
        let label = thread
            .detail
            .shell
            .plan_progress
            .as_ref()
            .map(|p| p.step.clone())
            .unwrap_or_else(|| "working".to_string());
        lines.push(Line::from(Span::styled(format!("… {label}"), style)));
    } else {
        for step in steps {
            let (mark, st) = match step.status.as_str() {
                "completed" => ("✓", Style::default().fg(Color::DarkGray)),
                "inProgress" => ("…", style),
                _ => ("○", Style::default().fg(Color::DarkGray)),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{mark} "), st),
                Span::styled(step.step, st),
            ]));
        }
    }
    Text::from(lines)
}

/// Render markdown to an owned `Text`.
pub fn markdown(text: &str) -> Text<'static> {
    let rendered = tui_markdown::from_str(text);
    Text {
        lines: rendered
            .lines
            .into_iter()
            .map(|line| Line {
                spans: line
                    .spans
                    .into_iter()
                    .map(|span| Span {
                        content: span.content.into_owned().into(),
                        style: span.style,
                    })
                    .collect(),
                style: line.style,
                alignment: line.alignment,
            })
            .collect(),
        style: rendered.style,
        alignment: rendered.alignment,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn activity(kind: &str, payload: serde_json::Value) -> Activity {
        serde_json::from_value(json!({
            "id": format!("{kind}-{}", payload["taskId"]),
            "kind": kind,
            "tone": "info",
            "summary": "",
            "payload": payload,
            "createdAt": "1",
        }))
        .unwrap()
    }

    fn work(rows: &[Activity]) -> Vec<WorkEntry> {
        let mut pending = Vec::new();
        for row in rows {
            merge_work(&mut pending, row);
        }
        pending
    }

    /// Every row of an expanded group owns its lines, whether or not it has anything to
    /// unfold. Without that, a click on a row the server kept no payload for lands on the
    /// group instead and shuts the whole thing.
    #[test]
    fn a_row_with_nothing_to_unfold_still_owns_its_line() {
        let entries = work(&[
            activity(
                "task.started",
                json!({"taskId": "a1", "agentKind": "agent", "title": "Review the diff"}),
            ),
            activity(
                "task.completed",
                json!({"taskId": "a2", "agentKind": "agent", "title": "Check the tests",
                       "summary": "All green."}),
            ),
        ]);
        let (_, rows) = render_work(&entries, true, &HashSet::new(), "work-1", 80);
        let keys: Vec<(&str, bool)> = rows
            .iter()
            .map(|region| (region.key.as_str(), region.foldable))
            .collect();
        assert_eq!(
            keys,
            [
                ("work-1", true),
                ("work-1/task:a1", false),
                ("work-1/task:a2", true),
            ]
        );
        // Contiguous from the header down, so no line inside the group falls through to it.
        assert_eq!(rows[0].first, 0);
        assert_eq!(rows[1].first, rows[0].end);
        assert_eq!(rows[2].first, rows[1].end);
    }

    #[test]
    fn a_subagent_and_a_background_task_read_differently() {
        let rows = [
            activity(
                "task.started",
                json!({"taskId": "a1", "agentKind": "agent", "title": "Review the diff",
                       "role": "general-purpose", "detail": "Review the diff"}),
            ),
            activity(
                "task.started",
                json!({"taskId": "b1", "agentKind": "background", "title": "Watch the build"}),
            ),
        ];
        let entries = work(&rows);
        assert_eq!(entries[0].icon, "⤷");
        assert_eq!(entries[0].title, "agent: Review the diff");
        // The detail the row repeats from its own title is not worth a second line.
        assert_eq!(entries[0].detail.as_deref(), Some("general-purpose"));
        assert_eq!(entries[1].icon, "⚙");
        assert_eq!(entries[1].title, "task: Watch the build");
        assert_eq!(entries[1].detail, None);
    }

    #[test]
    fn progress_refines_the_row_and_completion_carries_the_report() {
        let rows = [
            activity(
                "task.started",
                json!({"taskId": "a1", "agentKind": "agent", "title": "Review the diff",
                       "role": "general-purpose"}),
            ),
            activity(
                "task.progress",
                json!({"taskId": "a1", "detail": "Running Read display utils"}),
            ),
            activity(
                "task.completed",
                json!({"taskId": "a1", "status": "completed", "summary": "Three findings"}),
            ),
        ];
        let entries = work(&rows);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, "completed");
        assert_eq!(entries[0].output.as_deref(), Some("Three findings"));
        assert!(entries[0].has_more());
    }

    #[test]
    fn a_row_without_a_status_leaves_the_one_it_has() {
        let rows = [
            activity(
                "task.started",
                json!({"taskId": "a1", "agentKind": "agent", "title": "Review the diff"}),
            ),
            activity(
                "task.completed",
                json!({"taskId": "a1", "status": "completed", "summary": "done"}),
            ),
            activity(
                "task.updated",
                json!({"taskId": "a1", "isBackgrounded": true}),
            ),
        ];
        assert_eq!(work(&rows)[0].status, "completed");
    }
}

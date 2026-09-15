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
}

#[derive(Debug, Clone)]
struct WorkEntry {
    key: String,
    turn_id: Option<String>,
    icon: &'static str,
    title: String,
    detail: Option<String>,
    status: String,
    tone: String,
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
        blocks.push(Block {
            key: BlockKey::Work(key),
            text: render_work(pending, is_expanded, width),
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
                let text = match message.role.as_str() {
                    "user" => render_user(&message.text),
                    "system" => render_system(&message.text),
                    _ => render_assistant(&message.text, message.streaming),
                };
                blocks.push(Block {
                    key: BlockKey::Message(message.id.clone()),
                    text,
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
    let entry = match activity.kind.as_str() {
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
                status,
                tone: activity.tone.clone(),
            }
        }
        "task.started" | "task.completed" => {
            let key = activity
                .str("taskId")
                .map(|id| format!("task:{id}"))
                .unwrap_or_else(|| activity.id.clone());
            let status = if activity.kind == "task.completed" {
                activity.str("status").unwrap_or("completed").to_string()
            } else {
                "inProgress".to_string()
            };
            WorkEntry {
                key,
                turn_id: activity.turn_id.clone(),
                icon: "⤷",
                title: format!("agent: {}", activity.str("title").unwrap_or("task")),
                detail: activity.str("role").map(str::to_string),
                status,
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
            status: "completed".into(),
            tone: "approval".into(),
        },
        "user-input.requested" => WorkEntry {
            key: activity.id.clone(),
            turn_id: activity.turn_id.clone(),
            icon: "?",
            title: "Agent asked a question".into(),
            detail: None,
            status: "completed".into(),
            tone: "approval".into(),
        },
        "context-compaction" => WorkEntry {
            key: activity.id.clone(),
            turn_id: activity.turn_id.clone(),
            icon: "⇣",
            title: "Context compacted".into(),
            detail: None,
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
            existing.status = entry.status;
            if !entry.title.is_empty() {
                existing.title = entry.title;
            }
            if entry.detail.is_some() {
                existing.detail = entry.detail;
            }
            existing.icon = entry.icon;
        }
        None => pending.push(entry),
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

fn render_work(entries: &[WorkEntry], expanded: bool, width: u16) -> Text<'static> {
    let running = entries.iter().filter(|e| e.status == "inProgress").count();
    let failed = entries
        .iter()
        .filter(|e| e.status == "failed" || e.tone == "error")
        .count();
    let mut lines: Vec<Line<'static>> = Vec::new();
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
    if expanded {
        for entry in entries {
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(format!("{} ", entry.icon), status_style(entry)),
                Span::styled(
                    entry.title.clone(),
                    status_style(entry).add_modifier(Modifier::BOLD),
                ),
            ];
            if let Some(detail) = &entry.detail {
                spans.push(Span::styled(
                    format!(
                        "  {}",
                        truncate(
                            detail,
                            width.saturating_sub(entry.title.len() as u16 + 8) as usize
                        )
                    ),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            lines.push(Line::from(spans));
        }
    }
    Text::from(lines)
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

fn render_user(text: &str) -> Text<'static> {
    let style = Style::default().fg(Color::Green);
    let mut lines = vec![Line::from(vec![
        Span::styled(USER_MARK, style),
        Span::styled(" you", style.add_modifier(Modifier::BOLD)),
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

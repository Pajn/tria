//! Derives renderable blocks from a thread: user and assistant messages,
//! collapsed tool groups, plan cards, and error lines, in chronological order.

use std::collections::HashSet;

use ratatui::{
    layout::Size,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use serde_json::Value;

use crate::{model::Activity, picture, state::ThreadState};

pub const USER_MARK: &str = "▌";
const STREAM_CURSOR: &str = "▍";
/// Where an image starts, lined up with the output of the row it belongs to.
const IMAGE_INDENT: u16 = 6;

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
    /// Images an open row left room for, in text-line indices before wrapping. The lines
    /// they sit on are blank; the renderer draws over them.
    pub images: Vec<Placed>,
}

/// Where in a block an image goes. How large it is the drawing cache already knows.
#[derive(Debug, Clone)]
pub struct Placed {
    /// Text-line index the image starts on, before wrapping.
    pub line: usize,
    pub indent: u16,
    /// What the image is known by in the drawing cache.
    pub key: String,
}

#[derive(Debug, Clone)]
struct WorkEntry<'a> {
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
    /// Images the call came back with, borrowed from the activity that carried them:
    /// the bytes run to hundreds of kilobytes and the timeline is rebuilt often, so they
    /// are never copied for a row that may stay shut.
    images: Vec<ToolImage<'a>>,
    status: String,
    tone: String,
}

/// One image in a tool result: what to know it by, and where its bytes are.
#[derive(Debug, Clone)]
struct ToolImage<'a> {
    key: String,
    source: picture::Source<'a>,
}

impl WorkEntry<'_> {
    fn has_more(&self) -> bool {
        self.input
            .as_deref()
            .is_some_and(|i| Some(i) != self.detail.as_deref())
            || self.output.is_some()
            || !self.files.is_empty()
            || !self.images.is_empty()
    }
}

/// Expand key of one tool row inside a work group.
fn row_key(group: &str, entry: &WorkEntry<'_>) -> String {
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

/// Build the ordered blocks for a thread. `expanded` holds the keys of groups and calls
/// opened by hand; `open_levels` opens them wholesale, one level for the groups and two
/// for what is inside every call in them.
/// The height is the chat's own: it bounds how much of the window one image may take.
pub fn build(
    thread: &ThreadState,
    expanded: &HashSet<String>,
    open_levels: u8,
    width: u16,
    height: u16,
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
        let is_expanded = open_levels >= 1 || expanded.contains(&key);
        // Providers do not always emit a completion for every parallel call. Once the
        // turn that owned an entry is over, "in progress" can only be stale.
        for entry in pending.iter_mut() {
            if entry.status == "inProgress" && entry.turn_id.as_deref() != active_turn {
                entry.status = "completed".to_string();
            }
        }
        let (text, rows, images) = render_work(
            pending,
            is_expanded,
            open_levels >= 2,
            expanded,
            &key,
            width,
            height,
        );
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
            images,
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
                    images: Vec::new(),
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
                    images: Vec::new(),
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
            images: Vec::new(),
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

fn merge_work<'a>(pending: &mut Vec<WorkEntry<'a>>, activity: &'a Activity) {
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
                turn_id: activity.turn_id.clone(),
                icon,
                title,
                detail: tool_detail(item_type, activity),
                input: tool_input(item_type, activity),
                output: tool_output(activity),
                files: tool_files(activity),
                images: tool_images(&key, item_type, activity),
                key,
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
                images: Vec::new(),
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
            images: Vec::new(),
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
            images: Vec::new(),
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
            images: Vec::new(),
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
            images: Vec::new(),
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
            images: Vec::new(),
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
            if !entry.images.is_empty() {
                existing.images = entry.images;
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
        .and_then(result_text)
}

/// The text of a result: a string for most tools, and the blocks of one for the rest.
/// Images are drawn rather than described, so they are left out here; anything else that
/// is not text is named, so a row never reads as empty when something came back.
fn result_text(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(text) => text.trim().to_string(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| text.trim().to_string()),
                Some("image") => None,
                Some(other) => Some(format!("[{other}]")),
                None => None,
            })
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// The images a call has to show. Each is known by the call it belongs to, so an image
/// keeps its place in the drawing cache across the rebuilds of a running turn.
///
/// A transcript carries the picture itself, because it is the provider's file read whole.
/// A thread's own rows carry the path and nothing else: the server summarises tool
/// results on the way out and an image is not something a summary can hold. So the path
/// is the image, when the disk it names is the one under us.
fn tool_images<'a>(key: &str, item_type: &str, activity: &'a Activity) -> Vec<ToolImage<'a>> {
    let data = &activity.payload["data"];
    let carried: Vec<ToolImage<'a>> = data["result"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter(|(_, block)| block.get("type").and_then(Value::as_str) == Some("image"))
        .filter_map(|(index, block)| {
            let source = block.get("source")?;
            if source.get("type").and_then(Value::as_str) != Some("base64") {
                return None;
            }
            Some(ToolImage {
                key: format!("{key}/{index}"),
                source: picture::Source::Data(source.get("data").and_then(Value::as_str)?),
            })
        })
        .collect();
    if !carried.is_empty() || item_type != "image_view" || !picture::reads_files() {
        return carried;
    }
    let named = data
        .get("imagePath")
        .or_else(|| data.pointer("/input/file_path"))
        .or_else(|| data.get("path"))
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty());
    named
        .map(|path| ToolImage {
            key: format!("{key}/file"),
            source: picture::Source::File(path),
        })
        .into_iter()
        .collect()
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

fn status_style(entry: &WorkEntry<'_>) -> Style {
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
fn export_entry(entry: &WorkEntry<'_>) -> String {
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
    if !entry.images.is_empty() {
        out.push_str(&format!(
            "\n→ {} image{}\n",
            entry.images.len(),
            if entry.images.len() == 1 { "" } else { "s" }
        ));
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

fn export_group(entries: &[WorkEntry<'_>]) -> String {
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
    entries: &[WorkEntry<'_>],
    expanded: bool,
    // Every call in the group open, not only the ones opened by hand.
    all_open: bool,
    expanded_keys: &HashSet<String>,
    group_key: &str,
    width: u16,
    height: u16,
) -> (Text<'static>, Rows, Vec<Placed>) {
    let running = entries.iter().filter(|e| e.status == "inProgress").count();
    let failed = entries
        .iter()
        .filter(|e| e.status == "failed" || e.tone == "error")
        .count();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut rows: Rows = Vec::new();
    let mut images: Vec<Placed> = Vec::new();
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
        return (Text::from(lines), rows, images);
    }
    let dim = Style::default().fg(Color::DarkGray);
    for entry in entries {
        let key = row_key(group_key, entry);
        let open = entry.has_more() && (all_open || expanded_keys.contains(&key));
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
            for image in &entry.images {
                match picture::place(&image.key, image.source, image_room(width, height)) {
                    // Blank lines, which the renderer draws the image over once it knows
                    // where on the screen they landed.
                    Some(size) => {
                        images.push(Placed {
                            line: lines.len(),
                            indent: IMAGE_INDENT,
                            key: image.key.clone(),
                        });
                        lines.extend(std::iter::repeat_n(
                            Line::from(" ".repeat(IMAGE_INDENT as usize)),
                            size.height as usize,
                        ));
                    }
                    // Nothing here can draw it. A file a tool read is often a temporary
                    // one that has since been cleaned up, and that is worth saying: the
                    // row above names the file, and this says what became of it.
                    None => lines.push(Line::from(vec![
                        Span::styled("      → ", dim),
                        Span::styled(missing(image), Style::default().fg(Color::Gray)),
                    ])),
                }
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
    (Text::from(lines), rows, images)
}

/// What to say in place of an image that cannot be drawn.
fn missing(image: &ToolImage<'_>) -> &'static str {
    match image.source {
        picture::Source::File(path) if std::fs::metadata(path).is_err() => {
            "[image · nothing at that path now]"
        }
        _ => "[image]",
    }
}

/// The room one image is given: what the row leaves of the width, and half the window,
/// so what follows an open image is still in view.
fn image_room(width: u16, height: u16) -> Size {
    Size::new(
        width.saturating_sub(IMAGE_INDENT + 2),
        (height / 2).clamp(4, 24),
    )
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
            .unwrap_or_else(|| {
                if thread.is_compacting() {
                    "compacting the context".to_string()
                } else {
                    "working".to_string()
                }
            });
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
    use base64::Engine;
    use ratatui::widgets::{Paragraph, Wrap};
    use serde_json::json;

    use super::*;

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

    fn work(rows: &[Activity]) -> Vec<WorkEntry<'_>> {
        let mut pending = Vec::new();
        for row in rows {
            merge_work(&mut pending, row);
        }
        pending
    }

    fn image_call(data: &str) -> Activity {
        serde_json::from_value(json!({
            "id": "a-image",
            "kind": "tool.completed",
            "tone": "info",
            "summary": "Image view",
            "createdAt": "1",
            "payload": {
                "itemType": "image_view",
                "toolCallId": "t1",
                "status": "completed",
                "title": "Image view",
                "detail": "Read: /tmp/shot.png",
                "data": {
                    "toolName": "Read",
                    "result": { "type": "tool_result", "content": [
                        { "type": "text", "text": "the window" },
                        { "type": "image", "source": {
                            "type": "base64", "media_type": "image/png", "data": data } },
                    ]},
                },
            },
        }))
        .unwrap()
    }

    /// An image read is a row worth opening, and what it opens onto is the picture: the
    /// lines it reserves are blank, and one screen line each once wrapped.
    #[test]
    fn an_open_image_row_keeps_the_lines_its_picture_needs() {
        picture::draw_in_halfblocks();
        let data = picture::test_png(200, 100);
        let rows = [image_call(&data)];
        let entries = work(&rows);
        assert!(entries[0].has_more(), "an image is something to unfold");
        let open = HashSet::from(["work-1/t1".to_string()]);
        let (text, regions, images) = render_work(&entries, true, false, &open, "work-1", 80, 24);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].key, "t1/1");
        let size = picture::place(
            &images[0].key,
            picture::Source::Data(&data),
            image_room(80, 24),
        )
        .unwrap();
        let reserved = images[0].line..images[0].line + size.height as usize;
        assert_eq!(regions[1].end, reserved.end, "the row owns them");
        for index in reserved {
            let line = &text.lines[index];
            assert!(
                line.spans.iter().all(|span| span.content.trim().is_empty()),
                "line {index} is left blank for the image"
            );
            assert_eq!(
                Paragraph::new(Text::from(line.clone()))
                    .wrap(Wrap { trim: false })
                    .line_count(80),
                1,
                "line {index} is one line on the screen"
            );
        }
    }

    /// What a thread's own image row carries: the path the tool read, and no picture.
    /// That is the row the fix is for — it used to have nothing to unfold.
    fn image_row_naming(path: &str) -> Activity {
        serde_json::from_value(json!({
            "id": "a-named",
            "kind": "tool.completed",
            "tone": "info",
            "summary": "Image view",
            "createdAt": "1",
            "payload": {
                "itemType": "image_view",
                "toolCallId": "t2",
                "status": "completed",
                "title": "Image view",
                "detail": path,
                "data": { "imagePath": path, "toolName": "Read" },
            },
        }))
        .unwrap()
    }

    /// The server sends a path where the picture would not fit, so the path is the
    /// picture: the row unfolds onto the file it names.
    #[test]
    fn a_row_that_only_names_its_image_still_shows_it() {
        picture::draw_in_halfblocks();
        let path = std::env::temp_dir().join("tria-a-row-names.png");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(picture::test_png(120, 60))
            .unwrap();
        std::fs::write(&path, bytes).unwrap();
        let rows = [image_row_naming(path.to_str().unwrap())];
        let entries = work(&rows);
        assert!(entries[0].has_more(), "the file is something to unfold");
        let open = HashSet::from(["work-1/t2".to_string()]);
        let (_, _, images) = render_work(&entries, true, false, &open, "work-1", 80, 24);
        assert_eq!(images.len(), 1);
        std::fs::remove_file(&path).unwrap();
    }

    /// The text of a result still reads as text when an image came with it, and the
    /// picture is not described twice. A call is reported before it has a result, so the
    /// image arrives on the row that completes it and has to survive the merge.
    #[test]
    fn what_came_back_beside_the_image_is_still_read() {
        let started: Activity = serde_json::from_value(json!({
            "id": "a-image",
            "kind": "tool.started",
            "tone": "info",
            "summary": "Image view",
            "createdAt": "0",
            "payload": {
                "itemType": "image_view",
                "toolCallId": "t1",
                "status": "inProgress",
                "title": "Image view",
                "data": { "imagePath": "/tmp/shot.png", "toolName": "Read" },
            },
        }))
        .unwrap();
        let rows = [started, image_call("not-an-image")];
        let entries = work(&rows);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].output.as_deref(), Some("the window"));
        assert_eq!(entries[0].images.len(), 1);
    }

    /// The second level opens what every call kept, which is the one the group's own
    /// level cannot reach: a group open is a list of calls a line each.
    #[test]
    fn the_second_level_opens_what_the_calls_kept() {
        let run: Activity = serde_json::from_value(json!({
            "id": "a-run",
            "kind": "tool.completed",
            "tone": "info",
            "summary": "Bash",
            "createdAt": "1",
            "payload": {
                "itemType": "command_execution",
                "toolCallId": "t9",
                "status": "completed",
                "title": "Bash",
                "data": {
                    "command": "cargo test",
                    "result": { "type": "tool_result", "content": [
                        { "type": "text", "text": "142 passed" },
                    ]},
                },
            },
        }))
        .unwrap();
        let rows = [run];
        let entries = work(&rows);
        let text = |all_open| {
            let (text, _, _) =
                render_work(&entries, true, all_open, &HashSet::new(), "work-1", 80, 24);
            text.to_string()
        };
        // A group open is the call on its line, and no more than that.
        assert!(text(false).contains("cargo test"));
        assert!(!text(false).contains("142 passed"), "{}", text(false));
        // A level deeper is what the call came back with, without any one of them having
        // to be asked for by name the way a fold opened by hand is.
        assert!(text(true).contains("142 passed"), "{}", text(true));
    }

    /// Every row of an expanded group owns its lines, whether or not it has anything to
    /// unfold. Without that, a click on a row the server kept no payload for lands on the
    /// group instead and shuts the whole thing.
    #[test]
    fn a_row_with_nothing_to_unfold_still_owns_its_line() {
        let rows = [
            activity(
                "task.started",
                json!({"taskId": "a1", "agentKind": "agent", "title": "Review the diff"}),
            ),
            activity(
                "task.completed",
                json!({"taskId": "a2", "agentKind": "agent", "title": "Check the tests",
                       "summary": "All green."}),
            ),
        ];
        let entries = work(&rows);
        let (_, regions, _) = render_work(&entries, true, false, &HashSet::new(), "work-1", 80, 24);
        let keys: Vec<(&str, bool)> = regions
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
        assert_eq!(regions[0].first, 0);
        assert_eq!(regions[1].first, regions[0].end);
        assert_eq!(regions[2].first, regions[1].end);
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
// ── Wrapping ───────────────────────────────────────────────────────────

/// One drawn line of a block, and where in the block's text it came from.
#[derive(Debug, Clone)]
pub struct Row {
    /// The line of the block's text this row is part of.
    pub line: usize,
    /// Columns of decoration the row opens with — a mark, an indent. The text starts
    /// after them, which is what a selection has to know.
    pub indent: u16,
    /// The row's text as a byte range of its line, decoration not counted.
    pub start: usize,
    pub end: usize,
}

/// A block's text broken into the lines it is drawn as.
#[derive(Debug, Clone, Default)]
pub struct Wrapped {
    /// The rows as drawn, decoration and all.
    pub lines: Vec<Line<'static>>,
    pub rows: Vec<Row>,
    /// Each line of the block's text as it was written, without its decoration, so a
    /// selection can give back what was said rather than what was shown.
    pub texts: Vec<String>,
    /// The first row of each line of the text, with the total at the end, for turning a
    /// range of text lines into a range of rows.
    pub starts: Vec<usize>,
}

/// Break a block's text into the rows it fits `width` as.
///
/// The wrapping is ours rather than the paragraph widget's for two reasons. A line that
/// carries a mark — a message of yours, a plan — loses it on every row but the first
/// when something else does the breaking, because the mark is only the line's first
/// span. And selecting part of a line means knowing where the breaks fell and which
/// columns are decoration.
///
/// So the mark and indent a line opens with are repeated on each of its rows, and the
/// text is wrapped to what is left. Words are kept whole where they fit; one that never
/// fits is broken at the edge.
pub fn wrap(text: &Text<'static>, width: u16) -> Wrapped {
    let width = width.max(1) as usize;
    let mut out = Wrapped::default();
    for (index, line) in text.lines.iter().enumerate() {
        out.starts.push(out.lines.len());
        let chars = chars_of(line);
        let (prefix, body) = chars.split_at(hanging(&chars));
        let room = width.saturating_sub(display_width(prefix)).max(1);
        // Byte offset of every character of the body, so a row can say where it starts.
        let mut offsets = Vec::with_capacity(body.len() + 1);
        let mut at = 0;
        for (ch, _) in body {
            offsets.push(at);
            at += ch.len_utf8();
        }
        offsets.push(at);
        for (n, (from, to)) in break_line(body, room).into_iter().enumerate() {
            let mut row: Vec<(char, Style)> = if n == 0 {
                prefix.to_vec()
            } else {
                // The mark again, so a message broken over rows still reads as one; the
                // rest of the decoration only holds the text where it was.
                prefix
                    .iter()
                    .map(|(ch, style)| (if *ch == USER_MARK_CHAR { *ch } else { ' ' }, *style))
                    .collect()
            };
            let indent = display_width(&row) as u16;
            row.extend_from_slice(&body[from..to]);
            out.rows.push(Row {
                line: index,
                indent,
                start: offsets[from],
                end: offsets[to],
            });
            out.lines.push(line_of(&row));
        }
        out.texts.push(body.iter().map(|(ch, _)| *ch).collect());
    }
    out.starts.push(out.lines.len());
    out
}

impl Wrapped {
    /// What a row says, without the decoration it opens with.
    pub fn text(&self, row: usize) -> &str {
        match self.rows.get(row) {
            Some(row) => &self.texts[row.line][row.start..row.end],
            None => "",
        }
    }

    /// How many characters a row can be addressed by.
    pub fn len(&self, row: usize) -> usize {
        self.text(row).chars().count()
    }

    /// The column a character of a row is drawn at, counted from the block's left edge.
    /// One past the last character answers the column after it.
    pub fn column(&self, row: usize, index: usize) -> u16 {
        let indent = self.rows.get(row).map_or(0, |row| row.indent);
        indent
            + self
                .text(row)
                .chars()
                .take(index)
                .map(char_width)
                .sum::<usize>() as u16
    }

    /// The character of a row drawn at a column, for a click or a drag. A column before
    /// the text answers its first character, one past its end the character after it.
    pub fn index(&self, row: usize, column: u16) -> usize {
        let mut at = self.rows.get(row).map_or(0, |row| row.indent);
        for (index, ch) in self.text(row).chars().enumerate() {
            at += char_width(ch).max(1) as u16;
            if column < at {
                return index;
            }
        }
        self.len(row)
    }

    /// Where a character of a row sits in its line, as a byte offset.
    pub fn byte(&self, row: usize, index: usize) -> usize {
        let Some(at) = self.rows.get(row) else {
            return 0;
        };
        self.text(row)
            .char_indices()
            .nth(index)
            .map_or(at.end, |(byte, _)| at.start + byte)
    }
}

const USER_MARK_CHAR: char = '▌';

fn chars_of(line: &Line<'static>) -> Vec<(char, Style)> {
    let mut out = Vec::new();
    for span in &line.spans {
        let style = line.style.patch(span.style);
        out.extend(span.content.chars().map(|ch| (ch, style)));
    }
    out
}

fn line_of(chars: &[(char, Style)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (ch, style) in chars {
        match spans.last_mut() {
            Some(last) if last.style == *style => last.content.to_mut().push(*ch),
            _ => spans.push(Span::styled(ch.to_string(), *style)),
        }
    }
    Line::from(spans)
}

fn char_width(ch: char) -> usize {
    unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
}

fn display_width(chars: &[(char, Style)]) -> usize {
    chars.iter().map(|(ch, _)| char_width(*ch)).sum()
}

/// How much of the start of a line is decoration rather than what it says: the indent,
/// and the mark a message or a plan is drawn with. A line of nothing but whitespace has
/// none — it is a blank line, not an indent standing on its own.
fn hanging(chars: &[(char, Style)]) -> usize {
    let mut hang = 0;
    while let Some((ch, _)) = chars.get(hang) {
        if *ch == USER_MARK_CHAR || ch.is_whitespace() {
            hang += 1;
        } else {
            break;
        }
    }
    if hang == chars.len() { 0 } else { hang }
}

/// Greedy word wrapping over a line's body, as character ranges of it: whole words while
/// they fit, the break swallowing the whitespace it falls on, and a word too long for a
/// row of its own broken where the row ends.
fn break_line(chars: &[(char, Style)], room: usize) -> Vec<(usize, usize)> {
    if chars.is_empty() {
        return vec![(0, 0)];
    }
    let mut rows = Vec::new();
    let mut start = 0;
    let mut used = 0;
    let mut index = 0;
    // Where the row could end, and where the text after that break picks up.
    let mut cut: Option<(usize, usize)> = None;
    while index < chars.len() {
        let (ch, _) = chars[index];
        let width = char_width(ch);
        if ch.is_whitespace() && used > 0 {
            // A run of spaces is a break that costs nothing.
            let mut end = index;
            while matches!(chars.get(end), Some((ch, _)) if ch.is_whitespace()) {
                end += 1;
            }
            cut = Some((index, end));
        }
        if used + width > room && used > 0 {
            match cut.filter(|(at, _)| *at > start) {
                Some((at, next)) => {
                    rows.push((start, at));
                    start = next;
                    index = next;
                }
                // One long word, broken where the row runs out.
                None => {
                    rows.push((start, index));
                    start = index;
                }
            }
            used = 0;
            cut = None;
            continue;
        }
        used += width;
        index += 1;
    }
    rows.push((start, chars.len()));
    rows
}
#[cfg(test)]
mod wrap_tests {
    use super::*;

    fn drawn(wrapped: &Wrapped) -> Vec<String> {
        wrapped
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn a_message_too_wide_for_the_window_keeps_its_mark_all_the_way_down() {
        let text = render_user("one two three four five six", "you");
        let wrapped = wrap(&text, 12);
        assert_eq!(
            drawn(&wrapped),
            vec![
                "\u{258c} you",
                "\u{258c} one two",
                "\u{258c} three four",
                "\u{258c} five six",
                "",
            ]
        );
    }

    #[test]
    fn the_mark_a_row_carries_is_not_part_of_what_it_says() {
        let text = render_user("one two three", "you");
        let wrapped = wrap(&text, 12);
        assert_eq!(wrapped.rows[1].indent, 2);
        assert_eq!(wrapped.text(1), "one two");
        assert_eq!(wrapped.text(2), "three");
        // Every row of the message comes from the one line it was written as.
        assert_eq!(wrapped.rows[1].line, wrapped.rows[2].line);
    }

    #[test]
    fn a_word_wider_than_the_window_is_broken_where_it_runs_out() {
        let text = Text::from("aaaaaaaaaa bb");
        let wrapped = wrap(&text, 4);
        assert_eq!(drawn(&wrapped), vec!["aaaa", "aaaa", "aa", "bb"]);
    }

    #[test]
    fn an_indented_line_stays_under_itself() {
        let text = Text::from("    alpha beta gamma");
        let wrapped = wrap(&text, 12);
        assert_eq!(drawn(&wrapped), vec!["    alpha", "    beta", "    gamma"]);
        assert_eq!(wrapped.rows[1].indent, 4);
        assert_eq!(wrapped.text(1), "beta");
    }

    #[test]
    fn a_blank_line_is_a_row_of_its_own() {
        let text = Text::from(vec![Line::from("a"), Line::default(), Line::from("b")]);
        let wrapped = wrap(&text, 8);
        assert_eq!(drawn(&wrapped), vec!["a", "", "b"]);
        assert_eq!(wrapped.starts, vec![0, 1, 2, 3]);
    }

    #[test]
    fn a_wide_character_takes_the_room_it_draws_in() {
        let text = Text::from("\u{4f60}\u{597d} ab");
        let wrapped = wrap(&text, 4);
        assert_eq!(drawn(&wrapped), vec!["\u{4f60}\u{597d}", "ab"]);
    }

    #[test]
    fn a_line_keeps_the_colours_it_was_written_in() {
        let text = render_user("one two three", "you");
        let wrapped = wrap(&text, 12);
        let row = &wrapped.lines[1];
        assert_eq!(row.spans[0].content.as_ref(), USER_MARK);
        assert_eq!(row.spans[0].style.fg, Some(Color::Green));
        assert_eq!(row.spans[1].style.fg, None);
    }

    #[test]
    fn a_column_and_the_character_drawn_there_find_each_other() {
        let text = render_user("one two three", "you");
        let wrapped = wrap(&text, 12);
        // The row reads "\u{258c} one two": the text starts two columns in.
        assert_eq!(wrapped.column(1, 0), 2);
        assert_eq!(wrapped.column(1, 4), 6);
        assert_eq!(wrapped.index(1, 6), 4);
        // A column in the mark is the first character, one past the end the last.
        assert_eq!(wrapped.index(1, 0), 0);
        assert_eq!(wrapped.index(1, 40), wrapped.len(1));
    }

    #[test]
    fn a_wide_character_is_one_character_over_two_columns() {
        let text = Text::from("\u{4f60}\u{597d}ab");
        let wrapped = wrap(&text, 20);
        assert_eq!(wrapped.column(0, 2), 4);
        assert_eq!(wrapped.index(0, 1), 0);
        assert_eq!(wrapped.index(0, 2), 1);
        assert_eq!(wrapped.byte(0, 2), 6);
    }

    #[test]
    fn what_a_row_says_is_a_piece_of_the_line_it_came_from() {
        let text = render_user("one two three four", "you");
        let wrapped = wrap(&text, 12);
        let line = wrapped.rows[1].line;
        let from = wrapped.byte(1, 0);
        let to = wrapped.byte(2, wrapped.len(2));
        // Taken together the rows give the message back, spaces and all.
        assert_eq!(&wrapped.texts[line][from..to], "one two three four");
    }

    #[test]
    fn a_line_that_fits_is_left_alone() {
        let text = Text::from(vec![Line::from("short"), Line::from("also short")]);
        let wrapped = wrap(&text, 40);
        assert_eq!(drawn(&wrapped), vec!["short", "also short"]);
        assert_eq!(wrapped.starts, vec![0, 1, 2]);
    }
}

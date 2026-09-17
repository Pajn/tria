//! Rendering. Layout: optional thread sidebar, header, chat, approval panel,
//! composer, status line. Overlays: picker and help.

use std::collections::HashSet;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Widget, Wrap},
};

use crate::{
    app::{App, Focus, Mode, PickerKind, Scroll, Section, SidebarRow, approval_options},
    model::ThreadStatus,
    session::Status,
    subagent,
    timeline::{self, Block as ChatBlock, BlockKey},
};

const SIDEBAR_WIDTH: u16 = 34;
const MIN_WIDTH_FOR_SIDEBAR: u16 = 90;
const COMPOSER_MAX_ROWS: u16 = 8;

/// Cached rendered chat blocks with their wrapped heights.
#[derive(Default)]
pub struct ChatCache {
    key: Option<(String, u64, u16, u64, bool, usize)>,
    blocks: Vec<CachedBlock>,
    total: usize,
    /// Every content line as displayed, filled on demand for search.
    lines: Option<Vec<String>>,
}

pub struct CachedBlock {
    block: ChatBlock,
    /// Rendered height after wrapping.
    height: usize,
    /// Toggle regions in wrapped content lines relative to the block start.
    rows: Vec<timeline::Region>,
    exports: Vec<(String, String)>,
}

thread_local! {
    static CACHE: std::cell::RefCell<ChatCache> = std::cell::RefCell::new(ChatCache::default());
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let show_sidebar = app.sidebar_visible && area.width >= MIN_WIDTH_FOR_SIDEBAR;
    let (sidebar_area, main_area) = if show_sidebar {
        let [s, m] = Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Fill(1)])
            .areas(area);
        (Some(s), m)
    } else {
        (None, area)
    };

    if let Some(sidebar_area) = sidebar_area {
        draw_sidebar(frame, app, sidebar_area);
    } else {
        app.sidebar_inner = None;
    }

    let pending = app
        .thread
        .as_ref()
        .map(|t| t.pending_approvals())
        .unwrap_or_default();
    let user_input = app.thread.as_ref().and_then(|t| t.pending_user_input());
    let approval_rows = if pending.is_empty() {
        0
    } else {
        3 + pending[0]
            .detail
            .as_ref()
            .map(|d| d.lines().count().min(6) as u16)
            .unwrap_or(0)
    };
    let question_rows = user_input
        .as_ref()
        .map(|q| question_panel_rows(app, q, main_area.width))
        .unwrap_or(0);
    let composer_rows = app
        .composer
        .height(main_area.width.saturating_sub(4), COMPOSER_MAX_ROWS)
        + 2;

    let [header, chat, approvals, questions, composer, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(approval_rows),
        Constraint::Length(question_rows),
        Constraint::Length(composer_rows),
        Constraint::Length(1),
    ])
    .areas(main_area);

    draw_header(frame, app, header);
    draw_chat(frame, app, chat);
    if let Some(first) = pending.first() {
        draw_approval(frame, first, pending.len(), approvals);
    }
    if let Some(question) = &user_input {
        draw_question(frame, app, question, questions);
    }
    draw_composer(frame, app, composer);
    draw_status(frame, app, status);
    let chat_inner = app.chat_area;
    apply_links(frame, app, chat_inner);
    apply_chat_cursor(frame, app, chat_inner);
    apply_search_highlights(frame, app, chat_inner);
    apply_selection(frame, app, chat_inner);

    match app.mode {
        Mode::Picker => draw_picker(frame, app, area),
        Mode::Help => draw_help(frame, app, area),
        Mode::Tasks => draw_tasks(frame, app, area),
        Mode::Agents => draw_agents(frame, app, area),
        Mode::Terminals => draw_terminals(frame, app, area),
        Mode::TerminalPane => draw_terminal_pane(frame, app, area),
        _ => {}
    }
}

/// Highlight the chat line cursor and the linewise visual range when the chat has focus.
fn apply_chat_cursor(frame: &mut Frame, app: &App, chat: Rect) {
    if app.focus != Focus::Chat || app.thread.is_none() || chat.height == 0 {
        return;
    }
    let offset = app.chat_offset();
    let buffer = frame.buffer_mut();
    let paint = |buffer: &mut ratatui::buffer::Buffer, line: usize, style: Style| {
        if line < offset || line >= offset + chat.height as usize {
            return;
        }
        let y = chat.y + (line - offset) as u16;
        for x in chat.x..chat.x + chat.width {
            if let Some(cell) = buffer.cell_mut(Position::new(x, y)) {
                cell.set_style(style);
            }
        }
    };
    if let Some(anchor) = app.chat_visual {
        let (start, end) = (anchor.min(app.chat_cursor), anchor.max(app.chat_cursor));
        for line in start..=end {
            paint(
                buffer,
                line,
                Style::default().bg(Color::Blue).fg(Color::White),
            );
        }
    }
    paint(
        buffer,
        app.chat_cursor,
        Style::default().add_modifier(Modifier::REVERSED),
    );
}

/// Plain text of a block or tool row by export key.
pub fn chat_export(key: &str) -> Option<String> {
    CACHE.with(|cache| {
        cache
            .borrow()
            .blocks
            .iter()
            .flat_map(|b| b.exports.iter())
            .find(|(k, _)| k == key)
            .map(|(_, text)| text.clone())
    })
}

/// Plain text of the whole conversation as currently loaded.
pub fn chat_export_all() -> String {
    CACHE.with(|cache| {
        cache
            .borrow()
            .blocks
            .iter()
            .filter_map(|b| b.exports.first().map(|(_, text)| text.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// Every content line of the chat as displayed, rendered off screen once per rebuild.
pub fn chat_lines() -> Vec<String> {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.lines.is_none() {
            let Some(width) = cache.key.as_ref().map(|k| k.2) else {
                return Vec::new();
            };
            let mut lines = Vec::with_capacity(cache.total);
            for cached in &cache.blocks {
                let area = Rect::new(0, 0, width, cached.height.min(u16::MAX as usize) as u16);
                let mut buffer = ratatui::buffer::Buffer::empty(area);
                Paragraph::new(cached.block.text.clone())
                    .wrap(Wrap { trim: false })
                    .render(area, &mut buffer);
                for row in 0..area.height {
                    let mut text = String::new();
                    for x in 0..width {
                        if let Some(cell) = buffer.cell(Position::new(x, row)) {
                            text.push_str(cell.symbol());
                        }
                    }
                    lines.push(text.trim_end().to_string());
                }
            }
            cache.lines = Some(lines);
        }
        cache.lines.clone().unwrap_or_default()
    })
}

/// Paint search matches on the visible chat rows, reading the drawn cells so highlights land
/// on the right columns regardless of wrapping or wide characters.
/// Underline the links on screen and record where they are, so a click can open one.
/// This reads the drawn cells rather than the source text, so it covers everything the
/// chat shows without each renderer having to care.
fn apply_links(frame: &mut Frame, app: &mut App, chat: Rect) {
    app.links.clear();
    if app.thread.is_none() || chat.height == 0 {
        return;
    }
    let buffer = frame.buffer_mut();
    // Read the visible rows as text, keeping the column each byte came from.
    let mut rows: Vec<(String, Vec<(usize, u16)>)> = Vec::new();
    for y in chat.y..chat.y + chat.height {
        let mut text = String::new();
        let mut columns: Vec<(usize, u16)> = Vec::new();
        for x in chat.x..chat.x + chat.width {
            if let Some(cell) = buffer.cell(Position::new(x, y)) {
                let symbol = cell.symbol();
                if !symbol.is_empty() {
                    columns.push((text.len(), x));
                }
                text.push_str(symbol);
            }
        }
        rows.push((text, columns));
    }

    let mut links: Vec<crate::app::Link> = Vec::new();
    for index in 0..rows.len() {
        for (from, to) in crate::app::link_ranges(&rows[index].0) {
            // A link that runs to the end of its row carries on below, one row at a time.
            let mut url = rows[index].0[from..to].to_string();
            let mut spans = vec![(index, from, to)];
            let mut row = index;
            let mut end = to;
            while end == rows[row].0.trim_end().len() && row + 1 < rows.len() {
                let next = &rows[row + 1].0;
                let run = next.find(char::is_whitespace).unwrap_or(next.len());
                if run == 0 {
                    break;
                }
                url.push_str(&next[..run]);
                spans.push((row + 1, 0, run));
                row += 1;
                end = run;
            }
            for (row, from, to) in spans {
                let y = chat.y + row as u16;
                let mut first = None;
                let mut last = None;
                for &(byte, x) in &rows[row].1 {
                    if byte < from || byte >= to {
                        continue;
                    }
                    first.get_or_insert(x);
                    last = Some(x);
                    if let Some(cell) = buffer.cell_mut(Position::new(x, y)) {
                        let style = cell.style().add_modifier(Modifier::UNDERLINED);
                        cell.set_style(style);
                    }
                }
                if let (Some(first), Some(last)) = (first, last) {
                    links.push(crate::app::Link {
                        row: y,
                        start: first,
                        end: last + 1,
                        url: url.clone(),
                    });
                }
            }
        }
    }
    app.links = links;
}

fn apply_search_highlights(frame: &mut Frame, app: &App, chat: Rect) {
    let query = match (&app.search_input, &app.search) {
        (Some(input), _) if app.mode == Mode::Search => input.query.as_str(),
        (_, Some(search)) if app.focus == Focus::Chat => search.query.as_str(),
        _ => return,
    };
    if query.is_empty() || app.thread.is_none() {
        return;
    }
    let buffer = frame.buffer_mut();
    for y in chat.y..chat.y + chat.height {
        let mut text = String::new();
        let mut starts: Vec<(usize, u16)> = Vec::new();
        for x in chat.x..chat.x + chat.width {
            if let Some(cell) = buffer.cell(Position::new(x, y)) {
                let symbol = cell.symbol();
                if !symbol.is_empty() {
                    starts.push((text.len(), x));
                }
                text.push_str(symbol);
            }
        }
        for (from, to) in crate::app::match_ranges(&text, query) {
            for &(byte, x) in &starts {
                if byte >= from
                    && byte < to
                    && let Some(cell) = buffer.cell_mut(Position::new(x, y))
                {
                    cell.set_style(Style::default().bg(Color::Yellow).fg(Color::Black));
                }
            }
        }
    }
}

/// Text of the chat's content lines `start..=end`, as displayed after wrapping. Renders the
/// covering blocks off screen so lines outside the viewport are available too.
pub fn chat_text(start: usize, end: usize) -> Option<String> {
    CACHE.with(|cache| {
        let cache = cache.borrow();
        let width = cache.key.as_ref()?.2;
        let mut out: Vec<String> = Vec::new();
        let mut y = 0usize;
        for cached in &cache.blocks {
            let block_start = y;
            let block_end = y + cached.height;
            y = block_end;
            if block_end <= start || block_start > end {
                continue;
            }
            let area = Rect::new(0, 0, width, cached.height.min(u16::MAX as usize) as u16);
            let mut buffer = ratatui::buffer::Buffer::empty(area);
            Paragraph::new(cached.block.text.clone())
                .wrap(Wrap { trim: false })
                .render(area, &mut buffer);
            for line in start.max(block_start)..=end.min(block_end - 1) {
                let row = (line - block_start) as u16;
                let mut text = String::new();
                for x in 0..width {
                    if let Some(cell) = buffer.cell(Position::new(x, row)) {
                        text.push_str(cell.symbol());
                    }
                }
                out.push(text.trim_end().to_string());
            }
        }
        if out.is_empty() {
            None
        } else {
            Some(out.join("\n"))
        }
    })
}

/// Highlight the mouse selection over the drawn chat cells and, when a drag has just ended,
/// collect the selected text so the event loop can copy it.
fn apply_selection(frame: &mut Frame, app: &mut App, chat: Rect) {
    let Some(selection) = app.selection else {
        return;
    };
    let (start, end) = selection.ordered();
    let want_text = app.clipboard_pending.is_some();
    let buffer = frame.buffer_mut();
    let mut text = String::new();
    for y in start.y..=end.y {
        if y < chat.y || y >= chat.y + chat.height {
            continue;
        }
        let from = if y == start.y { start.x } else { chat.x };
        let to = if y == end.y {
            end.x
        } else {
            chat.x + chat.width.saturating_sub(1)
        };
        let mut row = String::new();
        for x in from.max(chat.x)..=to.min(chat.x + chat.width.saturating_sub(1)) {
            if let Some(cell) = buffer.cell_mut(Position::new(x, y)) {
                if want_text {
                    // Continuation cells of wide characters carry an empty symbol.
                    row.push_str(cell.symbol());
                }
                cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
        if want_text {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(row.trim_end());
        }
    }
    if want_text {
        app.clipboard_pending = Some(text);
    }
}

pub fn status_style(status: ThreadStatus) -> Style {
    match status {
        ThreadStatus::Approval | ThreadStatus::Question => Style::default().fg(Color::Yellow),
        ThreadStatus::Working => Style::default().fg(Color::Cyan),
        ThreadStatus::Monitoring => Style::default().fg(Color::Blue),
        ThreadStatus::Failed => Style::default().fg(Color::Red),
        ThreadStatus::PlanReady => Style::default().fg(Color::Magenta),
        ThreadStatus::Done | ThreadStatus::Idle => Style::default().fg(Color::DarkGray),
    }
}

fn draw_sidebar(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Sidebar;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(border_style)
        .title(Line::from(vec![Span::styled(
            " threads ",
            Style::default().fg(Color::DarkGray),
        )]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.sidebar_inner = Some(inner);

    let rows = app.sidebar_rows();
    let width = inner.width as usize;
    let dim = Style::default().fg(Color::DarkGray);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match row {
            SidebarRow::Header {
                section,
                count,
                collapsed,
            } => {
                let arrow = match section {
                    Section::Snoozed | Section::Settled => {
                        if *collapsed {
                            "▸ "
                        } else {
                            "▾ "
                        }
                    }
                    _ => "  ",
                };
                let label = format!("{arrow}{} ({count})", section.label());
                let hint = match section {
                    Section::Settled if *collapsed => "  S",
                    _ => "",
                };
                ListItem::new(Line::from(vec![
                    Span::styled(label, dim.add_modifier(Modifier::BOLD)),
                    Span::styled(hint, dim.add_modifier(Modifier::DIM)),
                ]))
            }
            SidebarRow::Thread { id, parked } => {
                let Some(t) = app.shell.threads.get(id) else {
                    return ListItem::new(Line::from(""));
                };
                let status = t.status();
                let (glyph, glyph_style) = match status {
                    ThreadStatus::Working => (app.spinner_frame(), status_style(status)),
                    ThreadStatus::Approval | ThreadStatus::Question => ("!", status_style(status)),
                    ThreadStatus::Failed => ("✗", status_style(status)),
                    ThreadStatus::Monitoring => ("◔", status_style(status)),
                    ThreadStatus::PlanReady => ("▤", status_style(status)),
                    ThreadStatus::Done | ThreadStatus::Idle => ("·", dim),
                };
                let is_current = app.current_thread_id.as_ref() == Some(id);
                // The glyph carries the status; the right column always names the project.
                let right = app.shell.project_title(&t.project_id).to_string();
                let right_style = dim;
                let right_width = right.chars().count().min(12);
                let title_width = width.saturating_sub(right_width + 4);
                let title = fit(&t.title, title_width);
                let mut title_style = if *parked { dim } else { Style::default() };
                if is_current {
                    title_style = title_style.add_modifier(Modifier::BOLD);
                }
                let padding = " ".repeat(title_width.saturating_sub(title.chars().count()) + 1);
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!(" {glyph} "),
                        if *parked { dim } else { glyph_style },
                    ),
                    Span::styled(title, title_style),
                    Span::raw(padding),
                    Span::styled(fit(&right, right_width), right_style),
                ]))
            }
        })
        .collect();

    // Selection is drawn by hand so the wheel can scroll the list without the
    // selected row dragging the viewport back.
    let selected = if rows.is_empty() {
        None
    } else if focused {
        Some(app.sidebar_selected.min(rows.len() - 1))
    } else {
        rows.iter().position(
            |row| matches!(row, SidebarRow::Thread { id, .. } if Some(id) == app.current_thread_id.as_ref()),
        )
    };
    let highlight = if focused {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::DIM)
    };
    let items: Vec<ListItem> = items
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            if Some(i) == selected {
                item.style(highlight)
            } else {
                item
            }
        })
        .collect();

    let height = inner.height as usize;
    let max_offset = rows.len().saturating_sub(height);
    app.sidebar_offset = app.sidebar_offset.min(max_offset);
    if app.sidebar_reveal {
        app.sidebar_reveal = false;
        if let Some(sel) = selected {
            if sel < app.sidebar_offset {
                app.sidebar_offset = sel;
            } else if height > 0 && sel >= app.sidebar_offset + height {
                app.sidebar_offset = sel + 1 - height;
            }
        }
    }
    let mut state = ListState::default().with_offset(app.sidebar_offset);
    let list = List::new(items);
    frame.render_stateful_widget(list, inner, &mut state);
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans: Vec<Span> = Vec::new();
    // While a transcript is open the header names what is being read, since the chat
    // below it is no longer the thread's own conversation.
    if let Some(transcript) = &app.transcript {
        spans.push(Span::styled("⤷ ", Style::default().fg(Color::Magenta)));
        spans.push(Span::styled(
            transcript.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!("  {}", transcript.subtitle),
            Style::default().fg(Color::DarkGray),
        ));
        if transcript.truncated {
            spans.push(Span::styled(
                "  cut at 1 MB",
                Style::default().fg(Color::Yellow),
            ));
        }
        spans.push(Span::styled(
            "  q back",
            Style::default().fg(Color::DarkGray),
        ));
        let width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let pad = (area.width as usize).saturating_sub(width);
        spans.push(Span::raw(" ".repeat(pad)));
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Indexed(236))),
            area,
        );
        return;
    }
    if let Some(draft) = &app.draft {
        spans.push(Span::styled(
            "New thread",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!("  in {}", app.shell.project_title(&draft.project_id)),
            Style::default().fg(Color::DarkGray),
        ));
        let branch = app.vcs.as_ref().and_then(|vcs| vcs.ref_name.as_deref());
        let (label, style) = if draft.worktree {
            ("⌂ new worktree", Style::default().fg(Color::Cyan))
        } else {
            ("⌂ project checkout", Style::default().fg(Color::DarkGray))
        };
        spans.push(Span::styled(format!("  {label}"), style));
        if let Some(branch) = branch {
            spans.push(Span::styled(
                if draft.worktree {
                    format!(" off {branch}")
                } else {
                    format!(" on {branch}")
                },
                Style::default().fg(Color::DarkGray),
            ));
        }
        spans.push(Span::styled("  gw", Style::default().fg(Color::DarkGray)));
    } else if let Some(thread) = &app.thread {
        let shell = &thread.detail.shell;
        spans.push(Span::styled(
            shell.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!("  {}", app.shell.project_title(&shell.project_id)),
            Style::default().fg(Color::DarkGray),
        ));
        // The thread list carries a branch only for threads the server made one for;
        // for the rest it comes from watching the checkout.
        let branch = shell
            .branch
            .clone()
            .or_else(|| app.vcs.as_ref().and_then(|vcs| vcs.ref_name.clone()));
        if let Some(branch) = branch {
            spans.push(Span::styled(
                format!("   {branch}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if let Some(vcs) = &app.vcs
            && vcs.has_working_tree_changes
        {
            let tree = &vcs.working_tree;
            let counts = if tree.insertions + tree.deletions > 0 {
                format!(" +{} −{}", tree.insertions, tree.deletions)
            } else {
                String::new()
            };
            spans.push(Span::styled(
                format!(" ●{counts}"),
                Style::default().fg(Color::Yellow),
            ));
        }
        if let Some(remote) = &app.vcs_remote {
            if remote.ahead_count > 0 {
                spans.push(Span::styled(
                    format!("  ↑{}", remote.ahead_count),
                    Style::default().fg(Color::Green),
                ));
            }
            if remote.behind_count > 0 {
                spans.push(Span::styled(
                    format!("  ↓{}", remote.behind_count),
                    Style::default().fg(Color::Red),
                ));
            }
        }
        // Only worth naming when the thread has a checkout of its own.
        if let Some(worktree) = shell.worktree_path.as_deref().and_then(worktree_name) {
            spans.push(Span::styled(
                format!("  ⌂ {worktree}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if let Some(pr) = shell.primary_pull_request() {
            let state_style = match pr.state.as_deref() {
                Some("merged") => Style::default().fg(Color::Magenta),
                Some("closed") => Style::default().fg(Color::Red),
                _ if pr.is_draft => Style::default().fg(Color::DarkGray),
                _ => Style::default().fg(Color::Green),
            };
            let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
            let room = (area.width as usize).saturating_sub(used + 28).max(12);
            spans.push(Span::styled(format!("  #{}", pr.number), state_style));
            if let Some(title) = &pr.title {
                spans.push(Span::styled(
                    format!(" {}", fit(title, room)),
                    Style::default().fg(Color::Gray),
                ));
            }
            let mut tags: Vec<&str> = Vec::new();
            if pr.is_draft {
                tags.push("draft");
            }
            match pr.state.as_deref() {
                Some("merged") => tags.push("merged"),
                Some("closed") => tags.push("closed"),
                _ => {}
            }
            let checks = match pr.checks_state.as_deref() {
                Some("passing") => Some(("✓", Color::Green)),
                Some("failing") => Some(("✗", Color::Red)),
                Some("pending") => Some(("○", Color::Yellow)),
                _ => None,
            };
            if !tags.is_empty() {
                spans.push(Span::styled(
                    format!(" ({})", tags.join(", ")),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if let Some((glyph, color)) = checks {
                spans.push(Span::styled(
                    format!(" {glyph}"),
                    Style::default().fg(color),
                ));
            }
            spans.push(Span::styled(
                "  gx opens",
                Style::default().fg(Color::DarkGray).dim(),
            ));
        }
        if thread.has_more {
            spans.push(Span::styled(
                "  ↑ older turns available (:older)",
                Style::default().fg(Color::DarkGray).dim(),
            ));
        }
    } else if app.current_thread_id.is_some() {
        spans.push(Span::styled(
            "loading…",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        spans.push(Span::styled(
            "tria",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            "  / pick a thread · n new thread · ? help",
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_chat(frame: &mut Frame, app: &mut App, area: Rect) {
    let inner = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };
    app.chat_area = inner;
    // A subagent's transcript is read in place of the conversation, through the same
    // renderer: it is a conversation too, only one held out of sight.
    let open = app
        .transcript
        .as_ref()
        .map(|transcript| &transcript.state)
        .or(app.thread.as_ref());
    let Some(thread) = open else {
        let text = if app.draft.is_some() {
            "Type your first message below and press Enter."
        } else if app.current_thread_id.is_some() {
            "loading…"
        } else if !app.shell.loaded {
            match &app.status {
                Status::Connecting => "connecting…",
                Status::Reconnecting { .. } => "reconnecting…",
                Status::Failed(_) => "connection failed",
                Status::Connected => "loading threads…",
            }
        } else {
            "No thread open."
        };
        frame.render_widget(
            Paragraph::new(text).style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        app.chat_viewport = (inner.height as usize, 0);
        return;
    };

    let expanded_hash = hash_set(&app.expanded);
    let key = (
        thread.id().to_string(),
        thread.revision,
        inner.width,
        expanded_hash,
        app.expand_all,
        app.spinner % 8,
    );
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let needs_rebuild = match &cache.key {
            Some(existing) => {
                existing.0 != key.0
                    || existing.1 != key.1
                    || existing.2 != key.2
                    || existing.3 != key.3
                    || existing.4 != key.4
                    || (thread.is_running() && existing.5 != key.5)
            }
            None => true,
        };
        if needs_rebuild {
            let blocks = timeline::build(thread, &app.expanded, app.expand_all, inner.width);
            let mut total = 0usize;
            let blocks: Vec<CachedBlock> = blocks
                .into_iter()
                .map(|mut block| {
                    let exports = std::mem::take(&mut block.exports);
                    let height = Paragraph::new(block.text.clone())
                        .wrap(Wrap { trim: false })
                        .line_count(inner.width);
                    total += height;
                    // Per-line wrapped heights turn text-line row ranges into content lines.
                    let rows = if block.rows.is_empty() {
                        Vec::new()
                    } else {
                        let mut starts = Vec::with_capacity(block.text.lines.len() + 1);
                        let mut acc = 0usize;
                        for line in &block.text.lines {
                            starts.push(acc);
                            acc += Paragraph::new(Text::from(line.clone()))
                                .wrap(Wrap { trim: false })
                                .line_count(inner.width);
                        }
                        starts.push(acc);
                        block
                            .rows
                            .iter()
                            .map(|region| timeline::Region {
                                first: starts[region.first],
                                end: starts[region.end.min(starts.len() - 1)],
                                ..region.clone()
                            })
                            .collect()
                    };
                    CachedBlock {
                        block,
                        height,
                        rows,
                        exports,
                    }
                })
                .collect();
            cache.blocks = blocks;
            cache.total = total;
            cache.key = Some(key);
            cache.lines = None;
        }

        let height = inner.height as usize;
        let max_offset = cache.total.saturating_sub(height);
        let offset = match app.scroll {
            Scroll::Follow => max_offset,
            Scroll::Offset(o) => o.min(max_offset),
        };
        app.chat_viewport = (height, cache.total);
        app.work_ranges.clear();
        app.block_ranges.clear();
        app.message_starts.clear();
        if app.focus == Focus::Chat {
            app.chat_cursor = if app.scroll == Scroll::Follow {
                cache.total.saturating_sub(1)
            } else {
                app.chat_cursor.min(cache.total.saturating_sub(1))
            };
        }

        let mut y = 0usize;
        let mut cursor = inner.y;
        let bottom = inner.y + inner.height;
        for CachedBlock {
            block,
            height: block_height,
            rows,
            exports,
        } in &cache.blocks
        {
            let start = y;
            let end = y + block_height;
            y = end;
            if matches!(block.key, BlockKey::Message(_)) {
                app.message_starts.push(start);
            }
            if let Some((key, _)) = exports.first() {
                app.block_ranges.push((start, end, key.clone()));
            }
            if let BlockKey::Work(key) = &block.key {
                app.work_ranges.push(timeline::Region {
                    first: start,
                    end,
                    key: key.clone(),
                    foldable: true,
                });
                for region in rows {
                    app.work_ranges.push(timeline::Region {
                        first: start + region.first,
                        end: start + region.end,
                        ..region.clone()
                    });
                }
            }
            if end <= offset {
                continue;
            }
            if cursor >= bottom {
                break;
            }
            let skip = offset.saturating_sub(start);
            let visible = (block_height - skip).min((bottom - cursor) as usize);
            let rect = Rect {
                x: inner.x,
                y: cursor,
                width: inner.width,
                height: visible as u16,
            };
            frame.render_widget(
                Paragraph::new(block.text.clone())
                    .wrap(Wrap { trim: false })
                    .scroll((skip as u16, 0)),
                rect,
            );
            cursor += visible as u16;
        }

        if cache.total > height {
            draw_scrollbar(frame, area, offset, cache.total, height);
        }
    });
}

fn draw_scrollbar(frame: &mut Frame, area: Rect, offset: usize, total: usize, height: usize) {
    if area.width == 0 || height == 0 {
        return;
    }
    let x = area.x + area.width - 1;
    let thumb = ((height * height) / total).max(1);
    let top = (offset * height) / total;
    for row in 0..height {
        let glyph = if row >= top && row < top + thumb {
            "┃"
        } else {
            "│"
        };
        let style = if row >= top && row < top + thumb {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Black)
        };
        frame.render_widget(
            Paragraph::new(Span::styled(glyph, style)),
            Rect {
                x,
                y: area.y + row as u16,
                width: 1,
                height: 1,
            },
        );
    }
}

fn draw_approval(
    frame: &mut Frame,
    approval: &crate::state::PendingApproval,
    count: usize,
    area: Rect,
) {
    let style = Style::default().fg(Color::Yellow);
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(style)
        .title(Line::from(Span::styled(
            format!(
                " approval needed: {}{} ",
                approval.request_kind,
                if count > 1 {
                    format!(" (+{} more)", count - 1)
                } else {
                    String::new()
                }
            ),
            style.add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines: Vec<Line> = Vec::new();
    if let Some(detail) = &approval.detail {
        for line in detail.lines().take(6) {
            lines.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(Color::Gray),
            )));
        }
    }
    let options = approval_options(approval);
    let mut spans = vec![Span::raw("  ")];
    for (i, option) in options.iter().enumerate().take(9) {
        spans.push(Span::styled(
            format!("{}", i + 1),
            style.add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {}   ", option.label),
            Style::default(),
        ));
    }
    spans.push(Span::styled(
        "(normal mode)",
        Style::default().fg(Color::DarkGray),
    ));
    lines.push(Line::from(spans));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn question_panel_rows(app: &App, pending: &crate::state::PendingUserInput, width: u16) -> u16 {
    let index = app
        .question
        .as_ref()
        .filter(|d| d.request_id == pending.request_id)
        .map(|d| d.index)
        .unwrap_or(0);
    let Some(question) = pending.questions.get(index) else {
        return 0;
    };
    let text_rows = Paragraph::new(question.text.clone())
        .wrap(Wrap { trim: false })
        .line_count(width.saturating_sub(4)) as u16;
    // border + question text + options + custom line + hint line
    (1 + text_rows + question.options.len() as u16 + 2).min(16)
}

fn draw_question(
    frame: &mut Frame,
    app: &App,
    pending: &crate::state::PendingUserInput,
    area: Rect,
) {
    let active = matches!(app.mode, Mode::Question | Mode::QuestionCustom);
    let draft = app
        .question
        .as_ref()
        .filter(|d| d.request_id == pending.request_id);
    let index = draft.map(|d| d.index).unwrap_or(0);
    let Some(question) = pending.questions.get(index) else {
        return;
    };
    let accent = if active {
        Style::default().fg(Color::Magenta)
    } else {
        Style::default().fg(Color::Yellow)
    };

    let mut title = format!(" {} ", question.header);
    if pending.questions.len() > 1 {
        title.push_str(&format!("· {}/{} ", index + 1, pending.questions.len()));
    }
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(accent)
        .title(Line::from(Span::styled(
            title,
            accent.add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("  {}", question.text),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    let selected = draft
        .map(|d| d.current_answer().selected.clone())
        .unwrap_or_default();
    let highlight = draft.map(|d| d.highlight).unwrap_or(usize::MAX);
    let custom = draft
        .map(|d| d.current_answer().custom.clone())
        .unwrap_or_default();
    for (i, option) in question.options.iter().enumerate() {
        let is_selected = selected.contains(&i) && custom.trim().is_empty();
        let marker = match (question.multi_select, is_selected) {
            (true, true) => "[x]",
            (true, false) => "[ ]",
            (false, true) => "(•)",
            (false, false) => "( )",
        };
        let row_style = if active && highlight == i {
            Style::default().bg(Color::DarkGray)
        } else {
            Style::default()
        };
        let mut spans = vec![
            Span::styled(format!("  {} ", i + 1), accent.add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("{marker} "),
                if is_selected {
                    accent
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(option.label.clone(), row_style),
        ];
        if !option.description.is_empty() {
            spans.push(Span::styled(
                format!(
                    "  {}",
                    fit(
                        &option.description,
                        inner
                            .width
                            .saturating_sub(option.label.chars().count() as u16 + 12)
                            as usize
                    )
                ),
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(spans).style(row_style));
    }
    // The field is one row of the panel, so a long answer scrolls inside it rather than
    // wrapping and pushing the hint off the bottom. `  c > ` takes the first six columns.
    let field_width = inner.width.saturating_sub(6) as usize;
    let mut field_cursor = None;
    if question.allow_custom {
        if app.mode == Mode::QuestionCustom {
            let (visible, cursor) = app.custom_answer.line_window(field_width);
            // The rows above the field are however many the question text and the
            // options wrapped to, which is not one apiece.
            let row: u16 = lines
                .iter()
                .map(|line| {
                    Paragraph::new(line.clone())
                        .wrap(Wrap { trim: false })
                        .line_count(inner.width) as u16
                })
                .sum();
            field_cursor = Some((row, cursor as u16));
            lines.push(Line::from(vec![
                Span::styled("  c ", accent.add_modifier(Modifier::BOLD)),
                Span::styled("> ", accent),
                Span::raw(visible),
            ]));
        } else if !custom.trim().is_empty() {
            lines.push(Line::from(vec![
                Span::styled("  c ", accent.add_modifier(Modifier::BOLD)),
                Span::styled("(•) ", accent),
                Span::raw(fit(&custom, field_width)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("  c ", accent.add_modifier(Modifier::BOLD)),
                Span::styled("( ) ", Style::default().fg(Color::DarkGray)),
                Span::styled("type a custom answer", Style::default().fg(Color::DarkGray)),
            ]));
        }
    }
    let hint = if !active {
        "  a or Enter to answer".to_string()
    } else if app.mode == Mode::QuestionCustom {
        "  Enter confirm · Esc back".to_string()
    } else {
        let mut parts = vec![if question.multi_select {
            "digits/Space toggle · Enter next"
        } else {
            "digit picks · Enter next"
        }];
        if question.allow_custom {
            parts.push("c custom");
        }
        if index > 0 {
            parts.push("h back");
        }
        if pending.dismissible {
            parts.push("d dismiss");
        }
        parts.push("Esc leave");
        format!("  {}", parts.join(" · "))
    };
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);

    if let Some((row, cursor)) = field_cursor
        && row < inner.height
    {
        frame.set_cursor_position((
            (inner.x + 6 + cursor).min(inner.x + inner.width.saturating_sub(1)),
            inner.y + row,
        ));
    }
}

fn draw_composer(frame: &mut Frame, app: &App, area: Rect) {
    let insert = app.mode == Mode::Insert;
    let border_style = if insert {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let mut title_spans = vec![Span::styled(
        if insert { " insert " } else { " message " },
        border_style,
    )];
    if let Some(selection) = app
        .draft
        .as_ref()
        .map(|d| &d.model_selection)
        .or_else(|| app.thread.as_ref().map(|t| &t.detail.shell.model_selection))
    {
        let mut label = format!(" {} ", selection.model);
        for option in &selection.options {
            if let Some(v) = option.value.as_str() {
                label.push_str(&format!("{v} "));
            }
        }
        title_spans.push(Span::styled(label, Style::default().fg(Color::DarkGray)));
    }
    let block = Block::bordered()
        .border_style(border_style)
        .title(Line::from(title_spans));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text_area = Rect {
        x: inner.x + 1,
        y: inner.y,
        width: inner.width.saturating_sub(2),
        height: inner.height,
    };
    let placeholder = if insert {
        "type a message · Enter sends · Alt-Enter newline · Esc normal"
    } else {
        "i to write · d c y w b f t motions edit"
    };
    let (lines, cursor) = app.composer.render(text_area, placeholder);
    frame.render_widget(Paragraph::new(lines), text_area);
    if insert || (app.mode == Mode::Normal && app.focus == Focus::Composer) {
        frame.set_cursor_position(cursor);
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    if app.mode == Mode::Command {
        let line = Line::from(vec![
            Span::styled(":", Style::default().fg(Color::Yellow)),
            Span::raw(app.command_line.clone()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        frame.set_cursor_position((area.x + 1 + app.command_line.chars().count() as u16, area.y));
        return;
    }
    if app.mode == Mode::Search
        && let Some(input) = &app.search_input
    {
        let prompt = if input.backward { "?" } else { "/" };
        let line = Line::from(vec![
            Span::styled(prompt, Style::default().fg(Color::Yellow)),
            Span::raw(input.query.clone()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        frame.set_cursor_position((area.x + 1 + input.query.chars().count() as u16, area.y));
        return;
    }
    let (mode_label, mode_style) = match (app.mode, app.focus) {
        (Mode::Insert, _) => (
            " INSERT ",
            Style::default().bg(Color::Green).fg(Color::Black).bold(),
        ),
        (Mode::Picker, _) => (
            " PICK ",
            Style::default().bg(Color::Magenta).fg(Color::Black).bold(),
        ),
        (Mode::Help, _) => (
            " HELP ",
            Style::default().bg(Color::Blue).fg(Color::Black).bold(),
        ),
        (Mode::Agents, _) => (
            " AGENTS ",
            Style::default().bg(Color::Magenta).fg(Color::Black).bold(),
        ),
        (Mode::Question | Mode::QuestionCustom, _) => (
            " ANSWER ",
            Style::default().bg(Color::Magenta).fg(Color::Black).bold(),
        ),
        (_, Focus::Sidebar) => (
            " THREADS ",
            Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
        ),
        (_, Focus::Chat) if app.chat_visual.is_some() => (
            " VISUAL ",
            Style::default().bg(Color::Magenta).fg(Color::Black).bold(),
        ),
        (_, Focus::Chat) => (
            " CHAT ",
            Style::default().bg(Color::Yellow).fg(Color::Black).bold(),
        ),
        _ => (
            " NORMAL ",
            Style::default().bg(Color::Blue).fg(Color::Black).bold(),
        ),
    };
    let mut spans = vec![Span::styled(mode_label, mode_style), Span::raw(" ")];
    if app.mode == Mode::Normal && app.focus == Focus::Composer && app.composer.vim_pending() {
        spans.push(Span::styled(
            format!("{} ", app.composer.vim_pending_label()),
            Style::default().fg(Color::Yellow),
        ));
    }
    let (dot, dot_style, conn_label) = match &app.status {
        Status::Connected => ("●", Style::default().fg(Color::Green), String::new()),
        Status::Connecting => (
            "○",
            Style::default().fg(Color::Yellow),
            " connecting".into(),
        ),
        Status::Reconnecting { attempt, .. } => (
            "○",
            Style::default().fg(Color::Yellow),
            format!(" reconnecting #{attempt}"),
        ),
        Status::Failed(_) => ("●", Style::default().fg(Color::Red), " auth failed".into()),
    };
    spans.push(Span::styled(dot, dot_style));
    spans.push(Span::styled(
        conn_label,
        Style::default().fg(Color::DarkGray),
    ));

    if let Some(thread) = &app.thread {
        let shell = &thread.detail.shell;
        spans.push(Span::styled(
            format!("  {}", shell.model_selection.instance_id),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            format!("  {}", shell.runtime_mode),
            Style::default().fg(Color::DarkGray),
        ));
        if shell.interaction_mode == "plan" {
            spans.push(Span::styled("  plan", Style::default().fg(Color::Blue)));
        }
        if thread.is_running() {
            spans.push(Span::styled(
                format!("  {} running", app.spinner_frame()),
                Style::default().fg(Color::Cyan),
            ));
        } else if let Some(session) = &shell.session {
            if let Some(error) = &session.last_error {
                spans.push(Span::styled(
                    format!("  ✗ {}", fit(error, 60)),
                    Style::default().fg(Color::Red),
                ));
            } else {
                let status = shell.status();
                let mut label = format!("  {}", status.label());
                if shell.is_settled() {
                    label.push_str("  settled");
                }
                spans.push(Span::styled(label, status_style(status)));
            }
        }
        let tasks = thread.running_tasks().len();
        if tasks > 0 {
            spans.push(Span::styled(
                format!("  ⚙ {tasks} bg"),
                Style::default().fg(Color::Blue),
            ));
        }
        let agents = thread
            .subagents()
            .into_iter()
            .filter(|agent| agent.status.is_active())
            .count();
        if agents > 0 {
            spans.push(Span::styled(
                format!("  ⤷ {agents} agent{}", if agents == 1 { "" } else { "s" }),
                Style::default().fg(Color::Magenta),
            ));
        }
    } else if let Some(draft) = &app.draft {
        spans.push(Span::styled(
            format!(
                "  {}  {}",
                draft.model_selection.instance_id, draft.runtime_mode
            ),
            Style::default().fg(Color::DarkGray),
        ));
        if draft.interaction_mode == "plan" {
            spans.push(Span::styled("  plan", Style::default().fg(Color::Blue)));
        }
    }

    let right = match &app.toast {
        Some((message, _, is_error)) => Span::styled(
            format!(" {message} "),
            if *is_error {
                Style::default().fg(Color::Black).bg(Color::Red)
            } else {
                Style::default().fg(Color::Black).bg(Color::Yellow)
            },
        ),
        None => Span::styled(" ? help  : cmd ", Style::default().fg(Color::DarkGray)),
    };
    let left_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let right_width = right.content.chars().count();
    let pad = (area.width as usize).saturating_sub(left_width + right_width);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(right);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_picker(frame: &mut Frame, app: &App, area: Rect) {
    let Some(picker) = &app.picker else { return };
    let width = (area.width * 3 / 4).clamp(40, 100).min(area.width);
    let height = (area.height * 2 / 3).clamp(8, 30).min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 3,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let title = match picker.kind {
        PickerKind::Thread => " threads ",
        PickerKind::Model => " models ",
        PickerKind::Project => " new thread in project ",
        PickerKind::Effort => " effort ",
        PickerKind::PullRequest => " pull requests ",
    };
    let block = Block::bordered()
        .border_style(Style::default().fg(Color::Magenta))
        .title(title);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [query_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Magenta)),
            Span::raw(picker.query.clone()),
        ])),
        query_area,
    );
    frame.set_cursor_position((
        query_area.x + 2 + picker.query.chars().count() as u16,
        query_area.y,
    ));

    let items = picker.filtered();
    let label_width = (list_area.width as usize).saturating_sub(4);
    let list_items: Vec<ListItem> = items
        .iter()
        .map(|item| {
            let label_len = item.label.chars().count().min(label_width * 2 / 3);
            let label = fit(&item.label, label_len.max(1));
            let remaining = label_width.saturating_sub(label.chars().count() + 2);
            ListItem::new(Line::from(vec![
                Span::raw(label),
                Span::styled(
                    format!("  {}", fit(&item.detail, remaining)),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(picker.selected.min(items.len() - 1)));
    }
    let list = List::new(list_items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(list, list_area, &mut state);
}

/// The attached terminal, drawn from the parsed screen as a popup over the chat.
/// The border carries the session's title and the detach key; everything inside is
/// the shell's own output, so the pty is sized to the inner area.
fn draw_terminal_pane(frame: &mut Frame, app: &mut App, area: Rect) {
    let width = area.width.saturating_sub(6).max(20).min(area.width);
    let height = area.height.saturating_sub(4).max(10).min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let scrollback = app.pane.as_ref().map(|p| p.scrollback()).unwrap_or(0);
    let title = match app.pane.as_ref() {
        Some(pane) => match &pane.exited {
            Some(exited) => format!(" {} · {exited} ", fit(&pane.label, 40)),
            None => format!(" {} ", fit(&pane.label, 40)),
        },
        None => " terminal ".into(),
    };
    let hint = if scrollback > 0 {
        format!(" scrollback {scrollback} · Ctrl-\\ detaches ")
    } else {
        " Ctrl-\\ detaches ".into()
    };
    let block = Block::bordered()
        .border_style(Style::default().fg(Color::Cyan))
        .title(title)
        .title_bottom(Line::from(hint).right_aligned());
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    app.pane_area = inner;
    app.sync_pane_size(inner.width, inner.height);
    let Some(pane) = &app.pane else { return };
    // Until the command has taken over the shell, show a notice rather than the prompt
    // it is about to replace.
    if let Some(starting) = &pane.starting {
        let notice = Paragraph::new(Line::from(Span::styled(
            format!("starting {starting}…"),
            Style::default().fg(Color::DarkGray),
        )))
        .centered();
        let row = Rect {
            y: inner.y + inner.height / 2,
            height: 1.min(inner.height),
            ..inner
        };
        frame.render_widget(notice, row);
        return;
    }
    let screen = pane.screen();

    let buffer = frame.buffer_mut();
    for row in 0..inner.height {
        for col in 0..inner.width {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let Some(target) = buffer.cell_mut(Position::new(inner.x + col, inner.y + row)) else {
                continue;
            };
            let contents = cell.contents();
            target.set_symbol(if contents.is_empty() { " " } else { contents });
            let mut style = Style::default()
                .fg(vt_color(cell.fgcolor(), Color::Reset))
                .bg(vt_color(cell.bgcolor(), Color::Reset));
            if cell.bold() {
                style = style.add_modifier(Modifier::BOLD);
            }
            if cell.dim() {
                style = style.add_modifier(Modifier::DIM);
            }
            if cell.italic() {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if cell.underline() {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if cell.inverse() {
                style = style.add_modifier(Modifier::REVERSED);
            }
            target.set_style(style);
        }
    }

    if !screen.hide_cursor() && pane.scrollback() == 0 {
        let (row, col) = screen.cursor_position();
        if row < inner.height && col < inner.width {
            frame.set_cursor_position(Position::new(inner.x + col, inner.y + row));
        }
    }
}

/// The last component of a worktree path, which is what identifies it.
fn worktree_name(path: &str) -> Option<&str> {
    path.rsplit('/').find(|part| !part.is_empty())
}

/// vt100 colors, with the terminal's own default for `Default`.
fn vt_color(color: vt100::Color, fallback: Color) -> Color {
    match color {
        vt100::Color::Default => fallback,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// The thread's terminal sessions, with close and restart. These are real shells on the
/// server, shared with the desktop app.
fn draw_terminals(frame: &mut Frame, app: &App, area: Rect) {
    let terminals = app.thread_terminals();
    let width = 80.min(area.width);
    let inner_width = width.saturating_sub(4) as usize;
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if terminals.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no terminals for this thread",
            dim,
        )));
    }
    for (index, terminal) in terminals.iter().enumerate() {
        let selected = index == app.terminal_selected.min(terminals.len() - 1);
        let (glyph, glyph_style) = match terminal.status.as_str() {
            _ if terminal.has_running_subprocess => {
                (app.spinner_frame(), Style::default().fg(Color::Cyan))
            }
            "running" | "starting" => ("●", Style::default().fg(Color::Green)),
            "error" => ("✗", Style::default().fg(Color::Red)),
            _ => ("·", dim),
        };
        let mut title_style = Style::default();
        if selected {
            title_style = title_style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
        }
        lines.push(Line::from(vec![
            Span::styled(
                if selected { " ▸ " } else { "   " },
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(format!("{glyph} "), glyph_style),
            Span::styled(
                fit(&terminal.label, inner_width.saturating_sub(6)),
                title_style,
            ),
        ]));
        let mut detail = format!("      {}", terminal.status);
        if let Some(pid) = terminal.pid {
            detail.push_str(&format!(" · pid {pid}"));
        }
        if terminal.has_running_subprocess {
            detail.push_str(" · command running");
        }
        if let Some(code) = terminal.exit_code {
            detail.push_str(&format!(" · exit {code}"));
        }
        lines.push(Line::from(Span::styled(detail, dim)));
        lines.push(Line::from(Span::styled(
            format!(
                "      {}",
                fit(&terminal.cwd, inner_width.saturating_sub(6))
            ),
            dim,
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  j k select · Enter attach · c new · x close · r restart · Esc",
        dim,
    )));
    let text = Text::from(lines);
    let height = (text.lines.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(" terminals ({}) ", terminals.len()));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(text), inner);
}

/// The subagents the thread has run: what each one is, how it is going or how it went,
/// and what it cost. Informational, like the task panel — a subagent is stopped by
/// interrupting the turn that started it.
fn draw_agents(frame: &mut Frame, app: &App, area: Rect) {
    let agents = app.subagents();
    let now = crate::commands::now_iso();
    let width = 84.min(area.width);
    let inner_width = width.saturating_sub(4) as usize;
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if agents.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no subagents in this thread",
            dim,
        )));
    }
    let selected_index = app.agent_selected.min(agents.len().saturating_sub(1));
    for (index, agent) in agents.iter().enumerate() {
        let selected = index == selected_index;
        let (glyph, glyph_style) = match agent.status {
            subagent::Status::Pending | subagent::Status::Running | subagent::Status::Waiting => {
                (app.spinner_frame(), Style::default().fg(Color::Cyan))
            }
            subagent::Status::Idle => ("○", dim),
            subagent::Status::Completed => ("✓", Style::default().fg(Color::Green)),
            subagent::Status::Failed => ("✗", Style::default().fg(Color::Red)),
            subagent::Status::Cancelled | subagent::Status::Interrupted => {
                ("·", Style::default().fg(Color::Yellow))
            }
        };
        // A running subagent is timed from its start; a settled one kept the time it took.
        let elapsed = agent.started_at.as_deref().map(|started| {
            elapsed_label(
                started,
                agent
                    .completed_at
                    .as_deref()
                    .filter(|_| !agent.status.is_active())
                    .unwrap_or(&now),
            )
        });
        let elapsed = elapsed.unwrap_or_default();
        let role = agent
            .role
            .as_deref()
            .filter(|role| !role.eq_ignore_ascii_case(agent.title.trim()))
            .map(|role| format!(" [{role}]"))
            .unwrap_or_default();
        let mut title_style = Style::default();
        if selected {
            title_style = title_style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
        }
        lines.push(Line::from(vec![
            Span::styled(
                if selected { " ▸ " } else { "   " },
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(format!("{glyph} "), glyph_style),
            Span::styled(
                fit(
                    &agent.title,
                    inner_width.saturating_sub(elapsed.chars().count() + role.chars().count() + 8),
                ),
                title_style,
            ),
            Span::styled(role, dim),
            Span::styled(format!("  {elapsed}"), dim),
        ]));
        let activity = agent
            .activity()
            .unwrap_or_else(|| agent.status.label().to_string());
        lines.push(Line::from(Span::styled(
            format!("      {}", fit(&activity, inner_width.saturating_sub(6))),
            if agent.status == subagent::Status::Failed {
                Style::default().fg(Color::Red)
            } else {
                dim
            },
        )));
        let mut facts: Vec<String> = Vec::new();
        if let Some(model) = agent.model_label() {
            facts.push(model);
        }
        facts.push(match &agent.usage {
            Some(usage) => format!("{} tok", tokens(usage.total_tokens)),
            None => "— tok".to_string(),
        });
        if let Some(tools) = agent.usage.as_ref().and_then(|usage| usage.tool_uses) {
            facts.push(format!("{tools} tools"));
        }
        if agent.activations > 1 {
            facts.push(format!("run {}", agent.activations));
        }
        if agent.output_file.is_none() {
            facts.push("no transcript".to_string());
        }
        if app.transcript_loading.as_deref() == Some(agent.id.as_str()) {
            facts.push("reading…".to_string());
        }
        lines.push(Line::from(Span::styled(
            format!(
                "      {}",
                fit(&facts.join(" · "), inner_width.saturating_sub(6))
            ),
            dim,
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  j k select · Enter read the transcript · y yank the report · Esc",
        dim,
    )));
    let text = Text::from(lines);
    let height = (text.lines.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let live = agents
        .iter()
        .filter(|agent| agent.status.is_active())
        .count();
    let title = if live > 0 {
        format!(" subagents ({} · {live} working) ", agents.len())
    } else {
        format!(" subagents ({}) ", agents.len())
    };
    let block = Block::bordered()
        .border_style(Style::default().fg(Color::Magenta))
        .title(title);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(text), inner);
}

/// Token counts as the desktop writes them: `840`, `73.4k`, `1.2M`.
fn tokens(total: u64) -> String {
    match total {
        0..=999 => total.to_string(),
        1_000..=999_999 => {
            let value = total as f64 / 1000.0;
            if value >= 100.0 {
                format!("{}k", value.round())
            } else {
                format!("{value:.1}k")
            }
        }
        _ => format!("{:.1}M", total as f64 / 1_000_000.0),
    }
}

/// The agent's unfinished background tasks. Informational: the protocol has no per-task
/// stop, so the panel points at the turn-level interrupt instead.
fn draw_tasks(frame: &mut Frame, app: &App, area: Rect) {
    let tasks = app.running_tasks();
    let now = crate::commands::now_iso();
    let mut lines: Vec<Line<'static>> = Vec::new();
    if tasks.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no background tasks running",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let width = 76.min(area.width) as usize;
    for task in &tasks {
        let elapsed = elapsed_label(&task.started_at, &now);
        let mut tags: Vec<&str> = Vec::new();
        if task.agent_kind == "background" {
            tags.push("background");
        }
        if task.backgrounded {
            tags.push("backgrounded");
        }
        if !task.task_type.is_empty() {
            tags.push(task.task_type.as_str());
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} ", app.spinner_frame()),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                fit(
                    &task.title,
                    width.saturating_sub(elapsed.chars().count() + 6),
                ),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {elapsed}"), Style::default().fg(Color::DarkGray)),
        ]));
        lines.push(Line::from(Span::styled(
            format!("   {}", tags.join(" · ")),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Ctrl-c interrupts the turn · no per-task stop exists · Esc to close",
        Style::default().fg(Color::DarkGray),
    )));
    let text = Text::from(lines);
    let width = 76.min(area.width);
    let height = (text.lines.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(" background tasks ({}) ", tasks.len()));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

/// Rough "how long ago" for two RFC 3339 timestamps, for example `3m` or `2h04`.
pub fn elapsed_label(since: &str, now: &str) -> String {
    let parse = |text: &str| {
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()
    };
    let (Some(start), Some(now)) = (parse(since), parse(now)) else {
        return String::new();
    };
    let seconds = (now - start).whole_seconds().max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h{:02}", seconds / 3600, (seconds % 3600) / 60)
    }
}

fn draw_help(frame: &mut Frame, app: &mut App, area: Rect) {
    let text = Text::from(vec![
        Line::from(Span::styled("Normal", Style::default().bold())),
        Line::from("  Tab / Shift-Tab         cycle focus: composer → chat → threads · Esc back"),
        Line::from("  J/K                     next / previous thread"),
        Line::from("  /                       fuzzy thread picker"),
        Line::from(""),
        Line::from("  chat (focused):"),
        Line::from("  j k  { }  gg G  Ctrl-d/u/f/b/e/y   line cursor / by message / scroll"),
        Line::from(
            "  za or Enter             fold or unfold the tool group or row under the cursor",
        ),
        Line::from("  V then y  ·  yy  ·  Ny  yank lines to the clipboard"),
        Line::from("  / ?  n N               search forward / backward, next / previous match"),
        Line::from(""),
        Line::from("  n                       new thread (pick project)"),
        Line::from("  m                       change model"),
        Line::from("  i / Enter               write a message"),
        Line::from("  za  zR  zM              toggle / expand all / collapse all tool groups"),
        Line::from("  1..9                    answer a pending approval (otherwise a count)"),
        Line::from(
            "  ga                      answer the agent's question (digits, Space, c custom, Enter)",
        ),
        Line::from("  gy                      yank last assistant message (OSC 52)"),
        Line::from("  gl                      lazygit in the thread's terminal pane"),
        Line::from("  g!                      a shell in the pane; exiting it closes the popup"),
        Line::from("  gT                      background tasks still running in this thread"),
        Line::from("  gA                      subagents this thread has run: Enter reads one's"),
        Line::from("                          transcript, y yanks its report"),
        Line::from("  gS                      terminals for this thread: Enter attaches,"),
        Line::from("                          c opens a new one, x closes, r restarts"),
        Line::from("  in the pane             every key goes to the shell · Ctrl-\\ detaches"),
        Line::from(
            "  ge                      composer: edit the draft · chat: view the block under the cursor",
        ),
        Line::from("  gE                      view the whole conversation in your editor"),
        Line::from("  Ctrl-e Ctrl-y Ctrl-d Ctrl-u  scroll the conversation by line / half page"),
        Line::from(""),
        Line::from("  composer (focused, Vim):"),
        Line::from("  h j k l w b e W B E 0 ^ $ gg G f F t T ; ,   motions, with counts"),
        Line::from(
            "  d c y + motion or iw aw i\" a( ...   operators and text objects; dd cc yy D C Y x X",
        ),
        Line::from(
            "  i a I A o O   insert · p P paste · r replace · ~ case · u / Ctrl-r undo / redo",
        ),
        Line::from("  s / S                   toggle sidebar / settled shelf"),
        Line::from("  gs                      settle the thread, or bring a settled one back"),
        Line::from("  gw                      new thread: fresh worktree or the project checkout"),
        Line::from("  gl                      lazygit in the thread's terminal pane"),
        Line::from("  g!                      a shell in the pane"),
        Line::from(
            "  gx                      open the link under the cursor, else the pull request",
        ),
        Line::from("  click a link            open it in the browser"),
        Line::from(
            "  gt                      switch to the tmux session for the thread's directory",
        ),
        Line::from("  mouse drag              select chat text; released, it is copied"),
        Line::from("  Ctrl-c                  interrupt the running turn"),
        Line::from(""),
        Line::from(Span::styled("Insert", Style::default().bold())),
        Line::from("  Enter send · Alt-Enter / Ctrl-j newline · Esc normal"),
        Line::from("  Up/Down or Ctrl-p/n     prompt history"),
        Line::from("  ← → Home End Ctrl-a/e  move · Alt-arrow or Alt-b/f by word"),
        Line::from("  Ctrl-w Ctrl-k Ctrl-u    kill word / to end / to start"),
        Line::from("  the same keys edit a custom answer under ga, where Enter confirms it"),
        Line::from(""),
        Line::from(Span::styled("Commands", Style::default().bold())),
        Line::from("  :new [project]  :model  :effort [level]  :mode plan|default"),
        Line::from("  :perm full-access|auto|auto-accept-edits|approval-required"),
        Line::from("  :rename <title>  :rename (regenerate)  :archive  :delete!"),
        Line::from(
            "  :pr  :tasks  :agents  :terminals  :tmux  :settle  :unsettle  :wake  :settled  :stop  :older  :answer  :dismiss  :sidebar  :q",
        ),
    ]);
    let width = 72.min(area.width);
    // Wrapped, so a line longer than the popup is folded rather than cut off its end.
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    let total = paragraph.line_count(width.saturating_sub(2));
    let height = (total as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    // There is more help than there is terminal on most screens, so it scrolls.
    let rows = popup.height.saturating_sub(2) as usize;
    app.help_viewport = (rows, total);
    let offset = app.help_offset.min(total.saturating_sub(rows));
    app.help_offset = offset;
    let more = total > rows;
    let block = Block::bordered()
        .border_style(Style::default().fg(Color::Blue))
        .title(" keys ")
        .title_bottom(Line::from(Span::styled(
            if more {
                format!(
                    " {}–{} of {total} · j k scroll · Esc ",
                    offset + 1,
                    (offset + rows).min(total)
                )
            } else {
                " Esc to close ".to_string()
            },
            Style::default().fg(Color::DarkGray),
        )));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(paragraph.scroll((offset as u16, 0)), inner);
    if more {
        // On the border column, so it never lands on top of the text.
        let track = Rect {
            x: inner.x + inner.width,
            y: inner.y,
            width: 1,
            height: inner.height,
        };
        draw_scrollbar(frame, track, offset, total, rows);
    }
}

fn fit(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        text.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn hash_set(set: &HashSet<String>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut keys: Vec<&String> = set.iter().collect();
    keys.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    keys.hash(&mut hasher);
    hasher.finish()
}

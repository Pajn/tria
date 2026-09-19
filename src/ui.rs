//! Rendering. Layout: optional thread sidebar, header, chat, approval panel,
//! composer, status line. Overlays: picker and help.

use std::collections::HashSet;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect, Size},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use ratatui_image::sliced::SignedPosition;
use unicode_width::UnicodeWidthStr;

use crate::{
    app::{App, Focus, Mode, PickerKind, Scroll, Section, SidebarRow, approval_options},
    config::SidebarLayout,
    model::ThreadStatus,
    picture,
    session::Status,
    subagent,
    timeline::{self, Block as ChatBlock, BlockKey},
};

const SIDEBAR_WIDTH: u16 = 34;
const MIN_WIDTH_FOR_SIDEBAR: u16 = 90;
/// Columns kept in front of a project's name for what it is drawn with. Two for the
/// picture or the emoji, one to stand it off the name.
const MARK: usize = 3;
/// Columns the two-line thread list keeps on its right for the project's icon: four for
/// the picture, which is two rows tall and so needs four columns to come out square, and
/// one to stand it off the text.
const ICON_COLUMN: usize = 5;
/// What a selected row is tinted with: the foreground at about a third, which is a mark
/// the eye finds without it being a block. Faint on purpose — half the list is written in
/// grey, and a fill dark enough to read black text on is a fill grey text disappears
/// into, so the tint is the same on every row whatever colour the row is written in.
const SELECTED: Color = Color::Indexed(238);
/// The largest a drawn icon is made, however much room it was given. Past this it is a
/// line drawing with more pixels than the lines have detail.
const MOST_ICON_PIXELS: u32 = 64;
const COMPOSER_MAX_ROWS: u16 = 8;

/// Cached rendered chat blocks with their wrapped heights.
#[derive(Default)]
pub struct ChatCache {
    key: Option<(String, u64, u16, u16, u64, u8, usize)>,
    blocks: Vec<CachedBlock>,
    total: usize,
    /// Every content line as displayed, filled on demand for search.
    lines: Option<Vec<String>>,
}

pub struct CachedBlock {
    block: ChatBlock,
    /// The block's text broken into the rows it is drawn as.
    wrapped: timeline::Wrapped,
    /// Toggle regions in wrapped content lines relative to the block start.
    rows: Vec<timeline::Region>,
    exports: Vec<(String, String)>,
    /// Images in wrapped content lines relative to the block start.
    images: Vec<timeline::Placed>,
}

impl CachedBlock {
    fn height(&self) -> usize {
        self.wrapped.lines.len()
    }
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

    // Trouble that outlasts a toast gets a row of its own under the header, and only
    // takes one when there is some.
    let trouble = app.trouble();
    let [header, banner, chat, approvals, questions, composer, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(u16::from(trouble.is_some())),
        Constraint::Fill(1),
        Constraint::Length(approval_rows),
        Constraint::Length(question_rows),
        Constraint::Length(composer_rows),
        Constraint::Length(1),
    ])
    .areas(main_area);

    draw_header(frame, app, header);
    if let Some(trouble) = trouble {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" ⚠ {trouble}"),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ))),
            banner,
        );
    }
    draw_chat(frame, app, chat);
    if let Some(first) = pending.first() {
        draw_approval(
            frame,
            first,
            pending.len(),
            app.digits_answer_approval(),
            approvals,
        );
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
        Mode::Usage => draw_usage(frame, app, area),
        Mode::Agents => draw_agents(frame, app, area),
        Mode::Terminals => draw_terminals(frame, app, area),
        Mode::Worktrees => draw_worktrees(frame, app, area),
        Mode::TerminalPane => draw_terminal_pane(frame, app, area),
        _ => {}
    }
}

/// Highlight the chat cursor and the visual selection when the chat has focus.
fn apply_chat_cursor(frame: &mut Frame, app: &App, chat: Rect) {
    if app.focus != Focus::Chat || app.thread.is_none() || chat.height == 0 {
        return;
    }
    let offset = app.chat_offset();
    let (cursor, column) = app.chat_spot();
    let row = |line: usize| {
        (line >= offset)
            .then(|| chat.y + (line - offset) as u16)
            .filter(|y| *y < chat.y + chat.height)
    };
    let buffer = frame.buffer_mut();
    let paint = |buffer: &mut ratatui::buffer::Buffer, line, from: u16, to: u16, style| {
        let Some(y) = row(line) else { return };
        for x in from.max(chat.x)..to.min(chat.x + chat.width) {
            if let Some(cell) = buffer.cell_mut(Position::new(x, y)) {
                cell.set_style(style);
            }
        }
    };
    // The line being read is tinted rather than filled, so the conversation keeps the
    // colours it is written in and the cursor is still easy to find on a wide screen.
    paint(
        buffer,
        cursor,
        chat.x,
        chat.x + chat.width,
        Style::default().bg(Color::Indexed(236)),
    );
    if let Some(anchor) = app.chat_visual {
        let style = Style::default().bg(Color::Blue).fg(Color::White);
        let head = (anchor.line, anchor.column.min(chat_len(anchor.line)));
        let spot = (cursor, column);
        let (first, last) = if head <= spot {
            (head, spot)
        } else {
            (spot, head)
        };
        for line in first.0..=last.0 {
            let (from, to) = if anchor.whole_lines {
                (0, chat.width)
            } else {
                let from = if line == first.0 {
                    chat_column(line, first.1)
                } else {
                    0
                };
                let to = if line == last.0 {
                    chat_column(line, last.1 + 1)
                } else {
                    chat_column(line, chat_len(line))
                };
                // An empty line still shows that it is in the selection.
                (from, to.max(from + 1))
            };
            paint(buffer, line, chat.x + from, chat.x + to, style);
        }
    }
    // The cursor is a block on the character under it, as the composer's is, drawn as
    // whichever way round that cell is not so it shows on the tinted line and inside a
    // selection alike.
    if let Some(y) = row(cursor) {
        let x = chat.x + chat_column(cursor, column);
        if x < chat.x + chat.width
            && let Some(cell) = buffer.cell_mut(Position::new(x, y))
        {
            let style = if cell.style().add_modifier.contains(Modifier::REVERSED) {
                Style::default().remove_modifier(Modifier::REVERSED)
            } else {
                Style::default().add_modifier(Modifier::REVERSED)
            };
            cell.set_style(style);
        }
    }
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

/// Every content line of the chat as displayed, marks and indents and all, kept from one
/// rebuild to the next for search to read.
pub fn chat_lines() -> Vec<String> {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.lines.is_none() {
            let mut lines = Vec::with_capacity(cache.total);
            for cached in &cache.blocks {
                lines.extend(cached.wrapped.lines.iter().map(|line| {
                    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                    text.trim_end().to_string()
                }));
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
        (Some(input), _) if app.mode == Mode::Search => input.query.text(),
        (_, Some(search)) if app.focus == Focus::Chat => search.query.clone(),
        _ => return,
    };
    let query = query.as_str();
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

/// The chat text from one point to another, each a content line and a character on it,
/// the end exclusive. What comes back is what was written rather than what was drawn: the
/// marks and indents the chat decorates its lines with are left out, and a line broken
/// over several rows comes back as the one line it was, spaces and all.
pub fn chat_span(start: (usize, usize), end: (usize, usize)) -> Option<String> {
    CACHE.with(|cache| {
        let cache = cache.borrow();
        // The pieces to take, each a byte range of one line of one block. Rows of the
        // same line join into one piece, which puts back the space a break swallowed.
        let mut pieces: Vec<(usize, usize, usize, usize)> = Vec::new();
        let mut y = 0usize;
        for (index, cached) in cache.blocks.iter().enumerate() {
            let block_start = y;
            y += cached.height();
            if y <= start.0 || block_start > end.0 {
                continue;
            }
            for (row, at) in cached.wrapped.rows.iter().enumerate() {
                let line = block_start + row;
                if line < start.0 || line > end.0 {
                    continue;
                }
                let from = if line == start.0 {
                    cached.wrapped.byte(row, start.1)
                } else {
                    at.start
                };
                let to = if line == end.0 {
                    cached.wrapped.byte(row, end.1)
                } else {
                    at.end
                };
                match pieces.last_mut() {
                    Some(last) if (last.0, last.1) == (index, at.line) => last.3 = to.max(last.3),
                    _ => pieces.push((index, at.line, from, to)),
                }
            }
        }
        if pieces.is_empty() {
            return None;
        }
        Some(
            pieces
                .iter()
                .map(|&(block, line, from, to)| {
                    &cache.blocks[block].wrapped.texts[line][from..to.max(from)]
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    })
}

/// The block a content line belongs to, and which of its rows the line is.
fn locate(cache: &ChatCache, line: usize) -> Option<(&CachedBlock, usize)> {
    let mut y = 0usize;
    for cached in &cache.blocks {
        if line < y + cached.height() {
            return Some((cached, line - y));
        }
        y += cached.height();
    }
    None
}

/// The character of a content line drawn at a column of the chat, for a click or a drag.
pub fn chat_index(line: usize, column: u16) -> usize {
    CACHE.with(|cache| {
        let cache = cache.borrow();
        locate(&cache, line).map_or(0, |(block, row)| block.wrapped.index(row, column))
    })
}

/// Where a character of a content line is drawn, as a column of the chat.
pub fn chat_column(line: usize, index: usize) -> u16 {
    CACHE.with(|cache| {
        let cache = cache.borrow();
        locate(&cache, line).map_or(0, |(block, row)| block.wrapped.column(row, index))
    })
}

/// What a content line says, without the decoration it is drawn with.
pub fn chat_row(line: usize) -> String {
    CACHE.with(|cache| {
        let cache = cache.borrow();
        locate(&cache, line).map_or(String::new(), |(block, row)| block.wrapped.text(row).into())
    })
}

/// How many characters a content line can be addressed by, its decoration not counted.
pub fn chat_len(line: usize) -> usize {
    CACHE.with(|cache| {
        let cache = cache.borrow();
        locate(&cache, line).map_or(0, |(block, row)| block.wrapped.len(row))
    })
}

/// Highlight the drag over the chat cells it covers.
fn apply_selection(frame: &mut Frame, app: &App, chat: Rect) {
    let Some(selection) = app.selection else {
        return;
    };
    let (start, end) = selection.ordered();
    let buffer = frame.buffer_mut();
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
        for x in from.max(chat.x)..=to.min(chat.x + chat.width.saturating_sub(1)) {
            if let Some(cell) = buffer.cell_mut(Position::new(x, y)) {
                cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
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
    let two_line = app.sidebar_layout == SidebarLayout::TwoLine;
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
    // Which list has the keys is the border's to say, so the mark is the same either
    // way: it is answering which row, not which pane.
    let highlight = Style::default().bg(SELECTED);
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
                // The settled pile is where worktrees collect, so it says how many are
                // still there: the count is the whole reminder that they need clearing.
                let label = match section {
                    Section::Settled => match app.settled_worktrees() {
                        0 => format!("{arrow}{} ({count})", section.label()),
                        held => format!("{arrow}{} ({count} · {held} ⌂)", section.label()),
                    },
                    _ => format!("{arrow}{} ({count})", section.label()),
                };
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
                    // Monitoring is the one status that outlasts the news it brings:
                    // the watcher sits there for hours, so it says in its own colour
                    // whether anything has happened since anybody looked.
                    ThreadStatus::Monitoring if app.unseen.contains(id) => {
                        ("◔", Style::default().fg(Color::Green))
                    }
                    ThreadStatus::Monitoring => ("◔", status_style(status)),
                    ThreadStatus::PlanReady => ("▤", status_style(status)),
                    // A thread that has spoken since anybody looked fills the dot in.
                    // The statuses above it are already asking to be looked at, and say
                    // so more precisely than this could.
                    ThreadStatus::Done | ThreadStatus::Idle if app.unseen.contains(id) => {
                        ("●", Style::default().fg(Color::Cyan))
                    }
                    ThreadStatus::Done | ThreadStatus::Idle => ("·", dim),
                };
                let is_current = app.current_thread_id.as_ref() == Some(id);
                // A worktree of its own, still on the disk. Marked only where the thread
                // is done with it, since that is when it is leavings rather than a
                // workplace.
                let holds_worktree = *parked && app.holds_worktree(t);
                let glyph_style = if *parked { dim } else { glyph_style };
                let mut title_style = if *parked { dim } else { Style::default() };
                if is_current {
                    title_style = title_style.add_modifier(Modifier::BOLD);
                }
                if two_line {
                    // The project is the icon drawn over the right of both lines, so the
                    // text stops short of it whether or not this project has one.
                    let title_width = width.saturating_sub(ICON_COLUMN + 4);
                    let title = fit(&t.title, title_width);
                    // What the thread is working in: its own branch, the checkout tria
                    // is watching when this is the thread whose checkout that is, and
                    // failing both the project it belongs to, which is the one thing
                    // about it that is always known.
                    let mut under = t.branch.clone().or_else(|| {
                        is_current
                            .then(|| app.vcs.as_ref().and_then(|vcs| vcs.ref_name.clone()))
                            .flatten()
                    });
                    if under.is_none() {
                        under = Some(app.shell.project_title(&t.project_id).to_string());
                    }
                    let mut under = fit(&under.unwrap_or_default(), title_width);
                    if holds_worktree {
                        under = fit(&format!("⌂ {under}"), title_width);
                    }
                    // The mark is the status column over both lines: a bar beside the
                    // thread, which says which row is selected without covering the two
                    // lines it is made of.
                    return ListItem::new(vec![
                        Line::from(vec![
                            Span::styled(format!(" {glyph} "), glyph_style),
                            Span::styled(title, title_style),
                        ]),
                        Line::from(Span::styled(format!("   {under}"), dim)),
                    ]);
                }
                // The glyph carries the status; the right column always names the project.
                let mut right = app.shell.project_title(&t.project_id).to_string();
                if holds_worktree {
                    right = format!("⌂ {right}");
                }
                let right_style = dim;
                let right_width = right.chars().count().min(14);
                let title_width = width.saturating_sub(right_width + 4);
                let title = fit(&t.title, title_width);
                let padding = " ".repeat(title_width.saturating_sub(title.chars().count()) + 1);
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {glyph} "), glyph_style),
                    Span::styled(title, title_style),
                    Span::raw(padding),
                    Span::styled(fit(&right, right_width), right_style),
                ]))
            }
        })
        .collect();

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
    let max_offset = app.sidebar_max_offset(&rows, height);
    app.sidebar_offset = app.sidebar_offset.min(max_offset);
    if app.sidebar_reveal {
        app.sidebar_reveal = false;
        if let Some(sel) = selected {
            app.sidebar_offset = app.sidebar_offset_showing(&rows, sel, height);
        }
    }
    let mut state = ListState::default().with_offset(app.sidebar_offset);
    let list = List::new(items);
    frame.render_stateful_widget(list, inner, &mut state);
    if two_line {
        draw_sidebar_icons(frame, app, inner, &rows);
    }
}

/// The project's icon on the right of a thread's two lines, drawn over the room the
/// rows left for it. A picture goes on after the list, the way the project picker draws
/// one; an emoji is a character and sits on the title's line.
/// Draw what a project is known by over the room a row kept for it, and say whether
/// anything went there. In the order the desktop app draws them: the icon somebody chose
/// for it from the drawing set, failing that the one its checkout carries, and failing
/// both a guess at what the project is from its name. All three are pictures and so go
/// on after the list rather than into it, over the room the text left.
///
/// An emoji is not here: it is a character, and a character belongs in the line.
fn draw_project_picture(frame: &mut Frame, app: &App, project: &str, area: Rect) -> bool {
    if area.width == 0 || area.height == 0 {
        return false;
    }
    if let Some((name, colour)) = app.project_lucide(project)
        && draw_drawn_icon(frame, name, colour, area)
    {
        return true;
    }
    if let Some(bytes) = app.favicon(project) {
        let key = format!("favicon:{project}:{}", area.height);
        if picture::place(&key, picture::Source::Bytes(bytes), area.as_size()).is_some() {
            picture::draw(frame, &key, area, SignedPosition::from((0, 0)));
            return true;
        }
    }
    // Nothing chosen and no icon in the checkout: the name is all there is to go on, and
    // a guess at what the project is beats a blank column.
    match app.project_guessed_icon(project) {
        Some((name, colour)) => draw_drawn_icon(frame, name, Some(colour), area),
        None => false,
    }
}

/// Draw an icon from the drawing set over `area`, at the size that room comes to in
/// pixels — a line drawing that has been resized is a line drawing with grey lines, so
/// it is made the size it is wanted rather than made once and shrunk.
fn draw_drawn_icon(frame: &mut Frame, name: &str, colour: Option<&str>, area: Rect) -> bool {
    let cell = picture::cell_size().unwrap_or(Size::new(8, 16));
    let side = (area.width as u32 * cell.width as u32)
        .min(area.height as u32 * cell.height as u32)
        .min(MOST_ICON_PIXELS);
    let Some(drawn) = crate::lucide::draw(name, colour, side) else {
        return false;
    };
    let key = format!("lucide:{name}:{}", colour.unwrap_or_default());
    let source = picture::Source::Pixels {
        bytes: &drawn.bytes,
        width: drawn.side,
        height: drawn.side,
        alpha: true,
    };
    if picture::place(&key, source, area.as_size()).is_some() {
        picture::draw(frame, &key, area, SignedPosition::from((0, 0)));
        return true;
    }
    false
}

fn draw_sidebar_icons(frame: &mut Frame, app: &App, inner: Rect, rows: &[SidebarRow]) {
    let mut line = 0usize;
    for row in rows.iter().skip(app.sidebar_offset) {
        let height = app.sidebar_row_height(row);
        if line >= inner.height as usize {
            break;
        }
        let y = inner.y + line as u16;
        line += height;
        let SidebarRow::Thread { id, .. } = row else {
            continue;
        };
        let Some(thread) = app.shell.threads.get(id) else {
            continue;
        };
        let project = &thread.project_id;
        // The last line of a row that only half fits is not there to draw on.
        let room = (inner.bottom().saturating_sub(y) as usize).min(height);
        if let Some(emoji) = app.project_emoji(project) {
            // Two cells of character in a column four wide, so it sits where the middle
            // of a picture would be rather than against the text.
            let area = Rect {
                x: inner.right().saturating_sub(ICON_COLUMN as u16 - 2),
                y,
                width: 2,
                height: 1,
            };
            frame.render_widget(Paragraph::new(Line::from(emoji.to_string())), area);
            continue;
        }
        let area = Rect {
            x: inner.right().saturating_sub(ICON_COLUMN as u16 - 1),
            y,
            width: 4,
            height: room as u16,
        };
        draw_project_picture(frame, app, project, area);
    }
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
        // A run still going has written more since this was read, and nothing tells us.
        if transcript.live {
            spans.push(Span::styled(
                "  still working",
                Style::default().fg(Color::Cyan),
            ));
        }
        spans.push(Span::styled(
            if transcript.live {
                "  r re-reads · q back"
            } else {
                "  q back"
            },
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
                Status::Failed(_) => "not connected",
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
        inner.height,
        expanded_hash,
        app.open_levels,
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
                    || existing.5 != key.5
                    || (thread.is_running() && existing.6 != key.6)
            }
            None => true,
        };
        if needs_rebuild {
            let blocks = timeline::build(
                thread,
                &app.expanded,
                app.open_levels,
                inner.width,
                inner.height,
            );
            let mut total = 0usize;
            let blocks: Vec<CachedBlock> = blocks
                .into_iter()
                .map(|mut block| {
                    let exports = std::mem::take(&mut block.exports);
                    let wrapped = timeline::wrap(&block.text, inner.width);
                    total += wrapped.lines.len();
                    // Where each line of the text starts turns text-line row ranges into
                    // content lines, and says where an image's reserved lines landed.
                    let (rows, images) = if block.rows.is_empty() {
                        (Vec::new(), Vec::new())
                    } else {
                        let starts = &wrapped.starts;
                        let rows = block
                            .rows
                            .iter()
                            .map(|region| timeline::Region {
                                first: starts[region.first],
                                end: starts[region.end.min(starts.len() - 1)],
                                ..region.clone()
                            })
                            .collect();
                        let images = block
                            .images
                            .iter()
                            .map(|placed| timeline::Placed {
                                line: starts[placed.line.min(starts.len() - 1)],
                                ..placed.clone()
                            })
                            .collect();
                        (rows, images)
                    };
                    CachedBlock {
                        block,
                        wrapped,
                        rows,
                        exports,
                        images,
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
        app.picture_ranges.clear();
        app.chat_pictures.clear();
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
        for cached in &cache.blocks {
            let CachedBlock {
                block,
                wrapped,
                rows,
                exports,
                images,
            } = cached;
            let start = y;
            let end = y + cached.height();
            y = end;
            if matches!(block.key, BlockKey::Message(_)) {
                app.message_starts.push(start);
            }
            if let Some((key, _)) = exports.first() {
                app.block_ranges.push((start, end, key.clone()));
            }
            app.chat_pictures.extend(block.pictures.iter().cloned());
            if matches!(block.key, BlockKey::Message(_)) {
                app.picture_ranges
                    .extend(rows.iter().map(|region| timeline::Region {
                        first: start + region.first,
                        end: start + region.end,
                        ..region.clone()
                    }));
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
            let visible = (cached.height() - skip).min((bottom - cursor) as usize);
            let rect = Rect {
                x: inner.x,
                y: cursor,
                width: inner.width,
                height: visible as u16,
            };
            // Already broken to the width, so the widget is only placing the rows.
            frame.render_widget(
                Paragraph::new(Text::from(wrapped.lines[skip..skip + visible].to_vec())),
                rect,
            );
            // Over the blank lines the block left for them, and clipped to what of the
            // block is on the screen: an image scrolls like the text it sits in.
            for placed in images {
                picture::draw(
                    frame,
                    &placed.key,
                    rect,
                    SignedPosition::from((placed.indent as i16, placed.line as i16 - skip as i16)),
                );
            }
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
    digits: bool,
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
    // The digits belong to the composer while something is written there, so say
    // what does answer rather than leaving a number that no longer does.
    spans.push(Span::styled(
        if digits {
            "(normal mode)"
        } else {
            "(:approve 1 — the composer has the digits)"
        },
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

fn draw_composer(frame: &mut Frame, app: &mut App, area: Rect) {
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
    // A thread being picked up after a long gap says what it is still carrying, in the
    // one place somebody about to write to it is already looking.
    let resume = app.resume_with_less().map(|used| {
        format!(
            "{} tokens from earlier · /compact resumes with less context",
            tokens_label(used)
        )
    });
    let placeholder = match resume.as_deref() {
        Some(resume) => resume,
        None if insert => "type a message · Enter sends · Ctrl-v Enter newline · Esc normal",
        None => "i or a click to write · d c y w b f t motions edit",
    };
    app.composer_area = text_area;
    let (lines, cursor) = app.composer.render(text_area, placeholder);
    frame.render_widget(Paragraph::new(lines), text_area);
    if insert || (app.mode == Mode::Normal && app.focus == Focus::Composer) {
        frame.set_cursor_position(cursor);
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    if app.mode == Mode::Command {
        // The line scrolls inside the row rather than wrapping, as the custom answer
        // field does: the status bar is one row and there is no second one to take.
        let (visible, cursor) = app
            .command_line
            .line_window(area.width.saturating_sub(1) as usize);
        let mut spans = vec![
            Span::styled(":", Style::default().fg(Color::Yellow)),
            Span::raw(visible.clone()),
        ];
        // What else `Tab` would offer, so that cycling is a choice being made rather
        // than words appearing one after another for no stated reason.
        if let Some(running) = &app.completing
            && running.options.len() > 1
        {
            let used = 1 + visible.chars().count() + 2;
            let mut room = (area.width as usize).saturating_sub(used);
            if room > 0 {
                spans.push(Span::raw("  "));
                for (i, option) in running.options.iter().enumerate() {
                    let width = option.chars().count() + 1;
                    if width > room {
                        spans.push(Span::styled("…", Style::default().fg(Color::DarkGray)));
                        break;
                    }
                    room -= width;
                    let style = if i == running.index {
                        Style::default().bg(Color::Yellow).fg(Color::Black)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    spans.push(Span::styled(option.clone(), style));
                    spans.push(Span::raw(" "));
                }
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        frame.set_cursor_position((area.x + 1 + cursor as u16, area.y));
        return;
    }
    if app.mode == Mode::Search
        && let Some(input) = &app.search_input
    {
        let prompt = if input.backward { "?" } else { "/" };
        let (visible, cursor) = input
            .query
            .line_window(area.width.saturating_sub(1) as usize);
        let line = Line::from(vec![
            Span::styled(prompt, Style::default().fg(Color::Yellow)),
            Span::raw(visible),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        frame.set_cursor_position((area.x + 1 + cursor as u16, area.y));
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
        (Mode::Normal, Focus::Composer)
            if app.composer.vim_visual().is_some_and(|v| v.linewise) =>
        {
            (
                " VISUAL LINE ",
                Style::default().bg(Color::Magenta).fg(Color::Black).bold(),
            )
        }
        (Mode::Normal, Focus::Composer) if app.composer.vim_visual().is_some() => (
            " VISUAL ",
            Style::default().bg(Color::Magenta).fg(Color::Black).bold(),
        ),
        (_, Focus::Chat) if app.chat_visual.is_some_and(|a| a.whole_lines) => (
            " VISUAL LINE ",
            Style::default().bg(Color::Magenta).fg(Color::Black).bold(),
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
    // `g` and `z` are keys that have not finished being pressed, so they are shown in
    // the same place as the rest of a command that is halfway typed. Where they wait
    // indefinitely this is the only sign that one is waiting at all.
    if let Some(prefix) = app.waiting_prefix() {
        spans.push(Span::styled(
            format!("{prefix} "),
            Style::default().fg(Color::Yellow),
        ));
    }
    // A partly typed command in insert mode is the same thing and is shown the same way.
    if app.literal_next {
        spans.push(Span::styled("^V ", Style::default().fg(Color::Yellow)));
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
        // What went wrong is a sentence, and it goes on the line under the header where
        // there is room for one. Here it is the dot that matters.
        Status::Failed(_) => (
            "●",
            Style::default().fg(Color::Red),
            " not connected".into(),
        ),
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
            // Compacting is a turn like any other on the wire, and says nothing while it
            // runs; a thread that looks like it is answering and is not is worth naming.
            let doing = if thread.is_compacting() {
                "compacting"
            } else {
                "running"
            };
            spans.push(Span::styled(
                format!("  {} {doing}", app.spinner_frame()),
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
        // A count when the rows for it were loaded, the mark alone when all there is to
        // go on is the server saying something is still running.
        let tasks = thread.running_tasks().len();
        let background = match (tasks, app.background_liveness()) {
            (0, None) => None,
            (0, Some(_)) => Some("  ⚙ bg".to_string()),
            (count, _) => Some(format!("  ⚙ {count} bg")),
        };
        if let Some(background) = background {
            spans.push(Span::styled(background, Style::default().fg(Color::Blue)));
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
        _ if picker.renaming.is_some() => " rename project · Enter renames · Esc keeps it ",
        PickerKind::Thread => " threads ",
        PickerKind::Model => " models ",
        PickerKind::Project => " new thread in project · ^R renames ",
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
    // The line scrolls inside its row rather than wrapping: the row below it is the
    // list, and a query that pushed it down would be a query in the way of the answer.
    let (visible, cursor) = picker
        .query
        .line_window(query_area.width.saturating_sub(2) as usize);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Magenta)),
            Span::raw(visible),
        ])),
        query_area,
    );
    frame.set_cursor_position((query_area.x + 2 + cursor as u16, query_area.y));

    let items = picker.filtered();
    // A project is drawn with what it is known by, in room kept at the front of the row:
    // the emoji it was given, or failing that the icon its checkout carries.
    let icons = picker.kind == PickerKind::Project;
    let gutter = if icons { MARK } else { 0 };
    let label_width = (list_area.width as usize).saturating_sub(4 + gutter);
    let list_items: Vec<ListItem> = items
        .iter()
        .map(|item| {
            let label_len = item.label.chars().count().min(label_width * 2 / 3);
            let mark = match icons.then(|| app.project_emoji(&item.key)).flatten() {
                // Emoji are drawn at whatever width the terminal gives them, so the room
                // is filled out to keep the names in a line.
                Some(emoji) => {
                    let used = UnicodeWidthStr::width(emoji).min(MARK);
                    format!("{emoji}{}", " ".repeat(MARK - used))
                }
                None => " ".repeat(gutter),
            };
            let label = format!("{mark}{}", fit(&item.label, label_len.max(1)));
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

    if icons {
        // Drawn after the list, over the room the labels left, so an icon lands on the
        // row it belongs to rather than under it.
        for (row, item) in items
            .iter()
            .skip(state.offset())
            .take(list_area.height as usize)
            .enumerate()
        {
            // The emoji was drawn with the label; a picture goes on over the row.
            if app.project_emoji(&item.key).is_some() {
                continue;
            }
            let area = Rect {
                x: list_area.x + 2,
                y: list_area.y + row as u16,
                width: 2,
                height: 1,
            };
            draw_project_picture(frame, app, &item.key, area);
        }
    }
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
        let mut col = 0;
        while col < inner.width {
            let Some(cell) = screen.cell(row, col) else {
                col += 1;
                continue;
            };
            if cell.is_wide_continuation() {
                col += 1;
                continue;
            }
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
            // An emoji vt100 spread over several cells goes back together, drawn once
            // at the first of them rather than each piece over the one before it.
            let (contents, span) = pane.cluster_at(row, col);
            if let Some(target) = buffer.cell_mut(Position::new(inner.x + col, inner.y + row)) {
                target.set_symbol(if contents.is_empty() { " " } else { &contents });
                target.set_style(style);
            }
            // The rest of the columns it came from keep its colours, so the background
            // does not break where the glyph is narrower than the cells it was in.
            for extra in 1..span {
                if let Some(target) =
                    buffer.cell_mut(Position::new(inner.x + col + extra, inner.y + row))
                {
                    target.set_symbol(" ");
                    target.set_style(style);
                }
            }
            col += span;
        }
    }

    // Pictures the program sent sit over the cells, and ride the text as it scrolls.
    let top = pane.top_line();
    for placement in pane.placements() {
        let row = placement.line - top;
        if row >= i64::from(inner.height) || row + i64::from(placement.size.height) <= 0 {
            continue;
        }
        crate::picture::draw(
            frame,
            &placement.key,
            inner,
            SignedPosition::from((placement.column as i16, row as i16)),
        );
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
fn draw_worktrees(frame: &mut Frame, app: &App, area: Rect) {
    let worktrees = &app.worktrees;
    let width = 88.min(area.width);
    let inner_width = width.saturating_sub(4) as usize;
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines: Vec<Line<'static>> = Vec::new();
    // Three lines each, and there can be a great many: show the ones around the cursor
    // rather than a list that runs off the bottom of the screen.
    let selection = app.worktree_selected.min(worktrees.len().saturating_sub(1));
    let room = (area.height.saturating_sub(6) / 3).max(1) as usize;
    let first = selection
        .saturating_sub(room.saturating_sub(1))
        .min(worktrees.len().saturating_sub(room.min(worktrees.len())));
    if first > 0 {
        lines.push(Line::from(Span::styled(format!("   ⋯ {first} above"), dim)));
    }
    for (index, worktree) in worktrees.iter().enumerate().skip(first).take(room) {
        let selected = index == selection;
        // What it is waiting for, which is what says whether it can go.
        let (glyph, glyph_style) = if worktree.running {
            (app.spinner_frame(), Style::default().fg(Color::Cyan))
        } else if worktree.settled {
            ("✓", Style::default().fg(Color::Green))
        } else {
            ("●", Style::default().fg(Color::Yellow))
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
                fit(&worktree.title, inner_width.saturating_sub(6)),
                title_style,
            ),
        ]));
        let mut detail = vec![
            Span::styled(format!("      {}", worktree.project), dim),
            Span::styled(
                format!("  {}", worktree.branch.as_deref().unwrap_or("no branch")),
                Style::default().fg(Color::Blue),
            ),
        ];
        match worktree.changes {
            None => detail.push(Span::styled("  reading…", dim)),
            Some(true) => detail.push(Span::styled(
                "  uncommitted work",
                Style::default().fg(Color::Yellow),
            )),
            Some(false) => detail.push(Span::styled("  clean", dim)),
        }
        lines.push(Line::from(detail));
        lines.push(Line::from(Span::styled(
            format!(
                "      {}",
                fit(&worktree.path, inner_width.saturating_sub(6))
            ),
            dim,
        )));
    }
    let below = worktrees.len().saturating_sub(first + room);
    if below > 0 {
        lines.push(Line::from(Span::styled(format!("   ⋯ {below} below"), dim)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  j k select · Enter open the thread · x remove · X remove anyway · Esc",
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
    let settled = worktrees.iter().filter(|w| w.settled).count();
    let block = Block::bordered()
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(
            " worktrees ({}, {settled} settled) ",
            worktrees.len()
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(text), inner);
    draw_worktree_confirm(frame, app, area);
}

/// The question a forced removal asks first, over the list it was asked from. Everything
/// else in that list is recoverable — a removed worktree leaves its branch behind — and
/// this is the one thing that is not, so it shows the work it would take rather than
/// asking about it in the abstract.
fn draw_worktree_confirm(frame: &mut Frame, app: &App, area: Rect) {
    let Some((confirm, worktree)) = app.confirming_worktree() else {
        return;
    };
    let width = 76.min(area.width);
    let inner_width = width.saturating_sub(4) as usize;
    let dim = Style::default().fg(Color::DarkGray);
    let warn = Style::default().fg(Color::Red);
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            fit(&worktree.title, inner_width.saturating_sub(2)),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        format!("  {}", fit(&worktree.path, inner_width.saturating_sub(2))),
        dim,
    )));
    // What survives this, which is most of it: the branch is not the worktree, and
    // anything committed to it is still there afterwards.
    lines.push(Line::from(vec![
        Span::styled("  ", dim),
        Span::styled(
            worktree
                .branch
                .clone()
                .unwrap_or_else(|| "no branch".into()),
            Style::default().fg(Color::Blue),
        ),
        Span::styled(" stays · only the checkout goes", dim),
    ]));
    lines.push(Line::from(""));

    // The list of what goes, which is the whole point of asking.
    let files = &worktree.files;
    match (worktree.changes, files.is_empty()) {
        (None, true) => lines.push(Line::from(Span::styled(
            "  still asking the server what is in it",
            dim,
        ))),
        (Some(true), true) => lines.push(Line::from(Span::styled(
            "  it has uncommitted work in it, which the server did not list",
            warn,
        ))),
        (_, false) => {
            lines.push(Line::from(Span::styled(
                format!(
                    "  {} uncommitted or untracked file{} would go with it:",
                    files.len(),
                    if files.len() == 1 { "" } else { "s" }
                ),
                warn,
            )));
        }
        (Some(false), true) => lines.push(Line::from(Span::styled(
            "  nothing uncommitted in it now",
            dim,
        ))),
    }
    // Room for the rest of the box: the border, the title, the path, the branch, the
    // blank, the count, the line saying how many are below, the blank, and the hint.
    let room = (area.height.saturating_sub(10)).max(1) as usize;
    let first = confirm.offset.min(files.len().saturating_sub(1));
    let first = first.min(files.len().saturating_sub(room.min(files.len())));
    for file in files.iter().skip(first).take(room) {
        // No label on a file with no line counts: an untracked one and a file whose
        // mode alone changed both come through with none, and saying which would be
        // making it up.
        let counts = match (file.insertions, file.deletions) {
            (0, 0) => String::new(),
            (added, 0) => format!("  +{added}"),
            (0, removed) => format!("  −{removed}"),
            (added, removed) => format!("  +{added} −{removed}"),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "    {}",
                    fit(&file.path, inner_width.saturating_sub(4 + counts.len()))
                ),
                Style::default(),
            ),
            Span::styled(counts, dim),
        ]));
    }
    let below = files.len().saturating_sub(first + room);
    if below > 0 {
        lines.push(Line::from(Span::styled(
            format!("    ⋯ {below} more · j k to move through them"),
            dim,
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  Enter", warn.add_modifier(Modifier::BOLD)),
        Span::styled(
            if files.is_empty() {
                " removes it and anything in it for good  ·  "
            } else {
                " removes it and these files for good  ·  "
            },
            dim,
        ),
        Span::styled("Esc", Style::default()),
        Span::styled(" or ", dim),
        Span::styled("q", Style::default()),
        Span::styled(" keeps it", dim),
    ]));

    let text = Text::from(lines);
    let height = (text.lines.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::bordered().border_style(warn).title(Span::styled(
        " remove this worktree and lose what is in it? ",
        warn.add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(text), inner);
}

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
        if app.transcript_path(agent).is_none() {
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

/// The agent's unfinished background tasks, and what the server says is still live in
/// the thread even when no row for it was loaded. The protocol has no per-task stop: the
/// stop offered here is the session's own, which is what the desktop's `Monitoring ·
/// Stop` sends too.
fn draw_tasks(frame: &mut Frame, app: &App, area: Rect) {
    let tasks = app.running_tasks();
    let liveness = app.background_liveness();
    let now = crate::commands::now_iso();
    let mut lines: Vec<Line<'static>> = Vec::new();
    if tasks.is_empty() {
        match liveness {
            // The server keeps its own register of what is still running, and it is
            // older than anything on screen: a watcher left going for hours started in
            // activities that have long since fallen off the end of the loaded history.
            // So it is said plainly rather than answered with "nothing running".
            Some(label) => {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!(" {} ", app.spinner_frame()),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(
                        format!("the server says this thread is {label}"),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ]));
                lines.push(Line::from(Span::styled(
                    "   it started before the history tria loaded",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            None => lines.push(Line::from(Span::styled(
                "  no background tasks running",
                Style::default().fg(Color::DarkGray),
            ))),
        }
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
    if app.confirm_stop_session {
        lines.push(Line::from(Span::styled(
            "  stop the session, and every process the agent started?",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "  S again or y · anything else leaves it running",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "  s stops it, as Ctrl-c does · S stops the whole session, after asking",
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(Span::styled(
            "  Esc to close",
            Style::default().fg(Color::DarkGray),
        )));
    }
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
        .title(match (tasks.len(), liveness) {
            // Nothing to count, but something running: a count of zero would read as
            // the opposite of what the panel is saying.
            (0, Some(_)) => " background tasks ".to_string(),
            (count, _) => format!(" background tasks ({count}) "),
        });
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

/// What each signed-in account has left of its subscription: one row per rolling
/// window, as the provider last reported it to the server. The figures are a probe's
/// answer rather than a live count, so the panel says how old they are.
fn draw_usage(frame: &mut Frame, app: &App, area: Rect) {
    let accounts = app.usage_accounts();
    let now = crate::commands::now_iso();
    let dim = Style::default().fg(Color::DarkGray);
    let width = 64.min(area.width);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if accounts.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no account here reports a quota",
            dim,
        )));
    }
    // The account this thread is talking to is the one the next message is spent from.
    let current = app.current_provider_instance();
    let mut oldest: Option<&str> = None;
    for account in &accounts {
        let Some(limits) = &account.usage_limits else {
            continue;
        };
        if !limits.checked_at.is_empty()
            && oldest.is_none_or(|kept| limits.checked_at.as_str() < kept)
        {
            oldest = Some(&limits.checked_at);
        }
        let mine = current.is_some_and(|instance| instance == account.instance_id);
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {}", account.label()),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(if mine { "  · this thread" } else { "" }, dim),
        ]));
        if let Some(unavailable) = &limits.unavailable {
            let reason = match unavailable.reason.as_str() {
                "unsupported" => "no quota to report".to_string(),
                _ => unavailable
                    .message
                    .clone()
                    .unwrap_or_else(|| "could not be read".to_string()),
            };
            lines.push(Line::from(Span::styled(format!("    {reason}"), dim)));
        }
        let mut windows: Vec<&crate::model::UsageWindow> = limits.windows.iter().collect();
        windows.sort_by_key(|window| window.rank());
        for window in windows {
            let percent = window.used_percent.clamp(0.0, 100.0);
            let resets = window
                .resets_at
                .as_deref()
                .map(|at| reset_label(&now, at))
                .unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(
                    format!("    {:<14}", fit(&window.label, 14)),
                    Style::default(),
                ),
                Span::styled(meter(percent), meter_style(percent)),
                Span::styled(format!(" {percent:>3.0}%  "), meter_style(percent)),
                Span::styled(resets, dim),
            ]));
        }
    }
    if let Some(checked_at) = oldest {
        let age = elapsed_label(checked_at, &now);
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            if age.is_empty() {
                "  r reads them again · Esc to close".to_string()
            } else {
                format!("  read {age} ago · r reads them again · Esc to close")
            },
            dim,
        )));
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  r reads them again · Esc to close",
            dim,
        )));
    }
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
        .title(" usage limits ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

/// A quota as a bar. Sixteen cells, so one cell is a little over six percent and a
/// window barely touched still shows something.
fn meter(percent: f64) -> String {
    const CELLS: usize = 16;
    let filled = ((percent / 100.0 * CELLS as f64).round() as usize).min(CELLS);
    format!("{}{}", "█".repeat(filled), "░".repeat(CELLS - filled))
}

/// Green while there is room, amber once the end is in sight, red when it is nearly
/// gone — the same reading as the desktop's bars.
fn meter_style(percent: f64) -> Style {
    let colour = if percent >= 90.0 {
        Color::Red
    } else if percent >= 70.0 {
        Color::Yellow
    } else {
        Color::Green
    };
    Style::default().fg(colour)
}

/// When the window comes back. Coarser than an elapsed label where a quota window is: a
/// weekly reset is days away and its minutes are not worth the width. A reset already
/// due is not counted down to — the figures are a probe's answer, and one taken before
/// the reset says nothing about after it.
fn reset_label(now: &str, at: &str) -> String {
    let parse = |text: &str| {
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()
    };
    let (Some(now), Some(at)) = (parse(now), parse(at)) else {
        return String::new();
    };
    let seconds = (at - now).whole_seconds();
    if seconds <= 0 {
        return "resetting".to_string();
    }
    // Up to the minute rather than down to it: a window is back when it is back, and a
    // reset two hours and fifty-nine seconds away is not two hours away.
    let minutes = (seconds + 59) / 60;
    if minutes < 60 {
        format!("resets in {minutes}m")
    } else if minutes < 24 * 60 {
        format!("resets in {}h{:02}", minutes / 60, minutes % 60)
    } else {
        let (days, hours) = (minutes / (24 * 60), minutes % (24 * 60) / 60);
        match hours {
            0 => format!("resets in {days}d"),
            hours => format!("resets in {days}d {hours}h"),
        }
    }
}

/// A token count the way the desktop writes one: `840`, `9.4k`, `101k`, `1.2m`.
pub fn tokens_label(value: u64) -> String {
    let thousands = value as f64 / 1_000.0;
    if value < 1_000 {
        value.to_string()
    } else if value < 10_000 {
        format!("{thousands:.1}k").replace(".0k", "k")
    } else if value < 1_000_000 {
        format!("{}k", thousands.round())
    } else {
        format!("{:.1}m", value as f64 / 1_000_000.0).replace(".0m", "m")
    }
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
    // What `g` runs is the config file's to say, so the help reads it rather than
    // naming the one program tria happens to ship with.
    let programs = if app.programs.is_empty() {
        "  g and a key             run a program, once the config file binds one".to_string()
    } else {
        let keys: Vec<String> = app.programs.iter().map(|p| format!("g{}", p.key)).collect();
        let names: Vec<&str> = app.programs.iter().map(|p| p.name()).collect();
        format!(
            "  {:<24}{} in the thread's terminal pane",
            keys.join(" "),
            names.join(", ")
        )
    };
    let text = Text::from(vec![
        Line::from(Span::styled("Normal", Style::default().bold())),
        Line::from("  Tab / Shift-Tab         cycle focus: composer → chat → threads · Esc back"),
        Line::from("  J/K                     next / previous thread"),
        Line::from("  /                       fuzzy thread picker"),
        Line::from(""),
        Line::from("  chat (focused):"),
        Line::from("  j k  { }  gg G  Ctrl-d/u/f/b/e/y   move the cursor / by message / scroll"),
        Line::from("  h l  0 $  w b           move along the line, character by character"),
        Line::from(
            "  za or Enter             fold or unfold the tool group or row under the cursor",
        ),
        Line::from("  v / V then y  ·  yy  ·  Ny   yank characters or lines to the clipboard"),
        Line::from("  / ?  n N               search forward / backward, next / previous match"),
        Line::from(""),
        Line::from("  n                       new thread (pick project; ^R renames one)"),
        Line::from("  m                       change model"),
        Line::from("  i / Enter               write a message"),
        Line::from("  za                      fold or unfold the tool group or row here"),
        Line::from(
            "  zr  zm  zR  zM          open or shut a level: groups, then the calls in them",
        ),
        Line::from("  1..9                    answer a pending approval, with nothing written in"),
        Line::from("                          the composer; :approve <n> answers it whatever is"),
        Line::from(
            "  ga                      answer the agent's question (digits, Space, c custom, Enter)",
        ),
        Line::from("  gy                      yank last assistant message (OSC 52)"),
        Line::from(programs.clone()),
        Line::from("  g!                      a shell in the pane; exiting it closes the popup"),
        Line::from("  gT                      background tasks still running in this thread:"),
        Line::from("                          s stops it as Ctrl-c does, S stops the session"),
        Line::from("                          after asking, since nothing undoes that one"),
        Line::from("  gA                      subagents this thread has run: Enter reads one's"),
        Line::from("                          transcript (r re-reads a running one), y yanks its"),
        Line::from("                          report"),
        Line::from("  gS                      terminals for this thread: Enter attaches,"),
        Line::from("                          c opens a new one, x closes, r restarts"),
        Line::from("  in the pane             every key goes to the shell · Ctrl-\\ detaches"),
        Line::from("                          a paste goes to the shell as a paste"),
        Line::from("  gW                      worktrees threads are holding: Enter opens the"),
        Line::from("                          thread, x removes one, X removes a dirty one after"),
        Line::from("                          showing what would go with it"),
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
        Line::from("  v V                     select by character or by line · o swaps the ends"),
        Line::from("  .                       the last change again"),
        Line::from("  gJ  3J                  join lines, since J itself is the next thread"),
        Line::from("  s / S                   toggle sidebar / settled shelf"),
        Line::from("  s S J K n m / ? 1-9     the app's, and only while nothing is half typed"),
        Line::from("                          at the composer: a count, an operator or a"),
        Line::from("                          selection gives them back · s is cl, S is cc"),
        Line::from("                          no marks, no search in the composer, no macros"),
        Line::from("  gs                      settle the thread, or bring a settled one back"),
        Line::from("  gw                      new thread: fresh worktree or the project checkout"),
        Line::from(programs),
        Line::from("  g!                      a shell in the pane"),
        Line::from(
            "  gx                      open the link, picture, or pull request under the cursor",
        ),
        Line::from("  click a link            open it in the browser"),
        Line::from(
            "  gt                      switch to the tmux session for the thread's directory",
        ),
        Line::from("  gP                      split a tmux pane beside tria, in that directory"),
        Line::from("  gN                      a tmux window in this session, in that directory"),
        Line::from("  gD                      show the thread's directory in the file browser"),
        Line::from("  mouse drag              select chat text; released, it is copied"),
        Line::from("  Ctrl-c                  interrupt the running turn, or the background work"),
        Line::from("                          left without one; anywhere else, it is Esc"),
        // The setting as it stands rather than as it ships: somebody reading this to
        // find out how long the pair holds together wants the answer, not the default.
        Line::from("  g  z                    shown on the status bar while they wait"),
        Line::from(format!(
            "                          for the key after them · {}",
            match app.prefix_timeout {
                None => "no limit".to_string(),
                Some(ttl) => format!("{:.1}s", ttl.as_secs_f32()),
            }
        )),
        Line::from("                          set by prefix_timeout_ms, 0 for no limit"),
        Line::from(""),
        Line::from(Span::styled("Notifications", Style::default().bold())),
        Line::from(format!(
            "  a thread stops working  {}",
            match app.notify {
                crate::notify::When::Never => "not announced · notify in the config file",
                crate::notify::When::Always => "always announced · notify in the config",
                crate::notify::When::Unfocused => "announced unless this terminal has the focus",
            }
        )),
        Line::from("                          notify = unfocused | always | never"),
        Line::from(""),
        Line::from(Span::styled("Insert", Style::default().bold())),
        Line::from("  Enter send · Alt-Enter / Ctrl-j newline · Esc or Ctrl-c normal"),
        Line::from("  Ctrl-v or Ctrl-q        the next key as a character: Ctrl-v Enter is a"),
        Line::from("                          newline where Enter would send"),
        Line::from("  Up/Down or Ctrl-p/n     prompt history"),
        Line::from("  ← → Home End Ctrl-a/e  move · Alt-arrow or Alt-b/f by word"),
        Line::from("  Ctrl-w Ctrl-k Ctrl-u    kill word / to end / to start"),
        Line::from("  the same keys edit every other line in the client: a custom answer"),
        Line::from("  under ga, the : command line, a picker's query, and / in the chat"),
        Line::from("  (in a picker Ctrl-k is the list's, and moves the cursor up a row)"),
        Line::from(""),
        Line::from(Span::styled("Commands", Style::default().bold())),
        Line::from("  :new [project]  :model  :effort [level]  :mode plan|default"),
        Line::from("  :perm full-access|auto|auto-accept-edits|approval-required"),
        Line::from("  :rename <title>  :rename (regenerate)  :archive  :delete!"),
        Line::from(
            "  :pr  :tasks  :agents  :terminals  :tmux  :usage  :settle  :unsettle  :wake  :settled  :approve [n]  :stop  :stop! (the session)  :older  :answer  :dismiss  :reconnect  :sidebar  :worktrees  :split  :window  :q",
        ),
        Line::from("  the line takes the editing keys above, Up/Down for the commands"),
        Line::from("  run this session, and Tab to complete a name or a listed argument"),
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

#[cfg(test)]
mod tests {
    use base64::Engine;
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    use super::*;
    use crate::app::ThreadWorktree;

    fn worktree(index: usize) -> ThreadWorktree {
        ThreadWorktree {
            thread_id: format!("id-{index}"),
            title: format!("thread number {index}"),
            project: "a-project".into(),
            project_cwd: "/src/a-project".into(),
            path: format!("/worktrees/a-project/w-{index}"),
            branch: Some(format!("tria/{index}")),
            settled: true,
            running: false,
            changes: Some(false),
            files: Vec::new(),
        }
    }

    fn drawn(width: u16, height: u16, app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw_worktrees(frame, app, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let area = buffer.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn dirty(app: &mut App, files: &[(&str, u32, u32)]) {
        let mut worktree = worktree(1);
        worktree.changes = Some(true);
        worktree.files = files
            .iter()
            .map(|(path, insertions, deletions)| crate::model::VcsFile {
                path: (*path).into(),
                insertions: *insertions,
                deletions: *deletions,
            })
            .collect();
        let path = worktree.path.clone();
        app.worktrees = vec![worktree];
        app.worktree_confirm = Some(crate::app::WorktreeConfirm { path, offset: 0 });
    }

    /// Removing a worktree is ordinarily safe — the branch stays — and git refuses one
    /// with anything uncommitted in it. `X` overrides that refusal, which is the one key
    /// in the list that destroys work, and untracked files are nowhere else. So it shows
    /// the work first.
    #[test]
    fn forcing_a_worktree_out_says_what_would_go_with_it() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        dirty(
            &mut app,
            &[("notes.md", 0, 0), ("src/lib.rs", 12, 3), ("out/", 0, 0)],
        );
        let drawn = drawn(90, 24, &app);

        assert!(drawn.contains("remove this worktree"), "{drawn}");
        assert!(
            drawn.contains("3 uncommitted or untracked files"),
            "{drawn}"
        );
        for path in ["notes.md", "src/lib.rs", "out/"] {
            assert!(drawn.contains(path), "{path} is not in:\n{drawn}");
        }
        // The counts are there where there are any, and nothing is labelled where the
        // server cannot say: an untracked file and a mode change both come with none.
        assert!(drawn.contains("+12 −3"), "{drawn}");
        assert!(drawn.contains("Enter"), "{drawn}");
        assert!(drawn.contains("Esc"), "{drawn}");
    }

    /// The list can be longer than the box, so it moves.
    #[test]
    fn a_long_list_of_losses_can_be_read_through() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        let many: Vec<(String, u32, u32)> =
            (0..40).map(|n| (format!("file-{n}.txt"), 0, 0)).collect();
        let many: Vec<(&str, u32, u32)> =
            many.iter().map(|(p, i, d)| (p.as_str(), *i, *d)).collect();
        dirty(&mut app, &many);

        let top = drawn(90, 24, &app);
        assert!(top.contains("file-0.txt"), "{top}");
        assert!(top.contains("more"), "it says there are more: {top}");

        app.worktree_confirm.as_mut().unwrap().offset = 39;
        let bottom = drawn(90, 24, &app);
        assert!(bottom.contains("file-39.txt"), "{bottom}");
        assert!(!bottom.contains("file-0.txt"), "{bottom}");
    }

    fn status_line(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(60, 1)).unwrap();
        terminal
            .draw(|frame| draw_status(frame, app, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect()
    }

    /// `g` on its own is a key that has not finished being pressed. Nothing said so, so
    /// a `g` that had quietly timed out and a `g` still waiting looked the same, and
    /// with the timeout turned off the only sign of one left pending is this.
    #[test]
    fn a_prefix_waiting_for_its_second_key_is_on_the_screen() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        assert!(!status_line(&app).contains(" g "));

        app.pending_prefix = Some(('g', Instant::now()));
        assert!(status_line(&app).contains(" g "), "{}", status_line(&app));

        // And it goes when its moment does, so the screen is not saying it is waiting
        // for a key it has already given up on.
        app.pending_prefix = Some(('g', Instant::now() - Duration::from_secs(5)));
        assert!(!status_line(&app).contains(" g "), "{}", status_line(&app));
    }

    /// A config with two signed-in accounts, in the shape the server sends one. The
    /// times are relative to the clock the panel reads, which is the real one.
    fn with_limits(app: &mut App) {
        let at = |minutes: i64| {
            (time::OffsetDateTime::now_utc() + time::Duration::minutes(minutes))
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        };
        app.config = serde_json::from_value(json!({
            "providers": [
                {
                    "instanceId": "claudeAgent", "driver": "claudeAgent", "enabled": true,
                    "installed": true, "status": "ready", "models": [],
                    "usageLimits": {
                        "checkedAt": at(-6),
                        // Out of order on the wire, and in order on the screen.
                        "windows": [
                            {"id": "seven_day", "kind": "weekly", "label": "Weekly",
                             "usedPercent": 16, "resetsAt": at(4 * 24 * 60 + 180)},
                            {"id": "five_hour", "kind": "session", "label": "Session",
                             "usedPercent": 4, "resetsAt": at(150)}
                        ]
                    }
                },
                {
                    "instanceId": "claudeAgent_claude_2", "driver": "claudeAgent",
                    "displayName": "second account", "enabled": true, "installed": true,
                    "status": "ready", "models": [],
                    "usageLimits": {
                        "checkedAt": at(-1),
                        "windows": [
                            {"id": "five_hour", "kind": "session", "label": "Session",
                             "usedPercent": 14, "resetsAt": at(11)},
                            {"id": "seven_day", "kind": "weekly", "label": "Weekly",
                             "usedPercent": 87, "resetsAt": at(2 * 24 * 60)}
                        ]
                    }
                },
                {
                    "instanceId": "codex", "driver": "codex", "enabled": false,
                    "installed": false, "status": "unavailable", "models": []
                }
            ]
        }))
        .unwrap();
    }

    fn usage_screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(70, 16)).unwrap();
        terminal
            .draw(|frame| draw_usage(frame, app, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let area = buffer.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every account that reports a quota, each window in the order it runs out, with
    /// how full it is and when it comes back.
    #[test]
    fn the_usage_window_says_what_is_left_of_each_subscription() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        with_limits(&mut app);
        let screen = usage_screen(&app);

        // The session window is drawn first however the provider ordered it.
        let rows: Vec<&str> = screen
            .lines()
            .filter(|line| line.contains("Session") || line.contains("Weekly"))
            .collect();
        assert!(rows[0].contains("Session") && rows[1].contains("Weekly"));
        assert!(rows[0].contains("4%"), "the bar is labelled: {}", rows[0]);
        assert!(rows[0].contains("resets in 2h30"), "{}", rows[0]);
        assert!(rows[1].contains("resets in 4d 3h"), "{}", rows[1]);
        // A round number of days is not padded out with an hour count of nothing.
        assert!(!rows[3].contains("2d 0h"), "{}", rows[3]);
        // A window under the hour is counted in minutes, not rounded away.
        assert!(rows[2].contains("resets in 11m"), "{}", rows[2]);
        // The bar fills with what is used: 87% of sixteen cells is fourteen.
        assert_eq!(rows[3].matches('█').count(), 14);
        assert_eq!(rows[3].matches('░').count(), 2);
        // An account with no quota to report is not an account with an empty one.
        assert!(!screen.contains("codex"));
        assert!(screen.contains("read 6m ago"));
        // Nothing says which account this thread is on, because there is no thread.
        assert!(!screen.contains("this thread"));
    }

    /// The account the next message is spent from is the one worth finding first.
    #[test]
    fn the_account_this_thread_is_on_is_marked() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        with_limits(&mut app);
        app.thread = Some(crate::state::ThreadState::from_snapshot(
            serde_json::from_value(json!({
                "snapshotSequence": 1,
                "thread": {
                    "id": "t1", "projectId": "p", "title": "t1",
                    "modelSelection": {"instanceId": "claudeAgent_claude_2", "model": "m"},
                    "messages": [], "activities": []
                }
            }))
            .unwrap(),
        ));
        let screen = usage_screen(&app);
        let marked: Vec<&str> = screen
            .lines()
            .filter(|line| line.contains("this thread"))
            .collect();
        assert_eq!(marked.len(), 1);
        assert!(marked[0].contains("second account"), "{}", marked[0]);
    }

    /// A server that reports no quota at all, which is every server without a
    /// subscription signed in to it.
    #[test]
    fn a_config_with_no_quota_on_it_says_so() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let app = App::new(handle, events);
        assert!(usage_screen(&app).contains("no account here reports a quota"));
    }

    /// The whole way through: a program in the pane sends a picture, and it lands on the
    /// screen rather than in the text.
    #[test]
    fn a_picture_a_program_sent_is_drawn_over_the_pane() {
        crate::picture::draw_in_halfblocks();
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        // The size the popup gives the pane inside a screen of forty by twelve.
        let mut pane = crate::term::Pane::new("t".into(), "term-1".into(), "shell".into(), 32, 8);
        let _ = pane.feed(&format!(
            "$ show\r\n\x1b_Ga=T,f=100,c=6,r=3;{}\x1b\\",
            crate::picture::test_png(60, 60)
        ));
        app.pane = Some(pane);

        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|frame| draw_terminal_pane(frame, &mut app, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        // Half-blocks are colour rather than glyphs, so a cell of the picture is one that
        // has been painted.
        let painted = |x: u16, y: u16| buffer[(x, y)].bg != Color::Reset;
        // The pane is inset by the popup's border, and the picture starts on the line
        // under the command that printed it.
        assert!(painted(4, 3), "the picture starts where the cursor was");
        assert!(!painted(4, 2), "and not on the line above it");
        assert!(painted(9, 5), "down to its far corner");
        assert!(!painted(10, 3), "and no wider than it was given");
        // Nothing of the sequence was printed as text.
        let row: String = (0..40).map(|x| buffer[(x, 2)].symbol()).collect();
        assert!(row.contains("$ show"), "{row}");
        assert!(!row.contains("a=T"), "{row}");
    }

    /// More worktrees than there are rows for is the case worth drawing: the cursor has
    /// to stay on the screen, and what is not on it has to be said rather than dropped.
    #[test]
    fn a_worktree_list_longer_than_the_screen_follows_the_cursor() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.worktrees = (0..20).map(worktree).collect();
        app.worktree_selected = 19;

        let screen = drawn(100, 24, &app);
        assert!(screen.contains("thread number 19"), "{screen}");
        assert!(!screen.contains("thread number 0 "), "{screen}");
        assert!(screen.contains("above"), "{screen}");
        assert!(screen.contains("worktrees (20, 20 settled)"), "{screen}");

        // From the top, the other end is the one summarised.
        app.worktree_selected = 0;
        let screen = drawn(100, 24, &app);
        assert!(screen.contains("thread number 0"), "{screen}");
        assert!(screen.contains("below"), "{screen}");
    }

    fn thread_saying(text: &str) -> crate::state::ThreadState {
        let snapshot: crate::model::ThreadDetailSnapshot = serde_json::from_value(json!({
            "snapshotSequence": 1,
            "thread": {
                "id": "t1", "projectId": "p1", "title": "Test",
                "modelSelection": {"instanceId": "claudeAgent", "model": "m"},
                "runtimeMode": "full-access", "latestTurn": null, "session": null,
                "messages": [{"id": "m1", "role": "user", "text": text}],
                "activities": []
            }
        }))
        .unwrap();
        crate::state::ThreadState::from_snapshot(snapshot)
    }

    /// The same thread, with the agent doing the talking: markdown, and so pictures.
    fn thread_answering(text: &str) -> crate::state::ThreadState {
        let snapshot: crate::model::ThreadDetailSnapshot = serde_json::from_value(json!({
            "snapshotSequence": 1,
            "thread": {
                "id": "t1", "projectId": "p1", "title": "Test",
                "modelSelection": {"instanceId": "claudeAgent", "model": "m"},
                "runtimeMode": "full-access", "latestTurn": null, "session": null,
                "messages": [{"id": "m1", "role": "assistant", "text": text}],
                "activities": []
            }
        }))
        .unwrap();
        crate::state::ThreadState::from_snapshot(snapshot)
    }

    /// An agent that has taken a screenshot writes it out and then shows it. The chat
    /// draws the picture under the caption, and the lines it goes on are the picture's,
    /// so `gx` anywhere on it opens the file.
    #[test]
    fn a_picture_a_message_shows_is_drawn_in_the_chat() {
        crate::picture::draw_in_halfblocks();
        let path = std::env::temp_dir().join("tria-a-chat-picture.png");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(crate::picture::test_png(120, 60))
            .unwrap();
        std::fs::write(&path, bytes).unwrap();
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.thread = Some(thread_answering(&format!(
            "Here it is.\n\n![the viewer serving a run]({})\n",
            path.display()
        )));

        let buffer = screen(60, &mut app);
        let region = app
            .picture_ranges
            .first()
            .cloned()
            .expect("the message has a picture");
        assert_eq!(
            app.chat_pictures,
            [(
                region.key.clone(),
                crate::timeline::Picture::File(path.to_string_lossy().into_owned())
            )],
            "and `gx` opens the file itself"
        );
        // Half-blocks are colour rather than glyphs, so a drawn row is a painted one.
        let painted = |y: u16| {
            (0..buffer.area.width).any(|x| {
                buffer[(x, y)]
                    .style()
                    .bg
                    .is_some_and(|bg| bg != Color::Reset)
            })
        };
        let top = app.chat_area.y + (region.first + 1 - app.chat_offset()) as u16;
        assert!(painted(top), "the picture starts under its caption");
        assert!(
            painted(app.chat_area.y + (region.end - 1 - app.chat_offset()) as u16),
            "and runs to the end of the lines it was given"
        );
        std::fs::remove_file(&path).unwrap();
    }

    /// The list is where somebody looks to find out what happened while they were
    /// somewhere else, so that is where a thread says it has spoken.
    #[test]
    fn a_thread_that_has_spoken_is_marked_in_the_list() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.sidebar_visible = true;
        let thread: crate::model::ThreadShell = serde_json::from_value(json!({
            "id": "t1", "projectId": "p", "title": "a quiet thread",
            "modelSelection": {"instanceId": "i", "model": "m"},
            "latestTurn": {
                "turnId": "turn", "state": "completed",
                "requestedAt": "2026-01-01T00:00:00Z"
            },
        }))
        .unwrap();
        app.shell.threads.insert("t1".into(), thread);

        assert!(chat(120, &mut app).contains("· a quiet thread"));
        app.unseen.insert("t1".into());
        assert!(chat(120, &mut app).contains("● a quiet thread"));
    }

    /// Monitoring outlasts the news it brings: the watcher sits there for hours, so the
    /// glyph it keeps has to say whether anything has happened since anybody looked.
    #[test]
    fn a_watching_thread_says_so_in_its_own_colour() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.sidebar_visible = true;
        let thread: crate::model::ThreadShell = serde_json::from_value(json!({
            "id": "t1", "projectId": "p", "title": "a watching thread",
            "modelSelection": {"instanceId": "i", "model": "m"},
            "backgroundLiveness": "monitoring",
        }))
        .unwrap();
        app.shell.threads.insert("t1".into(), thread);

        // The glyph stays whatever happens; only its colour is the news.
        let colour = |app: &mut App| {
            let buffer = screen(120, app);
            (0..buffer.area.width)
                .find(|x| buffer[(*x, 2)].symbol() == "◔")
                .map(|x| buffer[(x, 2)].fg)
                .expect("the thread is drawn as watching")
        };
        assert_eq!(colour(&mut app), Color::Blue);
        app.unseen.insert("t1".into());
        assert_eq!(colour(&mut app), Color::Green);
    }

    /// Somebody coming back to a long thread: the context window was counted hours ago
    /// and there is a lot behind it.
    fn thread_carrying(used: u64) -> crate::state::ThreadState {
        let counted = time::OffsetDateTime::now_utc() - time::Duration::hours(4);
        let counted = counted
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let snapshot: crate::model::ThreadDetailSnapshot = serde_json::from_value(json!({
            "snapshotSequence": 1,
            "thread": {
                "id": "t1", "projectId": "p1", "title": "Test",
                "modelSelection": {"instanceId": "claudeAgent", "model": "m"},
                "runtimeMode": "full-access", "latestTurn": null,
                "session": {"status": "idle"}, "messages": [],
                "activities": [{
                    "id": "a1", "kind": "context-window.updated", "tone": "info",
                    "summary": "", "payload": {"usedTokens": used},
                    "createdAt": counted
                }]
            }
        }))
        .unwrap();
        crate::state::ThreadState::from_snapshot(snapshot)
    }

    /// The composer is where somebody about to write is already looking, so that is
    /// where a thread says what it is still carrying.
    #[test]
    fn a_thread_picked_up_again_says_what_it_is_carrying() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.config = serde_json::from_value(json!({
            "providers": [{
                "instanceId": "claudeAgent", "driver": "claudeAgent",
                "enabled": true, "installed": true, "status": "ready",
                "slashCommands": [{"name": "compact"}]
            }]
        }))
        .unwrap();
        app.thread = Some(thread_carrying(101_000));
        assert!(chat(80, &mut app).contains("101k tokens from earlier"));

        // A provider with no way to compact has nothing to offer.
        app.config.providers[0].slash_commands.clear();
        assert!(!chat(80, &mut app).contains("tokens from earlier"));
    }

    /// An emoji vt100 spread over several cells is drawn once, whole, at the first of
    /// them. Drawn a cell apart the pieces overlapped and the last one won, which is
    /// how a trans flag in a shell prompt came out as the symbol on its own.
    // A draw tells the server the size it drew at, which wants a runtime to send on.
    #[tokio::test]
    async fn an_emoji_in_the_pane_is_drawn_whole() {
        let trans = "\u{1F3F3}\u{FE0F}\u{200D}\u{26A7}\u{FE0F}";
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.thread = Some(thread_answering("here"));
        app.mode = Mode::TerminalPane;
        let mut pane = crate::term::Pane::new("t1".into(), "term".into(), "shell".into(), 40, 8);
        let _ = pane.feed(&format!("{trans} ~ $ "));
        app.pane = Some(pane);

        let buffer = screen(60, &mut app);
        let drawn: Vec<String> = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
            .map(|(x, y)| buffer[(x, y)].symbol().to_string())
            .collect();
        assert!(
            drawn.iter().any(|symbol| symbol == trans),
            "the flag is drawn as one thing, whole"
        );
        assert!(
            !drawn.iter().any(|symbol| symbol == "\u{26A7}\u{FE0F}"),
            "and the symbol it is joined to is not drawn on top of it"
        );
    }

    fn screen(width: u16, app: &mut App) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, 16)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn chat(width: u16, app: &mut App) -> String {
        let buffer = screen(width, app);
        let area = buffer.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A selection in the composer is drawn where it is. `v` is worth having only if you
    /// can see what it has got hold of, and the mode says so beside it.
    #[test]
    fn a_selection_in_the_composer_is_marked() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.thread = Some(thread_saying("hello"));
        app.composer.set_text("one two");
        for key in ['0', 'v', 'e'] {
            app.composer.vim_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(key),
                crossterm::event::KeyModifiers::NONE,
            ));
        }

        let buffer = screen(40, &mut app);
        let area = app.composer_area;
        let marked = |x: u16| buffer[(area.x + x, area.y)].style().bg == Some(Color::Blue);
        assert!(marked(0) && marked(1) && marked(2), "the word is marked");
        assert!(!marked(3), "and the space after it is not");
        assert!(
            chat(40, &mut app).contains("VISUAL"),
            "and the mode says so"
        );
    }

    /// A message wider than the chat is drawn over several rows, and each of them has to
    /// carry the mark: without it the rows after the first read as somebody else talking.
    #[test]
    fn every_row_of_a_message_carries_its_mark() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.thread = Some(thread_saying("one two three four five six seven eight"));

        let screen = chat(30, &mut app);
        let said: Vec<&str> = screen
            .lines()
            .map(str::trim_start)
            .filter(|line| line.contains("one") || line.contains("eight"))
            .collect();
        assert!(said.len() > 1, "the message should have wrapped: {screen}");
        assert!(
            said.iter().all(|line| line.starts_with("\u{258c} ")),
            "{screen}"
        );
    }

    /// What is taken out of the chat is what was written into it, not what was drawn: no
    /// marks, and a message broken over rows comes back as the one line it was.
    #[test]
    fn what_is_yanked_is_the_message_and_not_its_decoration() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        let said = "one two three four five six seven eight";
        app.thread = Some(thread_saying(said));
        chat(30, &mut app);

        let start = app.message_starts.first().copied().unwrap();
        // The label is the block's first row; the message itself starts under it and
        // runs over the two rows it was broken into.
        let text = chat_span((start + 1, 0), (start + 2, usize::MAX)).unwrap();
        assert_eq!(text, said);
        // And a piece of a row is only that piece.
        assert_eq!(
            chat_span((start + 1, 4), (start + 1, 7)),
            Some("two".into())
        );
    }

    /// A character-wise selection covers the characters between its ends and nothing
    /// else — not the rest of the row, and not the mark the row is drawn with.
    #[test]
    fn a_selection_covers_the_characters_between_its_ends() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.thread = Some(thread_saying("one two three four five six seven eight"));
        chat(30, &mut app);

        // "two" on the first row of the message.
        let line = app.message_starts.first().copied().unwrap() + 1;
        app.focus = crate::app::Focus::Chat;
        // Following new output keeps the cursor on the last line; this is a reader
        // looking at something further up.
        app.scroll = crate::app::Scroll::Offset(0);
        app.chat_cursor = line;
        app.chat_column = 6;
        app.chat_visual = Some(crate::app::ChatAnchor {
            line,
            column: 4,
            whole_lines: false,
        });
        let buffer = screen(30, &mut app);

        let y = app.chat_area.y + (line - app.chat_offset()) as u16;
        let marked: String = (0..buffer.area.width)
            .filter(|x| buffer[(*x, y)].style().bg == Some(Color::Blue))
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        assert_eq!(marked, "two");
        // And the cursor, at the far end of it, is turned the other way round again.
        let x = app.chat_area.x + chat_column(line, 6);
        assert!(
            buffer[(x, y)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(chat_span((line, 4), (line, 7)), Some("two".into()));
    }

    /// The line being read is tinted and the cursor is the one character on it, so the
    /// conversation keeps the colours it is written in.
    #[test]
    fn the_cursor_marks_a_character_on_a_tinted_line() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.thread = Some(thread_saying("one two three four five six seven eight"));
        chat(30, &mut app);

        let line = app.message_starts.first().copied().unwrap() + 1;
        app.focus = crate::app::Focus::Chat;
        app.scroll = crate::app::Scroll::Offset(0);
        app.chat_cursor = line;
        app.chat_column = 4;
        let buffer = screen(30, &mut app);

        let y = app.chat_area.y + (line - app.chat_offset()) as u16;
        let marked: Vec<u16> = (0..buffer.area.width)
            .filter(|x| {
                buffer[(*x, y)]
                    .style()
                    .add_modifier
                    .contains(Modifier::REVERSED)
            })
            .collect();
        assert_eq!(marked, vec![app.chat_area.x + chat_column(line, 4)]);

        // The line carries the tint from edge to edge, and no other line does.
        let tint = |y: u16| {
            (app.chat_area.x..app.chat_area.x + app.chat_area.width)
                .all(|x| buffer[(x, y)].style().bg == Some(Color::Indexed(236)))
        };
        assert!(tint(y));
        assert!(!tint(y - 1));
    }

    /// A sidebar with threads in two projects, one of them in a worktree.
    fn with_threads(app: &mut App) {
        app.shell.apply(crate::model::ShellItem::Snapshot {
            snapshot: serde_json::from_value(json!({
                "snapshotSequence": 1,
                "projects": [
                    {"id": "p1", "title": "tria", "workspaceRoot": "/src/tria"},
                    {"id": "p2", "title": "shelfie", "workspaceRoot": "/src/shelfie",
                     "projectIcon": {"kind": "emoji", "emoji": "📚"}}
                ],
                "threads": [
                    {"id": "t1", "projectId": "p1",
                     "title": "Can we add a way to stop an active monitor",
                     "modelSelection": {"instanceId": "i", "model": "m"},
                     "createdAt": "2026-01-01T00:00:03Z"},
                    {"id": "t2", "projectId": "p1",
                     "title": "Nx cache invalidation PR 8683",
                     "branch": "t3code/mobile-update-publish",
                     "worktreePath": "/worktrees/t3code-afa6757e",
                     "modelSelection": {"instanceId": "i", "model": "m"},
                     "createdAt": "2026-01-01T00:00:02Z"},
                    {"id": "t3", "projectId": "p2",
                     "title": "What do I need to do to get this on playstore?",
                     "modelSelection": {"instanceId": "i", "model": "m"},
                     "createdAt": "2026-01-01T00:00:01Z"}
                ]
            }))
            .unwrap(),
        });
    }

    fn icon_bytes() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(picture::test_png(64, 64))
            .unwrap()
    }

    /// The sidebar's own columns, without the border it ends in.
    fn sidebar_text(app: &mut App) -> String {
        let buffer = screen(100, app);
        (0..buffer.area.height)
            .map(|y| {
                (0..SIDEBAR_WIDTH - 1)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The layout the config can ask for: the title on one line, what the thread is
    /// working in on the next, and the project drawn once beside both of them.
    #[test]
    fn a_thread_takes_two_lines_in_the_two_line_layout() {
        picture::draw_in_halfblocks();
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.sidebar_layout = SidebarLayout::TwoLine;
        with_threads(&mut app);
        app.give_favicon("p1", icon_bytes());

        let lines: Vec<String> = sidebar_text(&mut app).lines().map(str::to_string).collect();
        let row = |needle: &str| {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle} should be listed"))
        };
        // A thread the server gave a branch says which one under its title.
        let nx = row("Nx cache invalidation");
        assert!(
            lines[nx + 1].contains("t3code/mobile-update-pu"),
            "{}",
            lines[nx + 1]
        );
        // One running in the project's own checkout has no branch of its own to name,
        // so it names the project, which is the thing about it that is always known.
        let stop = row("Can we add a way to sto");
        assert!(lines[stop + 1].trim() == "tria", "{}", lines[stop + 1]);
        // And the rows that follow start two lines apart, not one.
        assert_eq!(nx, stop + 2);
    }

    /// The project's icon is drawn over the right of both the thread's lines, which is
    /// what a two-row picture needs four columns to do.
    #[test]
    fn the_project_icon_spans_both_lines_of_its_thread() {
        picture::draw_in_halfblocks();
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.sidebar_layout = SidebarLayout::TwoLine;
        with_threads(&mut app);
        app.give_favicon("p1", icon_bytes());
        let buffer = screen(100, &mut app);

        let text = |y: u16| {
            (0..SIDEBAR_WIDTH)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let row = |needle: &str| {
            (0..buffer.area.height)
                .find(|y| text(*y).contains(needle))
                .unwrap_or_else(|| panic!("{needle} should be listed"))
        };
        // Half blocks are colour rather than glyphs: a cell of the picture is one with
        // a background of its own.
        let painted = |y: u16| {
            (0..SIDEBAR_WIDTH)
                .filter(|x| buffer[(*x, y)].bg != Color::Reset)
                .count()
        };
        let titled = row("Nx cache invalidation");
        assert_eq!(painted(titled), 4, "the icon is four columns wide");
        assert_eq!(painted(titled + 1), 4, "and is on the branch's line too");
        // It is on the right, past everything the text could reach.
        let first = (0..SIDEBAR_WIDTH)
            .find(|x| buffer[(*x, titled)].bg != Color::Reset)
            .unwrap();
        assert_eq!(first, SIDEBAR_WIDTH - 5);

        // A project given an emoji is drawn with that instead, on the title's line.
        let store = row("What do I need");
        assert_eq!(painted(store), 0);
        assert!(text(store).contains("📚"), "{}", text(store));
    }

    /// The set the desktop app draws a project's icon from is a set of pictures, which
    /// a terminal can be given as pixels like any other picture.
    #[test]
    fn a_project_named_a_drawn_icon_is_drawn_with_it() {
        picture::draw_in_halfblocks();
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.sidebar_layout = SidebarLayout::TwoLine;
        app.shell.apply(crate::model::ShellItem::Snapshot {
            snapshot: serde_json::from_value(json!({
                "snapshotSequence": 1,
                "projects": [
                    {"id": "p1", "title": "mobile", "workspaceRoot": "/src/mobile",
                     "projectIcon": {"kind": "lucide", "name": "file-json", "color": "blue"}}
                ],
                "threads": [
                    {"id": "t1", "projectId": "p1", "title": "Nx cache invalidation",
                     "modelSelection": {"instanceId": "i", "model": "m"},
                     "createdAt": "2026-01-01T00:00:01Z"}
                ]
            }))
            .unwrap(),
        });
        let buffer = screen(100, &mut app);

        let titled = (0..buffer.area.height)
            .find(|y| {
                (0..SIDEBAR_WIDTH)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("Nx cache")
            })
            .unwrap();
        // Half blocks are colour, so the icon is the cells that have any, and it takes
        // the four columns kept for it on both of the thread's lines.
        let painted = |y: u16| {
            (0..SIDEBAR_WIDTH)
                .filter(|x| buffer[(*x, y)].bg != Color::Reset)
                .count()
        };
        assert_eq!(painted(titled), 4);
        assert_eq!(painted(titled + 1), 4);
        // And it is drawn in the colour it was named in.
        let blue = (0..SIDEBAR_WIDTH).any(|x| {
            matches!(buffer[(x, titled)].fg, Color::Rgb(r, g, b) if b > r && b > g)
                || matches!(buffer[(x, titled)].bg, Color::Rgb(r, g, b) if b > r && b > g)
        });
        assert!(
            blue,
            "the icon is drawn in the colour the project was given"
        );
    }

    /// Selecting a thread marks the whole of it, both lines and whatever colour each of
    /// them is written in.
    #[test]
    fn selecting_a_two_line_thread_marks_both_of_its_lines() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.sidebar_layout = SidebarLayout::TwoLine;
        with_threads(&mut app);
        app.focus = crate::app::Focus::Sidebar;
        // The header is row zero, so the first thread is row one.
        app.sidebar_selected = 1;
        let buffer = screen(100, &mut app);

        // Up to the icon, which is a picture and paints over the row it is on, and short
        // of the border, which is not the list's to mark.
        let text = SIDEBAR_WIDTH - 1 - ICON_COLUMN as u16;
        let marked = |y: u16| (0..text).all(|x| buffer[(x, y)].style().bg == Some(SELECTED));
        let titled = (0..buffer.area.height)
            .find(|y| {
                (0..SIDEBAR_WIDTH)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("Can we add a way")
            })
            .unwrap();
        assert!(marked(titled), "the title's line is marked");
        assert!(marked(titled + 1), "and so is the branch's");
        assert!(!marked(titled + 2), "and nothing else is");
        // The row is tinted, not filled: what is written on it keeps its own colours.
        let branch = (0..text)
            .map(|x| buffer[(x, titled + 1)].style().fg)
            .find(|fg| fg.is_some());
        assert_eq!(branch, Some(Some(Color::DarkGray)));
    }

    /// A project the server found an icon for is drawn with it, in room the label left;
    /// one with none is not shifted out of line by the others having icons.
    #[test]
    fn a_project_is_listed_with_the_icon_its_checkout_is_known_by() {
        picture::draw_in_halfblocks();
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.mode = Mode::Picker;
        app.picker = Some(crate::app::Picker {
            kind: PickerKind::Project,
            query: crate::composer::Composer::new(),
            selected: 1,
            items: vec![
                crate::app::PickerItem {
                    label: "shelfie".into(),
                    detail: "/src/shelfie".into(),
                    key: "p1".into(),
                },
                crate::app::PickerItem {
                    label: "tria".into(),
                    detail: "/src/tria".into(),
                    key: "p2".into(),
                },
            ],
            renaming: None,
        });
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(picture::test_png(64, 64))
            .unwrap();
        app.give_favicon("p1", bytes);

        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text = |y: u16| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let row = |name: &str| {
            (0..buffer.area.height)
                .find(|y| text(*y).contains(name))
                .unwrap_or_else(|| panic!("{name} should be listed"))
        };
        // Half blocks are what a terminal with no image protocol gets, and they are
        // colour rather than glyphs: a cell of the picture is one with a background.
        let painted = |y: u16| {
            (0..buffer.area.width)
                .filter(|x| buffer[(*x, y)].bg != Color::Reset)
                .count()
        };
        assert_eq!(
            painted(row("/src/shelfie")),
            2,
            "{:?}",
            text(row("/src/shelfie"))
        );
        // The one without an icon keeps the room anyway, so the labels line up.
        let label_at = |detail: &str, name: &str| {
            let line = text(row(detail));
            line.find(name).map(|byte| line[..byte].chars().count())
        };
        assert_eq!(
            label_at("/src/shelfie", "shelfie"),
            label_at("/src/tria", "tria")
        );
    }

    /// A project given an emoji is drawn with it, and that is what it is drawn with:
    /// somebody chose it, so it stands in front of the icon the server went looking for.
    #[test]
    fn a_project_given_an_emoji_is_drawn_with_it() {
        picture::draw_in_halfblocks();
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.shell.projects.insert(
            "p1".into(),
            serde_json::from_value(json!({
                "id": "p1", "title": "shelfie", "workspaceRoot": "/src/shelfie",
                "defaultModelSelection": null,
                "projectIcon": { "kind": "emoji", "emoji": "\u{1f4da}" }
            }))
            .unwrap(),
        );
        app.mode = Mode::Picker;
        app.picker = Some(crate::app::Picker {
            kind: PickerKind::Project,
            query: crate::composer::Composer::new(),
            selected: 0,
            items: vec![crate::app::PickerItem {
                label: "shelfie".into(),
                detail: "/src/shelfie".into(),
                key: "p1".into(),
            }],
            renaming: None,
        });
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(picture::test_png(64, 64))
            .unwrap();
        app.give_favicon("p1", bytes);

        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text = |y: u16| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let row = (0..buffer.area.height)
            .find(|y| text(*y).contains("shelfie"))
            .expect("the project should be listed");
        let line = text(row);
        assert!(line.contains("\u{1f4da}"), "{line:?}");
        // And the picture was left off, since the row already says what the project is:
        // nothing on it is coloured but the bar under the selected row.
        assert!(
            (0..buffer.area.width)
                .all(|x| matches!(buffer[(x, row)].bg, Color::Reset | Color::DarkGray)),
            "{line:?}"
        );
    }

    /// A screen with no room at all still draws something rather than panicking.
    #[test]
    fn a_worktree_list_in_no_room_draws_anyway() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.worktrees = vec![worktree(1)];
        drawn(24, 4, &app);
        app.worktrees.clear();
        drawn(24, 4, &app);
    }
}

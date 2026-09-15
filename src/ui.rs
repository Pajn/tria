//! Rendering. Layout: optional thread sidebar, header, chat, approval panel,
//! composer, status line. Overlays: picker and help.

use std::collections::HashSet;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    app::{App, Focus, Mode, PickerKind, Scroll, approval_options, thread_status},
    session::Status,
    timeline::{self, Block as ChatBlock, BlockKey},
};

const SIDEBAR_WIDTH: u16 = 34;
const MIN_WIDTH_FOR_SIDEBAR: u16 = 90;
const COMPOSER_MAX_ROWS: u16 = 8;

/// Cached rendered chat blocks with their wrapped heights.
#[derive(Default)]
pub struct ChatCache {
    key: Option<(String, u64, u16, u64, bool, usize)>,
    blocks: Vec<(ChatBlock, usize)>,
    total: usize,
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

    match app.mode {
        Mode::Picker => draw_picker(frame, app, area),
        Mode::Help => draw_help(frame, area),
        _ => {}
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

    let threads = app.shell.sorted_threads();
    let width = inner.width as usize;
    let items: Vec<ListItem> = threads
        .iter()
        .map(|t| {
            let (glyph, style) = if t.has_pending_approvals || t.has_pending_user_input {
                ("!", Style::default().fg(Color::Yellow))
            } else if t.is_running() {
                (app.spinner_frame(), Style::default().fg(Color::Cyan))
            } else if t.latest_turn.as_ref().is_some_and(|l| l.state == "error") {
                ("✗", Style::default().fg(Color::Red))
            } else {
                ("·", Style::default().fg(Color::DarkGray))
            };
            let project = app.shell.project_title(&t.project_id);
            let project_width = project.chars().count().min(12);
            let title_width = width.saturating_sub(project_width + 4);
            let title = fit(&t.title, title_width);
            let is_current = app.current_thread_id.as_ref() == Some(&t.id);
            let title_style = if is_current {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let padding = " ".repeat(title_width.saturating_sub(title.chars().count()) + 1);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{glyph} "), style),
                Span::styled(title, title_style),
                Span::raw(padding),
                Span::styled(
                    fit(project, project_width),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    if !threads.is_empty() {
        let selected = if focused {
            app.sidebar_selected.min(threads.len() - 1)
        } else {
            app.current_thread_id
                .as_ref()
                .and_then(|id| threads.iter().position(|t| &t.id == id))
                .unwrap_or(app.sidebar_selected.min(threads.len() - 1))
        };
        state.select(Some(selected));
    }
    let highlight = if focused {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::DIM)
    };
    let list = List::new(items).highlight_style(highlight);
    frame.render_stateful_widget(list, inner, &mut state);
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans: Vec<Span> = Vec::new();
    if let Some(draft) = &app.draft {
        spans.push(Span::styled(
            "New thread",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!("  in {}", app.shell.project_title(&draft.project_id)),
            Style::default().fg(Color::DarkGray),
        ));
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
        if let Some(branch) = &shell.branch {
            spans.push(Span::styled(
                format!("  {branch}"),
                Style::default().fg(Color::DarkGray),
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
    let Some(thread) = &app.thread else {
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
            let blocks: Vec<(ChatBlock, usize)> = blocks
                .into_iter()
                .map(|block| {
                    let height = Paragraph::new(block.text.clone())
                        .wrap(Wrap { trim: false })
                        .line_count(inner.width);
                    total += height;
                    (block, height)
                })
                .collect();
            cache.blocks = blocks;
            cache.total = total;
            cache.key = Some(key);
        }

        let height = inner.height as usize;
        let max_offset = cache.total.saturating_sub(height);
        let offset = match app.scroll {
            Scroll::Follow => max_offset,
            Scroll::Offset(o) => o.min(max_offset),
        };
        app.chat_viewport = (height, cache.total);
        app.work_ranges.clear();

        let mut y = 0usize;
        let mut cursor = inner.y;
        let bottom = inner.y + inner.height;
        for (block, block_height) in &cache.blocks {
            let start = y;
            let end = y + block_height;
            y = end;
            if let BlockKey::Work(key) = &block.key {
                app.work_ranges.push((start, end, key.clone()));
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
    if question.allow_custom {
        if app.mode == Mode::QuestionCustom {
            lines.push(Line::from(vec![
                Span::styled("  c ", accent.add_modifier(Modifier::BOLD)),
                Span::styled("> ", accent),
                Span::raw(app.custom_answer.clone()),
            ]));
        } else if !custom.trim().is_empty() {
            lines.push(Line::from(vec![
                Span::styled("  c ", accent.add_modifier(Modifier::BOLD)),
                Span::styled("(•) ", accent),
                Span::raw(custom.clone()),
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

    if app.mode == Mode::QuestionCustom {
        let row = 1 + question.options.len() as u16;
        let x = inner.x + 6 + app.custom_answer.chars().count() as u16;
        if row < inner.height {
            frame.set_cursor_position((
                x.min(inner.x + inner.width.saturating_sub(1)),
                inner.y + row,
            ));
        }
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
        "press i to write"
    };
    let (lines, cursor) = app.composer.render(text_area, placeholder);
    frame.render_widget(Paragraph::new(lines), text_area);
    if insert {
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
        (Mode::Question | Mode::QuestionCustom, _) => (
            " ANSWER ",
            Style::default().bg(Color::Magenta).fg(Color::Black).bold(),
        ),
        (_, Focus::Sidebar) => (
            " THREADS ",
            Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
        ),
        _ => (
            " NORMAL ",
            Style::default().bg(Color::Blue).fg(Color::Black).bold(),
        ),
    };
    let mut spans = vec![Span::styled(mode_label, mode_style), Span::raw(" ")];
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
                spans.push(Span::styled(
                    format!("  {}", thread_status(shell)),
                    Style::default().fg(Color::DarkGray),
                ));
            }
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

fn draw_help(frame: &mut Frame, area: Rect) {
    let text = Text::from(vec![
        Line::from(Span::styled("Normal", Style::default().bold())),
        Line::from("  j/k  Ctrl-d/u  gg/G      scroll chat"),
        Line::from("  J/K                     next / previous thread"),
        Line::from("  /  or  Space            fuzzy thread picker"),
        Line::from("  Tab                     focus thread list (j/k, Enter, Esc)"),
        Line::from("  n                       new thread (pick project)"),
        Line::from("  m                       change model"),
        Line::from("  i / Enter               write a message"),
        Line::from("  za  zR  zM              toggle / expand all / collapse all tool groups"),
        Line::from("  1..9                    answer a pending approval"),
        Line::from(
            "  a                       answer the agent's question (digits, Space, c custom, Enter)",
        ),
        Line::from("  y                       yank last assistant message (OSC 52)"),
        Line::from("  s                       toggle sidebar"),
        Line::from("  Ctrl-c                  interrupt the running turn"),
        Line::from(""),
        Line::from(Span::styled("Insert", Style::default().bold())),
        Line::from("  Enter send · Alt-Enter / Ctrl-j newline · Esc normal"),
        Line::from("  Up/Down or Ctrl-p/n     prompt history"),
        Line::from("  Ctrl-w Ctrl-k Ctrl-u    kill word / to end / to start"),
        Line::from(""),
        Line::from(Span::styled("Commands", Style::default().bold())),
        Line::from("  :new [project]  :model  :effort [level]  :mode plan|default"),
        Line::from("  :perm full-access|auto|auto-accept-edits|approval-required"),
        Line::from("  :rename <title>  :rename (regenerate)  :archive  :delete!"),
        Line::from("  :stop  :older  :answer  :dismiss  :sidebar  :help  :q"),
        Line::from(""),
        Line::from(Span::styled(
            "  press Esc to close",
            Style::default().fg(Color::DarkGray),
        )),
    ]);
    let width = 72.min(area.width);
    let height = (text.lines.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_style(Style::default().fg(Color::Blue))
        .title(" keys ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(text), inner);
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

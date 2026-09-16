//! Application state, key handling, and the main event loop.

use std::{
    collections::{HashMap, HashSet},
    io::Write,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::layout::{Position, Rect};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{
    commands,
    composer::Composer,
    model::{Id, ModelSelection, ServerConfig, ShellItem, ThreadDetailSnapshot, ThreadItem},
    question::QuestionDraft,
    session::{self, Handle, Status, Update},
    state::{ApprovalOption, PendingApproval, Shell, ThreadState},
    ui, vim,
};

/// Size used when restarting a terminal; the desktop app resizes when it attaches.
const TERMINAL_COLS: u16 = 120;
const TERMINAL_ROWS: u16 = 30;

/// The terminal `gl` reuses, one per thread, so the git command keeps its place.
const GIT_TERMINAL_ID: &str = "tria-git";

/// The terminal `g!` reuses, one per thread.
const SHELL_TERMINAL_ID: &str = "tria-shell";

/// Draft key for a thread that does not exist yet.
const NEW_THREAD_DRAFT_KEY: &str = "\0new-thread";

/// Rows moved per mouse wheel notch.
const MOUSE_SCROLL_LINES: usize = 3;
const TICK: Duration = Duration::from_millis(120);
const TOAST_TTL: Duration = Duration::from_secs(6);
const PREFIX_TTL: Duration = Duration::from_millis(1200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Command,
    Picker,
    Help,
    /// Answering an agent question: digits pick options, Enter advances.
    Question,
    /// Typing a free-text answer to the current question.
    QuestionCustom,
    /// Typing a `/` or `?` search over the chat; the cursor follows the first match.
    Search,
    /// Listing the agent's background tasks that have not reported an end.
    Tasks,
    /// Listing the thread's terminal sessions, with close and restart.
    Terminals,
    /// Attached to a terminal: keys go to the shell, `Ctrl-\` comes back.
    TerminalPane,
}

/// An accepted chat search, reused by `n` and `N` and for highlighting.
#[derive(Debug, Clone)]
pub struct Search {
    pub query: String,
    pub backward: bool,
}

/// A search being typed: the query so far, its direction, and where the cursor was.
#[derive(Debug, Clone)]
pub struct SearchInput {
    pub query: String,
    pub backward: bool,
    origin: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// Keys edit the composer with Vim motions; the chat scrolls with Ctrl keys.
    Composer,
    /// A line cursor moves through the conversation.
    Chat,
    Sidebar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    PullRequest,
    Thread,
    Model,
    Project,
    Effort,
}

#[derive(Debug, Clone)]
pub struct PickerItem {
    pub label: String,
    pub detail: String,
    pub key: String,
}

#[derive(Debug)]
pub struct Picker {
    pub kind: PickerKind,
    pub query: String,
    pub selected: usize,
    pub items: Vec<PickerItem>,
}

impl Picker {
    pub fn filtered(&self) -> Vec<&PickerItem> {
        let query = self.query.to_lowercase();
        let mut scored: Vec<(i64, &PickerItem)> = self
            .items
            .iter()
            .filter_map(|item| fuzzy_score(&query, &item.label, &item.detail).map(|s| (s, item)))
            .collect();
        if !query.is_empty() {
            scored.sort_by(|a, b| b.0.cmp(&a.0));
        }
        scored.into_iter().map(|(_, item)| item).collect()
    }
}

fn fuzzy_score(query: &str, label: &str, detail: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let haystack = format!("{} {}", label.to_lowercase(), detail.to_lowercase());
    if let Some(pos) = haystack.find(query) {
        return Some(1000 - pos as i64);
    }
    // Subsequence match for typo-tolerant filtering.
    let mut chars = haystack.chars();
    let mut score = 0i64;
    for q in query.chars() {
        loop {
            let c = chars.next()?;
            score -= 1;
            if c == q {
                break;
            }
        }
    }
    Some(score)
}

/// A thread being composed that does not exist on the server yet.
#[derive(Debug, Clone)]
pub struct NewThreadDraft {
    pub project_id: Id,
    pub model_selection: ModelSelection,
    pub runtime_mode: String,
    pub interaction_mode: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    Follow,
    Offset(usize),
}

pub enum AppEvent {
    Terminal(Event),
    Tick,
    Update(Box<Update>),
    Dispatched(Result<(), String>),
    /// A non-command RPC finished; `ok` is the toast for the success case.
    Called {
        result: Result<(), String>,
        ok: String,
    },
}

pub struct App {
    pub handle: Handle,
    events: mpsc::UnboundedSender<AppEvent>,
    pub shell: Shell,
    pub config: ServerConfig,
    pub status: Status,
    pub thread: Option<ThreadState>,
    pub current_thread_id: Option<Id>,
    pub draft: Option<NewThreadDraft>,
    pub mode: Mode,
    pub focus: Focus,
    pub composer: Composer,
    pub command_line: String,
    pub picker: Option<Picker>,
    pub question: Option<QuestionDraft>,
    pub custom_answer: String,
    pub sidebar_visible: bool,
    /// Index into `sidebar_rows()`.
    pub sidebar_selected: usize,
    pub show_settled: bool,
    pub show_snoozed: bool,
    pub scroll: Scroll,
    pub expanded: HashSet<String>,
    pub expand_all: bool,
    pub toast: Option<(String, Instant, bool)>,
    pub spinner: usize,
    pending_prefix: Option<(char, Instant)>,
    /// Filled by the renderer each frame so key handling can page correctly.
    pub chat_viewport: (usize, usize),
    /// First visible sidebar row; the renderer reads and clamps it.
    pub sidebar_offset: usize,
    /// Set by keyboard navigation so the renderer scrolls the selection into view.
    pub sidebar_reveal: bool,
    /// Screen regions from the last frame, for mouse hit testing.
    pub sidebar_inner: Option<Rect>,
    pub chat_area: Rect,
    /// Line cursor in the chat, as a content line index. Tracks the last line while the
    /// view follows new output.
    pub chat_cursor: usize,
    /// Anchor line of a linewise visual selection in the chat.
    pub chat_visual: Option<usize>,
    /// Count typed before a chat motion.
    chat_count: Option<usize>,
    /// First content line of every message block, for `{` and `}`.
    pub message_starts: Vec<usize>,
    pub search: Option<Search>,
    pub search_input: Option<SearchInput>,
    /// Unsent composer text per thread, keyed by thread id, so switching threads keeps a
    /// half-written message where it belongs. New-thread drafts use `NEW_THREAD_DRAFT_KEY`.
    drafts: HashMap<String, String>,
    /// Every terminal session the server knows about, across threads.
    pub terminals: Vec<crate::model::TerminalSummary>,
    /// Selection in the terminal panel.
    pub terminal_selected: usize,
    /// The attached terminal, when one is open.
    pub pane: Option<crate::term::Pane>,
    /// A command to type into the pane once its shell reports for duty, for `gl`.
    pending_pane_command: Option<String>,
    /// Command for `gl` and `:git`, from the config file.
    pub git_command: String,
    /// Editor for `ge` and `gE`.
    pub editor: String,
    /// A program to run in the terminal in tria's place; the event loop picks it up.
    pub pending_external: Option<ExternalCommand>,
    /// A tmux popup still running, with what to do when it closes.
    popup: Option<(std::process::Child, Option<FollowUp>)>,
    /// Content-line range of every block with its export key.
    pub block_ranges: Vec<(usize, usize, String)>,
    /// Mouse selection in the chat, in screen cells.
    pub selection: Option<Selection>,
    /// Text to push to the clipboard after the next frame is drawn.
    pub clipboard_pending: Option<String>,
    pub work_ranges: Vec<(usize, usize, String)>,
    quit: bool,
}

impl App {
    pub fn new(handle: Handle, events: mpsc::UnboundedSender<AppEvent>) -> Self {
        Self {
            handle,
            events,
            shell: Shell::default(),
            config: ServerConfig::default(),
            status: Status::Connecting,
            thread: None,
            current_thread_id: None,
            draft: None,
            mode: Mode::Normal,
            focus: Focus::Composer,
            composer: Composer::new(),
            command_line: String::new(),
            picker: None,
            question: None,
            custom_answer: String::new(),
            sidebar_visible: true,
            sidebar_selected: 0,
            show_settled: false,
            show_snoozed: true,
            scroll: Scroll::Follow,
            expanded: HashSet::new(),
            expand_all: false,
            toast: None,
            spinner: 0,
            pending_prefix: None,
            chat_viewport: (0, 0),
            sidebar_offset: 0,
            sidebar_reveal: false,
            sidebar_inner: None,
            chat_area: Rect::default(),
            chat_cursor: 0,
            chat_visual: None,
            chat_count: None,
            message_starts: Vec::new(),
            search: None,
            search_input: None,
            terminals: Vec::new(),
            terminal_selected: 0,
            pane: None,
            pending_pane_command: None,
            drafts: HashMap::new(),
            git_command: crate::config::DEFAULT_GIT_COMMAND.to_string(),
            editor: "nvim".to_string(),
            pending_external: None,
            popup: None,
            block_ranges: Vec::new(),
            selection: None,
            clipboard_pending: None,
            work_ranges: Vec::new(),
            quit: false,
        }
    }

    pub fn toast(&mut self, message: impl Into<String>, is_error: bool) {
        self.toast = Some((message.into(), Instant::now(), is_error));
    }

    pub fn spinner_frame(&self) -> &'static str {
        const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
        FRAMES[self.spinner % FRAMES.len()]
    }

    // ── Sidebar model ──────────────────────────────────────────────────

    /// Threads reachable with J/K and the picker: pinned and active, plus the parked
    /// sections when they are expanded.
    pub fn visible_threads(&self) -> Vec<Id> {
        let now = commands::now_iso();
        let sections = self.shell.sections(&now);
        let mut ids: Vec<Id> = sections
            .pinned
            .iter()
            .chain(&sections.active)
            .map(|t| t.id.clone())
            .collect();
        if self.show_snoozed {
            ids.extend(sections.snoozed.iter().map(|t| t.id.clone()));
        }
        if self.show_settled {
            ids.extend(sections.settled.iter().map(|t| t.id.clone()));
        }
        ids
    }

    /// Rows of the sidebar list, including section headers.
    pub fn sidebar_rows(&self) -> Vec<SidebarRow> {
        let now = commands::now_iso();
        let sections = self.shell.sections(&now);
        let mut rows = Vec::new();
        if !sections.pinned.is_empty() {
            rows.push(SidebarRow::Header {
                section: Section::Pinned,
                count: sections.pinned.len(),
                collapsed: false,
            });
            rows.extend(sections.pinned.iter().map(|t| SidebarRow::Thread {
                id: t.id.clone(),
                parked: false,
            }));
        }
        rows.push(SidebarRow::Header {
            section: Section::Active,
            count: sections.active.len(),
            collapsed: false,
        });
        rows.extend(sections.active.iter().map(|t| SidebarRow::Thread {
            id: t.id.clone(),
            parked: false,
        }));
        if !sections.snoozed.is_empty() {
            rows.push(SidebarRow::Header {
                section: Section::Snoozed,
                count: sections.snoozed.len(),
                collapsed: !self.show_snoozed,
            });
            if self.show_snoozed {
                rows.extend(sections.snoozed.iter().map(|t| SidebarRow::Thread {
                    id: t.id.clone(),
                    parked: true,
                }));
            }
        }
        if !sections.settled.is_empty() {
            rows.push(SidebarRow::Header {
                section: Section::Settled,
                count: sections.settled.len(),
                collapsed: !self.show_settled,
            });
            if self.show_settled {
                rows.extend(sections.settled.iter().map(|t| SidebarRow::Thread {
                    id: t.id.clone(),
                    parked: true,
                }));
            }
        }
        rows
    }

    fn select_sidebar_thread(&mut self, thread_id: &str) {
        if let Some(index) = self
            .sidebar_rows()
            .iter()
            .position(|row| matches!(row, SidebarRow::Thread { id, .. } if id == thread_id))
        {
            self.sidebar_selected = index;
            self.sidebar_reveal = true;
        }
    }

    fn toggle_section(&mut self, section: Section) {
        match section {
            Section::Settled => self.show_settled = !self.show_settled,
            Section::Snoozed => self.show_snoozed = !self.show_snoozed,
            Section::Pinned | Section::Active => {}
        }
    }

    fn sidebar_activate(&mut self) {
        let rows = self.sidebar_rows();
        match rows.get(self.sidebar_selected) {
            Some(SidebarRow::Thread { id, .. }) => {
                let id = id.clone();
                self.open_thread(&id);
                self.focus = Focus::Composer;
            }
            Some(SidebarRow::Header { section, .. }) => self.toggle_section(*section),
            None => {}
        }
    }

    // ── Navigation ─────────────────────────────────────────────────────

    /// Key under which the composer text of the current view is stashed.
    fn draft_key(&self) -> Option<String> {
        if self.draft.is_some() {
            Some(NEW_THREAD_DRAFT_KEY.to_string())
        } else {
            self.current_thread_id.clone()
        }
    }

    /// Park the composer text for the current thread and load the target's, if any.
    fn swap_composer_draft(&mut self, target: &str) {
        if let Some(key) = self.draft_key() {
            let text = self.composer.text();
            if text.trim().is_empty() {
                self.drafts.remove(&key);
            } else {
                self.drafts.insert(key, text);
            }
        }
        self.composer.clear();
        if let Some(text) = self.drafts.get(target) {
            let text = text.clone();
            self.composer.set_text(&text);
            self.composer.leave_insert();
        }
    }

    pub fn open_thread(&mut self, thread_id: &str) {
        if self.current_thread_id.as_deref() == Some(thread_id) && self.draft.is_none() {
            return;
        }
        self.swap_composer_draft(thread_id);
        self.current_thread_id = Some(thread_id.to_string());
        self.thread = None;
        self.draft = None;
        self.question = None;
        if matches!(self.mode, Mode::Question | Mode::QuestionCustom) {
            self.mode = Mode::Normal;
        }
        self.scroll = Scroll::Follow;
        self.expanded.clear();
        self.handle.open_thread(thread_id);
        if let Some(thread) = self.shell.threads.get(thread_id) {
            if thread.is_settled() {
                self.show_settled = true;
            } else if thread.is_snoozed(&commands::now_iso()) {
                self.show_snoozed = true;
            }
        }
        self.select_sidebar_thread(thread_id);
    }

    fn open_relative(&mut self, delta: isize) {
        let threads = self.visible_threads();
        if threads.is_empty() {
            return;
        }
        let current = self
            .current_thread_id
            .as_ref()
            .and_then(|id| threads.iter().position(|t| t == id))
            .map(|i| i as isize)
            .unwrap_or(-1);
        let next = (current + delta).clamp(0, threads.len() as isize - 1) as usize;
        let id = threads[next].clone();
        self.open_thread(&id);
    }

    fn start_new_thread(&mut self, project_id: &str) {
        let project = self.shell.projects.get(project_id);
        let model_selection = project
            .and_then(|p| p.default_model_selection.clone())
            .or_else(|| self.config.settings.default_model_selection.clone())
            .or_else(|| self.first_usable_model());
        let Some(model_selection) = model_selection else {
            self.toast("no usable provider or model configured on the server", true);
            return;
        };
        self.swap_composer_draft(NEW_THREAD_DRAFT_KEY);
        self.draft = Some(NewThreadDraft {
            project_id: project_id.to_string(),
            model_selection,
            runtime_mode: self
                .config
                .settings
                .default_runtime_mode
                .clone()
                .unwrap_or_else(|| "full-access".into()),
            interaction_mode: "default".into(),
        });
        self.current_thread_id = None;
        self.thread = None;
        self.handle.close_thread();
        self.scroll = Scroll::Follow;
        self.mode = Mode::Insert;
        self.focus = Focus::Composer;
    }

    fn first_usable_model(&self) -> Option<ModelSelection> {
        self.config
            .providers
            .iter()
            .filter(|p| p.is_usable())
            .find_map(|provider| {
                let model = provider
                    .models
                    .iter()
                    .find(|m| m.is_default)
                    .or_else(|| provider.models.first())?;
                Some(ModelSelection {
                    instance_id: provider.instance_id.clone(),
                    model: model.slug.clone(),
                    options: vec![],
                })
            })
    }

    // ── Dispatch ───────────────────────────────────────────────────────

    fn dispatch(&self, command: Value) {
        let handle = self.handle.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let result = handle
                .dispatch(command)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string());
            let _ = events.send(AppEvent::Dispatched(result));
        });
    }

    fn send_message(&mut self) {
        let text = self.composer.text().trim_end().to_string();
        if text.trim().is_empty() {
            return;
        }
        // Read the slot before sending: starting a new thread moves the view to it, and the
        // parked text belongs to the slot the message was written in.
        let draft_key = self.draft_key();
        if let Some(draft) = self.draft.take() {
            let thread_id = commands::new_id();
            let title: String = text
                .lines()
                .next()
                .unwrap_or("New thread")
                .chars()
                .take(60)
                .collect();
            let command = commands::turn_start(
                &thread_id,
                &text,
                &draft.model_selection,
                &draft.runtime_mode,
                &draft.interaction_mode,
                Some(commands::NewThread {
                    project_id: &draft.project_id,
                    title: title.trim(),
                    model_selection: &draft.model_selection,
                    runtime_mode: &draft.runtime_mode,
                    interaction_mode: &draft.interaction_mode,
                }),
            );
            self.dispatch(command);
            self.current_thread_id = Some(thread_id.clone());
            self.thread = None;
            self.handle.open_thread(&thread_id);
        } else if let Some(thread) = &self.thread {
            let shell = &thread.detail.shell;
            let command = commands::turn_start(
                thread.id(),
                &text,
                &shell.model_selection,
                &shell.runtime_mode,
                &shell.interaction_mode,
                None,
            );
            self.dispatch(command);
        } else {
            self.toast(
                "no thread open; press n for a new thread or / to pick one",
                true,
            );
            return;
        }
        self.composer.push_history(text);
        self.composer.clear();
        if let Some(key) = draft_key {
            self.drafts.remove(&key);
        }
        self.scroll = Scroll::Follow;
    }

    fn interrupt(&mut self) {
        let Some(thread) = &self.thread else { return };
        if !thread.is_running() {
            self.toast("nothing running", false);
            return;
        }
        let turn_id = thread
            .detail
            .shell
            .latest_turn
            .as_ref()
            .map(|t| t.turn_id.clone());
        self.dispatch(commands::turn_interrupt(thread.id(), turn_id.as_deref()));
        self.toast("interrupting…", false);
    }

    fn respond_approval(&mut self, index: usize) {
        let Some(thread) = &self.thread else { return };
        let pending = thread.pending_approvals();
        let Some(approval) = pending.first() else {
            return;
        };
        let options = approval_options(approval);
        let Some(option) = options.get(index) else {
            return;
        };
        let command =
            commands::approval_respond(thread.id(), &approval.request_id, &option.decision);
        self.dispatch(command);
        self.toast(option.label.to_string(), false);
    }

    // ── Agent questions ────────────────────────────────────────────────

    /// Enter question mode for the pending question, if any.
    fn begin_answering(&mut self) -> bool {
        let Some(pending) = self.thread.as_ref().and_then(|t| t.pending_user_input()) else {
            return false;
        };
        let stale = self
            .question
            .as_ref()
            .is_none_or(|q| q.request_id != pending.request_id);
        if stale {
            self.question = Some(QuestionDraft::new(&pending));
        }
        self.mode = Mode::Question;
        true
    }

    /// Called after every thread change: keep the draft in step with the pending question.
    fn reconcile_question(&mut self) {
        let pending = self.thread.as_ref().and_then(|t| t.pending_user_input());
        match (&self.question, &pending) {
            (Some(draft), Some(pending)) if draft.request_id == pending.request_id => {}
            (Some(_), _) => {
                // Answered elsewhere or superseded: drop the draft.
                self.question = None;
                if matches!(self.mode, Mode::Question | Mode::QuestionCustom) {
                    self.mode = Mode::Normal;
                }
            }
            (None, Some(_)) => {
                // A new question arrived. Open it unless the user is mid-typing.
                let idle = matches!(self.mode, Mode::Normal)
                    || (self.mode == Mode::Insert && self.composer.is_empty());
                if idle {
                    self.begin_answering();
                } else {
                    self.toast("the agent asked a question · press a to answer", false);
                }
            }
            (None, None) => {}
        }
    }

    fn submit_answers(&mut self) {
        let Some(draft) = self.question.as_mut() else {
            return;
        };
        match draft.advance() {
            Some(answers) => {
                let request_id = draft.request_id.clone();
                let Some(thread_id) = self.thread.as_ref().map(|t| t.id().to_string()) else {
                    return;
                };
                self.dispatch(commands::user_input_respond(
                    &thread_id,
                    &request_id,
                    answers,
                ));
                self.mode = Mode::Normal;
                self.toast("answers sent", false);
            }
            None => {
                if !draft.is_answered(draft.index) {
                    self.toast("pick an option or type a custom answer (c)", false);
                }
            }
        }
    }

    fn dismiss_question(&mut self) {
        let Some(thread) = &self.thread else { return };
        let Some(pending) = thread.pending_user_input() else {
            return;
        };
        if !pending.dismissible {
            self.toast(
                "this question blocks the agent and cannot be dismissed; answer it or Ctrl-c",
                true,
            );
            return;
        }
        self.dispatch(commands::user_input_dismiss(
            thread.id(),
            &pending.request_id,
        ));
        self.question = None;
        self.mode = Mode::Normal;
    }

    fn on_question_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(draft) = self.question.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Char(c @ '1'..='9') => {
                let index = c as usize - '1' as usize;
                let single = !draft.current().multi_select;
                draft.choose(index);
                if single && draft.current().options.len() > index {
                    self.submit_answers();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => draft.move_highlight(1),
            KeyCode::Char('k') | KeyCode::Up => draft.move_highlight(-1),
            KeyCode::Char('n') if ctrl => draft.move_highlight(1),
            KeyCode::Char('p') if ctrl => draft.move_highlight(-1),
            KeyCode::Char(' ') | KeyCode::Char('x') => {
                let highlight = draft.highlight;
                draft.choose(highlight);
            }
            KeyCode::Enter => {
                if !draft.is_answered(draft.index) && !draft.current().options.is_empty() {
                    let highlight = draft.highlight;
                    draft.choose(highlight);
                }
                self.submit_answers();
            }
            KeyCode::Char('h') | KeyCode::Left | KeyCode::BackTab => draft.back(),
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => {
                if draft.is_answered(draft.index) && !draft.is_last() {
                    draft.index += 1;
                    draft.highlight = 0;
                }
            }
            KeyCode::Char('c') | KeyCode::Char('i') | KeyCode::Char('/') => {
                if draft.current().allow_custom {
                    self.custom_answer = draft.current_answer().custom.clone();
                    self.mode = Mode::QuestionCustom;
                } else {
                    self.toast("this question does not accept a custom answer", false);
                }
            }
            KeyCode::Char('d') => self.dismiss_question(),
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
    }

    fn on_question_custom_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.custom_answer.clear();
                self.mode = Mode::Question;
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.custom_answer);
                if let Some(draft) = self.question.as_mut() {
                    draft.set_custom(text);
                }
                self.mode = Mode::Question;
                self.submit_answers();
            }
            KeyCode::Backspace => {
                self.custom_answer.pop();
            }
            KeyCode::Char('u') if ctrl => self.custom_answer.clear(),
            KeyCode::Char('w') if ctrl => {
                let trimmed = self.custom_answer.trim_end().to_string();
                let cut = trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0);
                self.custom_answer.truncate(cut);
            }
            KeyCode::Char(c) if !ctrl => self.custom_answer.push(c),
            _ => {}
        }
    }

    fn yank_last_assistant(&mut self) {
        let Some(thread) = &self.thread else { return };
        let Some(message) = thread
            .detail
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
        else {
            self.toast("no assistant message to yank", true);
            return;
        };
        let text = message.text.clone();
        copy_to_clipboard(&text);
        self.toast("yanked last assistant message to clipboard", false);
    }

    // ── Settling ───────────────────────────────────────────────────────

    /// `gs`: park an active thread on the settled shelf, or bring a settled one back.
    fn toggle_settled(&mut self, thread_id: Option<String>) {
        let Some(id) = thread_id else {
            self.toast("no thread selected", true);
            return;
        };
        let Some(shell) = self.shell.threads.get(&id) else {
            return;
        };
        let title = shell.title.clone();
        if shell.is_settled() {
            self.dispatch(commands::unsettle(&id));
            self.toast(format!("un-settled: {title}"), false);
        } else {
            self.dispatch(commands::simple("thread.settle", &id));
            self.toast(format!("settled: {title}"), false);
        }
    }

    /// Thread under the sidebar selection, when a thread row is selected.
    fn sidebar_selected_thread(&self) -> Option<String> {
        match self.sidebar_rows().get(self.sidebar_selected) {
            Some(SidebarRow::Thread { id, .. }) => Some(id.clone()),
            _ => None,
        }
    }

    fn on_terminals_key(&mut self, key: KeyEvent) {
        let count = self.thread_terminals().len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down => {
                self.terminal_selected = (self.terminal_selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.terminal_selected = self.terminal_selected.saturating_sub(1);
            }
            KeyCode::Char('g') => self.terminal_selected = 0,
            KeyCode::Char('G') => self.terminal_selected = count.saturating_sub(1),
            KeyCode::Char('x') | KeyCode::Char('d') => self.close_terminal(),
            KeyCode::Char('r') => self.restart_terminal(),
            KeyCode::Char('c') => self.new_terminal(),
            KeyCode::Enter | KeyCode::Char('a') | KeyCode::Char('l') => self.attach_terminal(),
            _ => {}
        }
    }

    // ── Terminals ──────────────────────────────────────────────────────

    fn apply_terminal_event(&mut self, event: crate::model::TerminalEvent) {
        use crate::model::TerminalEvent;
        match event {
            TerminalEvent::Snapshot { terminals } => self.terminals = terminals,
            TerminalEvent::Upsert { terminal } => {
                match self.terminals.iter_mut().find(|t| {
                    t.terminal_id == terminal.terminal_id && t.thread_id == terminal.thread_id
                }) {
                    Some(existing) => *existing = terminal,
                    None => self.terminals.push(terminal),
                }
            }
            TerminalEvent::Remove {
                thread_id,
                terminal_id,
            } => self
                .terminals
                .retain(|t| !(t.thread_id == thread_id && t.terminal_id == terminal_id)),
            TerminalEvent::Unknown => {}
        }
    }

    /// The open thread's terminals, live ones first.
    pub fn thread_terminals(&self) -> Vec<&crate::model::TerminalSummary> {
        let Some(id) = self.current_thread_id.as_deref() else {
            return Vec::new();
        };
        let mut list: Vec<&crate::model::TerminalSummary> = self
            .terminals
            .iter()
            .filter(|t| t.thread_id == id)
            .collect();
        list.sort_by_key(|t| (!t.is_live(), t.terminal_id.clone()));
        list
    }

    fn open_terminals(&mut self) {
        if self.thread.is_none() {
            self.toast("no thread open", true);
            return;
        }
        self.terminal_selected = 0;
        self.mode = Mode::Terminals;
    }

    fn selected_terminal(&self) -> Option<(String, String, String)> {
        self.thread_terminals()
            .get(self.terminal_selected)
            .map(|t| (t.thread_id.clone(), t.terminal_id.clone(), t.cwd.clone()))
    }

    /// Close the selected terminal. The server kills whatever is running in it.
    fn close_terminal(&mut self) {
        let Some((thread_id, terminal_id, _)) = self.selected_terminal() else {
            self.toast("no terminal selected", true);
            return;
        };
        self.call(
            "terminal.close",
            json!({"threadId": thread_id, "terminalId": terminal_id}),
            format!("closed {terminal_id}"),
        );
    }

    /// Restart the selected terminal: the shell is replaced, so the running command dies.
    fn restart_terminal(&mut self) {
        let Some((thread_id, terminal_id, cwd)) = self.selected_terminal() else {
            self.toast("no terminal selected", true);
            return;
        };
        self.call(
            "terminal.restart",
            json!({
                "threadId": thread_id,
                "terminalId": terminal_id,
                "cwd": cwd,
                "cols": TERMINAL_COLS,
                "rows": TERMINAL_ROWS,
            }),
            format!("restarted {terminal_id}"),
        );
    }

    /// Open a new shell for the thread and attach to it.
    fn new_terminal(&mut self) {
        let Some(thread_id) = self.current_thread_id.clone() else {
            self.toast("no thread open", true);
            return;
        };
        let Some(cwd) = self.thread_directory() else {
            self.toast("thread has no directory", true);
            return;
        };
        let terminal_id = format!("tria-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let (cols, rows) = self.pane_size();
        let handle = self.handle.clone();
        let events = self.events.clone();
        let payload = json!({
            "threadId": thread_id,
            "terminalId": terminal_id,
            "cwd": cwd,
            "cols": cols,
            "rows": rows,
        });
        tokio::spawn(async move {
            let result = handle
                .call("terminal.open", payload)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string());
            let _ = events.send(AppEvent::Called {
                result,
                ok: format!("opened {terminal_id}"),
            });
        });
    }

    /// Attach to the selected terminal and hand the keyboard to it.
    fn attach_terminal(&mut self) {
        let Some(terminal) = self
            .thread_terminals()
            .get(self.terminal_selected)
            .map(|t| (*t).clone())
        else {
            self.toast("no terminal selected", true);
            return;
        };
        self.open_pane(
            terminal.thread_id,
            terminal.terminal_id,
            terminal.label,
            terminal.cwd,
        );
    }

    /// Attach and show the pane. The server opens or revives the terminal as needed.
    fn open_pane(&mut self, thread_id: String, terminal_id: String, label: String, cwd: String) {
        let (cols, rows) = self.pane_size();
        self.handle
            .attach_terminal(&thread_id, &terminal_id, &cwd, cols, rows);
        self.pane = Some(crate::term::Pane::new(
            thread_id,
            terminal_id,
            label,
            cols,
            rows,
        ));
        self.mode = Mode::TerminalPane;
    }

    /// Leave the pane. The shell keeps running; the server just stops streaming here.
    fn detach_terminal(&mut self) {
        self.handle.detach_terminal();
        self.pane = None;
        self.mode = Mode::Normal;
    }

    /// The size the pane will be drawn at, for the initial attach and for `terminal.open`.
    fn pane_size(&self) -> (u16, u16) {
        match &self.pane {
            Some(pane) => pane.size(),
            None => (
                self.chat_area.width.max(20),
                (self.chat_area.height + 4).max(10),
            ),
        }
    }

    fn apply_terminal_stream(&mut self, event: crate::model::TerminalStreamEvent) {
        use crate::model::TerminalStreamEvent as Event;
        let Some(pane) = self.pane.as_mut() else {
            return;
        };
        match event {
            Event::Snapshot { snapshot } | Event::Restarted { snapshot } => {
                pane.label = snapshot.label;
                pane.reset(&snapshot.history);
                let terminal_id = pane.terminal_id.clone();
                // Only type the queued command into a shell that is sitting idle: an
                // existing session already running it should be left alone.
                if let Some(command) = self.pending_pane_command.take()
                    && !self.terminal_busy(&terminal_id)
                {
                    self.write_to_pane(command);
                }
            }
            Event::Output { data } => {
                pane.feed(&data);
                // A full-screen program is up, so there is nothing of the shell left to hide.
                if pane.alternate_screen() {
                    pane.starting = None;
                }
            }
            Event::Cleared => pane.reset(""),
            // The program the pane was opened for is gone, so the pane goes with it.
            Event::Exited { .. } => {
                let label = pane.label.clone();
                let terminal_id = pane.terminal_id.clone();
                let thread_id = pane.thread_id.clone();
                self.detach_terminal();
                self.toast(format!("{label} exited"), false);
                // Sessions tria opened are scratch; leave the desktop app's own alone.
                if terminal_id.starts_with("tria-") {
                    self.call(
                        "terminal.close",
                        json!({"threadId": thread_id, "terminalId": terminal_id}),
                        String::new(),
                    );
                }
            }
            Event::Closed => {
                self.toast("terminal closed", false);
                self.detach_terminal();
            }
            Event::Activity {
                has_running_subprocess,
                label,
            } => {
                pane.label = label;
                if has_running_subprocess {
                    pane.starting = None;
                }
            }
            Event::Error { message } => self.toast(message, true),
            Event::Unknown => {}
        }
    }

    /// Keys while attached: everything goes to the shell except the detach key.
    fn on_pane_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl-\, which a terminal without the kitty protocol reports as Ctrl-4.
        if ctrl && matches!(key.code, KeyCode::Char('\\') | KeyCode::Char('4')) {
            self.detach_terminal();
            return;
        }
        let Some(pane) = self.pane.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        // Typing returns to the live screen, the way a terminal behaves.
        if pane.scrollback() > 0 {
            pane.scroll(isize::MIN / 2);
        }
        let app_cursor = pane.screen().application_cursor();
        let Some(data) = crate::term::encode_key(&key, app_cursor) else {
            return;
        };
        self.write_to_pane(data);
    }

    /// Send bytes to the attached terminal.
    fn write_to_pane(&self, data: String) {
        let Some(pane) = self.pane.as_ref() else {
            return;
        };
        let payload = json!({
            "threadId": pane.thread_id,
            "terminalId": pane.terminal_id,
            "data": data,
        });
        let handle = self.handle.clone();
        tokio::spawn(async move {
            let _ = handle.call("terminal.write", payload).await;
        });
    }

    /// Whether that terminal has a command running, as the metadata stream last said.
    fn terminal_busy(&self, terminal_id: &str) -> bool {
        self.terminals
            .iter()
            .any(|t| t.terminal_id == terminal_id && t.has_running_subprocess)
    }

    /// Tell the server the pane's size after a redraw changed it.
    pub fn sync_pane_size(&mut self, cols: u16, rows: u16) {
        let Some(pane) = self.pane.as_mut() else {
            return;
        };
        if !pane.resize(cols, rows) {
            return;
        }
        let payload = json!({
            "threadId": pane.thread_id,
            "terminalId": pane.terminal_id,
            "cols": cols,
            "rows": rows,
        });
        let handle = self.handle.clone();
        tokio::spawn(async move {
            let _ = handle.call("terminal.resize", payload).await;
        });
    }

    /// Fire an RPC and toast the outcome.
    fn call(&self, tag: &str, payload: Value, ok: String) {
        let handle = self.handle.clone();
        let events = self.events.clone();
        let tag = tag.to_string();
        tokio::spawn(async move {
            let result = handle
                .call(&tag, payload)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string());
            let _ = events.send(AppEvent::Called { result, ok });
        });
    }

    // ── Background tasks ───────────────────────────────────────────────

    /// Tasks the agent started that have not reported an end: monitors and backgrounded
    /// commands, which outlive the turn that started them.
    pub fn running_tasks(&self) -> Vec<crate::state::RunningTask> {
        self.thread
            .as_ref()
            .map(|t| t.running_tasks())
            .unwrap_or_default()
    }

    fn open_tasks(&mut self) {
        if self.thread.is_none() {
            self.toast("no thread open", true);
            return;
        }
        self.mode = Mode::Tasks;
    }

    // ── Git popup ──────────────────────────────────────────────────────

    /// `gl` and `:git`: run the git command in the thread's directory. Inside tmux it opens
    /// as a popup over the pane and tria keeps running; elsewhere tria steps aside until
    /// the command exits.
    /// `gl` and `:git`: run the git command in the thread's own terminal, in the pane.
    /// Reuses one terminal per thread, so leaving and coming back finds it where it was.
    fn open_git(&mut self) {
        let Some(dir) = self.thread_directory() else {
            self.toast("no thread open", true);
            return;
        };
        let Some(thread_id) = self.current_thread_id.clone() else {
            self.toast("no thread open", true);
            return;
        };
        let command = self.git_command.clone();
        // `exec` replaces the shell, so quitting the command ends the session and the
        // popup closes with it. It also means `git_command` should be interactive.
        self.pending_pane_command = Some(format!("exec {command}\r"));
        self.open_pane(thread_id, GIT_TERMINAL_ID.to_string(), command.clone(), dir);
        if let Some(pane) = self.pane.as_mut() {
            pane.starting = Some(command);
        }
    }

    /// `g!` and `:shell`: a plain shell for the thread, in the pane. Exiting it closes
    /// the popup, so this is a scratch shell rather than something to keep around.
    fn open_shell(&mut self) {
        let Some(dir) = self.thread_directory() else {
            self.toast("no thread open", true);
            return;
        };
        let Some(thread_id) = self.current_thread_id.clone() else {
            self.toast("no thread open", true);
            return;
        };
        self.open_pane(
            thread_id,
            SHELL_TERMINAL_ID.to_string(),
            "shell".into(),
            dir,
        );
    }

    /// Run a program that needs the terminal. Inside tmux it goes into a popup and tria
    /// polls for it to close; elsewhere the event loop hands the terminal over.
    fn launch_external(&mut self, command: String, dir: String, then: Option<FollowUp>) {
        if std::env::var_os("TMUX").is_some() {
            if self.popup.is_some() {
                self.toast("a popup is already open", true);
                return;
            }
            let spawned = std::process::Command::new("tmux")
                .args([
                    "display-popup",
                    "-E",
                    "-d",
                    &dir,
                    "-w",
                    "90%",
                    "-h",
                    "90%",
                    &command,
                ])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            match spawned {
                Ok(child) => self.popup = Some((child, then)),
                Err(err) => self.toast(format!("could not run tmux: {err}"), true),
            }
            return;
        }
        self.pending_external = Some(ExternalCommand { command, dir, then });
    }

    /// Called on every tick: when the tmux popup has closed, run its follow-up.
    fn poll_popup(&mut self) {
        let done = match self.popup.as_mut() {
            Some((child, _)) => matches!(child.try_wait(), Ok(Some(_)) | Err(_)),
            None => return,
        };
        if done
            && let Some((_, then)) = self.popup.take()
            && let Some(then) = then
        {
            self.follow_up(then);
        }
    }

    pub fn follow_up(&mut self, then: FollowUp) {
        match then {
            FollowUp::LoadComposer(path) => {
                let text = match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(err) => {
                        self.toast(format!("could not read editor file: {err}"), true);
                        return;
                    }
                };
                let _ = std::fs::remove_file(&path);
                let text = text.strip_suffix('\n').unwrap_or(&text).to_string();
                if text == self.composer.text() {
                    return;
                }
                self.composer.checkpoint();
                self.composer.set_text(&text);
                self.composer.leave_insert();
                self.toast("composer updated from the editor", false);
            }
        }
    }

    // ── Editor ─────────────────────────────────────────────────────────

    /// The editor over a temp file; `-R` for editors that understand it when read only.
    fn editor_command(&self, path: &std::path::Path, read_only: bool) -> String {
        let editor = self.editor.clone();
        let program = editor
            .split_whitespace()
            .next()
            .and_then(|p| std::path::Path::new(p).file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let vimlike = matches!(program.as_str(), "vim" | "nvim" | "vi" | "view" | "gvim");
        let flag = if read_only && vimlike { " -R" } else { "" };
        format!("{editor}{flag} '{}'", path.display())
    }

    fn external_dir(&self) -> String {
        self.thread_directory().unwrap_or_else(|| {
            std::env::current_dir()
                .map(|d| d.display().to_string())
                .unwrap_or_else(|_| ".".to_string())
        })
    }

    /// `ge` in the composer: edit the draft in the editor; the file is read back on exit.
    fn edit_composer(&mut self) {
        let path = match write_temp_file("draft", "md", &self.composer.text()) {
            Ok(path) => path,
            Err(err) => {
                self.toast(format!("could not write temp file: {err}"), true);
                return;
            }
        };
        let command = self.editor_command(&path, false);
        let dir = self.external_dir();
        self.launch_external(command, dir, Some(FollowUp::LoadComposer(path)));
    }

    /// View text read only in the editor.
    fn view_in_editor(&mut self, name: &str, text: &str) {
        if text.trim().is_empty() {
            self.toast("nothing to show", true);
            return;
        }
        let path = match write_temp_file(name, "md", text) {
            Ok(path) => path,
            Err(err) => {
                self.toast(format!("could not write temp file: {err}"), true);
                return;
            }
        };
        let command = self.editor_command(&path, true);
        let dir = self.external_dir();
        self.launch_external(command, dir, None);
    }

    /// `gE`: the whole conversation as loaded.
    fn view_conversation(&mut self) {
        if self.thread.is_none() {
            self.toast("no thread open", true);
            return;
        }
        let text = ui::chat_export_all();
        self.view_in_editor("thread", &text);
    }

    /// `ge` in the chat: the message, plan, tool row, or tool group under the cursor.
    fn view_at_cursor(&mut self) {
        let line = self.chat_cursor;
        // A tool row is more specific than its group, which is more specific than a block.
        let key = self
            .work_ranges
            .iter()
            .filter(|(start, end, _)| *start <= line && line < *end)
            .min_by_key(|(start, end, _)| end - start)
            .map(|(_, _, key)| key.clone())
            .or_else(|| {
                self.block_ranges
                    .iter()
                    .find(|(start, end, _)| *start <= line && line < *end)
                    .map(|(_, _, key)| key.clone())
            });
        let Some(key) = key else {
            self.toast("nothing under the cursor", true);
            return;
        };
        match ui::chat_export(&key) {
            Some(text) => self.view_in_editor("block", &text),
            None => self.toast("nothing under the cursor", true),
        }
    }

    // ── tmux ───────────────────────────────────────────────────────────

    /// Directory the current thread works in: its worktree, else the project root.
    fn thread_directory(&self) -> Option<String> {
        let shell = self.thread.as_ref().map(|t| &t.detail.shell)?;
        shell.worktree_path.clone().or_else(|| {
            self.shell
                .projects
                .get(&shell.project_id)
                .map(|p| p.workspace_root.clone())
        })
    }

    /// `gt` and `:tmux`: switch the tmux client to the session named after the thread's
    /// directory, creating it there first when it does not exist.
    fn switch_tmux_session(&mut self) {
        if std::env::var_os("TMUX").is_none() {
            self.toast("not running inside tmux", true);
            return;
        }
        let Some(dir) = self.thread_directory() else {
            self.toast("no thread open", true);
            return;
        };
        let name = tmux_session_name(&dir);
        let exists = std::process::Command::new("tmux")
            .args(["has-session", "-t", &format!("={name}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !exists {
            let created = std::process::Command::new("tmux")
                .args(["new-session", "-d", "-s", &name, "-c", &dir])
                .output();
            match created {
                Ok(out) if out.status.success() => {}
                Ok(out) => {
                    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    self.toast(format!("tmux new-session failed: {err}"), true);
                    return;
                }
                Err(err) => {
                    self.toast(format!("could not run tmux: {err}"), true);
                    return;
                }
            }
        }
        let switched = std::process::Command::new("tmux")
            .args(["switch-client", "-t", &format!("={name}")])
            .output();
        match switched {
            Ok(out) if out.status.success() => self.toast(
                if exists {
                    format!("switched to tmux session {name}")
                } else {
                    format!("created tmux session {name}")
                },
                false,
            ),
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                self.toast(format!("tmux switch-client failed: {err}"), true);
            }
            Err(err) => self.toast(format!("could not run tmux: {err}"), true),
        }
    }

    // ── Pull requests ──────────────────────────────────────────────────

    /// `gx` and `:pr`: open the thread's pull request in the browser. With several linked
    /// pull requests, `:pr` offers a picker.
    fn open_pull_request(&mut self, pick: bool) {
        let Some(shell) = self.thread.as_ref().map(|t| &t.detail.shell) else {
            self.toast("no thread open", true);
            return;
        };
        let prs = shell.all_pull_requests();
        match prs.as_slice() {
            [] => self.toast("no pull request linked to this thread", true),
            [_] => self.open_url(&prs[0].url.clone()),
            _ if pick => self.open_picker(PickerKind::PullRequest),
            _ => self.open_url(&prs[0].url.clone()),
        }
    }

    fn open_url(&mut self, url: &str) {
        match open::that_detached(url) {
            Ok(()) => self.toast(format!("opened {url}"), false),
            Err(err) => self.toast(format!("could not open browser: {err}"), true),
        }
    }

    // ── Pickers ────────────────────────────────────────────────────────

    fn open_picker(&mut self, kind: PickerKind) {
        let items: Vec<PickerItem> = match kind {
            PickerKind::Thread => self
                .shell
                .sorted_threads(&commands::now_iso(), true)
                .iter()
                .map(|t| PickerItem {
                    label: t.title.clone(),
                    detail: format!(
                        "{} · {}",
                        self.shell.project_title(&t.project_id),
                        if t.is_settled() {
                            "settled"
                        } else {
                            t.status().label()
                        }
                    ),
                    key: t.id.clone(),
                })
                .collect(),
            PickerKind::Project => {
                let mut projects: Vec<_> = self.shell.projects.values().collect();
                projects.sort_by(|a, b| a.title.cmp(&b.title));
                projects
                    .into_iter()
                    .map(|p| PickerItem {
                        label: p.title.clone(),
                        detail: p.workspace_root.clone(),
                        key: p.id.clone(),
                    })
                    .collect()
            }
            PickerKind::Model => self
                .config
                .providers
                .iter()
                .filter(|p| p.is_usable())
                .flat_map(|p| {
                    p.models
                        .iter()
                        .filter(|m| !m.is_legacy)
                        .map(move |m| PickerItem {
                            label: format!("{} · {}", p.label(), m.name),
                            detail: m.slug.clone(),
                            key: format!("{}\t{}", p.instance_id, m.slug),
                        })
                })
                .collect(),
            PickerKind::PullRequest => self
                .thread
                .as_ref()
                .map(|t| t.detail.shell.all_pull_requests())
                .unwrap_or_default()
                .into_iter()
                .map(|pr| PickerItem {
                    label: pr.label(),
                    detail: pr.state.clone().unwrap_or_default(),
                    key: pr.url,
                })
                .collect(),
            PickerKind::Effort => {
                let Some(descriptor) = self.effort_descriptor() else {
                    self.toast("current model has no effort option", true);
                    return;
                };
                descriptor
                    .options
                    .iter()
                    .map(|o| PickerItem {
                        label: o.label.clone(),
                        detail: String::new(),
                        key: o.id.clone(),
                    })
                    .collect()
            }
        };
        if items.is_empty() {
            self.toast("nothing to pick from", true);
            return;
        }
        self.picker = Some(Picker {
            kind,
            query: String::new(),
            selected: 0,
            items,
        });
        self.mode = Mode::Picker;
    }

    fn current_model_selection(&self) -> Option<&ModelSelection> {
        self.draft.as_ref().map(|d| &d.model_selection).or_else(|| {
            self.thread
                .as_ref()
                .map(|t| &t.detail.shell.model_selection)
        })
    }

    fn effort_descriptor(&self) -> Option<&crate::model::OptionDescriptor> {
        let selection = self.current_model_selection()?;
        let provider = self
            .config
            .providers
            .iter()
            .find(|p| p.instance_id == selection.instance_id)?;
        let model = provider.models.iter().find(|m| m.slug == selection.model)?;
        model.option_descriptors().iter().find(|d| {
            d.kind == "select"
                && (d.id == "effort" || d.id == "reasoningEffort" || d.id == "variant")
        })
    }

    fn picker_select(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        let Some(item) = picker.filtered().get(picker.selected).map(|i| (*i).clone()) else {
            self.mode = Mode::Normal;
            return;
        };
        self.mode = Mode::Normal;
        match picker.kind {
            PickerKind::Thread => self.open_thread(&item.key),
            PickerKind::PullRequest => self.open_url(&item.key),
            PickerKind::Project => self.start_new_thread(&item.key),
            PickerKind::Model => {
                let (instance_id, slug) = item.key.split_once('\t').unwrap_or((&item.key, ""));
                let selection = ModelSelection {
                    instance_id: instance_id.into(),
                    model: slug.into(),
                    options: vec![],
                };
                self.set_model(selection);
            }
            PickerKind::Effort => {
                let Some(mut selection) = self.current_model_selection().cloned() else {
                    return;
                };
                let id = self
                    .effort_descriptor()
                    .map(|d| d.id.clone())
                    .unwrap_or_else(|| "effort".into());
                selection.options.retain(|o| o.id != id);
                selection.options.push(crate::model::OptionSelection {
                    id,
                    value: Value::String(item.key),
                });
                self.set_model(selection);
            }
        }
    }

    fn set_model(&mut self, selection: ModelSelection) {
        if let Some(draft) = self.draft.as_mut() {
            draft.model_selection = selection;
            self.mode = Mode::Insert;
        } else if let Some(thread) = &self.thread {
            self.dispatch(commands::meta_update_model(thread.id(), &selection));
        }
    }

    // ── Commands (`:`) ─────────────────────────────────────────────────

    fn run_command(&mut self, line: &str) {
        let line = line.trim();
        let (name, arg) = match line.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (line, ""),
        };
        let thread_id = self.thread.as_ref().map(|t| t.id().to_string());
        match name {
            "" => {}
            "q" | "quit" | "q!" => self.quit = true,
            "help" | "h" => self.mode = Mode::Help,
            "new" | "n" => {
                if arg.is_empty() {
                    self.open_picker(PickerKind::Project);
                } else {
                    let needle = arg.to_lowercase();
                    let found = self
                        .shell
                        .projects
                        .values()
                        .find(|p| p.title.to_lowercase().contains(&needle))
                        .map(|p| p.id.clone());
                    match found {
                        Some(id) => self.start_new_thread(&id),
                        None => self.toast(format!("no project matching {arg:?}"), true),
                    }
                }
            }
            "model" | "m" => self.open_picker(PickerKind::Model),
            "effort" | "e" => {
                if arg.is_empty() {
                    self.open_picker(PickerKind::Effort);
                } else {
                    let Some(mut selection) = self.current_model_selection().cloned() else {
                        return;
                    };
                    let id = self
                        .effort_descriptor()
                        .map(|d| d.id.clone())
                        .unwrap_or_else(|| "effort".into());
                    selection.options.retain(|o| o.id != id);
                    selection.options.push(crate::model::OptionSelection {
                        id,
                        value: Value::String(arg.into()),
                    });
                    self.set_model(selection);
                }
            }
            "mode" => match (arg, thread_id.as_deref()) {
                ("plan" | "default", Some(id)) => {
                    self.dispatch(commands::interaction_mode_set(id, arg))
                }
                ("plan" | "default", None) => {
                    if let Some(d) = self.draft.as_mut() {
                        d.interaction_mode = arg.into();
                    }
                }
                _ => self.toast("usage: :mode plan|default", true),
            },
            "perm" | "permissions" => {
                let valid = [
                    "approval-required",
                    "auto-accept-edits",
                    "auto",
                    "full-access",
                ];
                if !valid.contains(&arg) {
                    self.toast(format!("usage: :perm {}", valid.join("|")), true);
                } else if let Some(id) = thread_id.as_deref() {
                    self.dispatch(commands::runtime_mode_set(id, arg));
                } else if let Some(d) = self.draft.as_mut() {
                    d.runtime_mode = arg.into();
                }
            }
            "rename" | "title" => match thread_id.as_deref() {
                Some(id) if arg.is_empty() => self.dispatch(commands::meta_regenerate_title(id)),
                Some(id) => self.dispatch(commands::meta_update_title(id, arg)),
                None => self.toast("no thread open", true),
            },
            "archive" => {
                if let Some(id) = thread_id.as_deref() {
                    self.dispatch(commands::simple("thread.archive", id));
                    self.toast("archived", false);
                    self.current_thread_id = None;
                    self.thread = None;
                    self.handle.close_thread();
                }
            }
            "delete!" => {
                if let Some(id) = thread_id.as_deref() {
                    self.dispatch(commands::simple("thread.delete", id));
                    self.current_thread_id = None;
                    self.thread = None;
                    self.handle.close_thread();
                }
            }
            "delete" => self.toast("use :delete! to confirm deleting this thread", true),
            "stop" | "interrupt" => self.interrupt(),
            "sidebar" => self.sidebar_visible = !self.sidebar_visible,
            "pr" | "pull" => self.open_pull_request(true),
            "tmux" => self.switch_tmux_session(),
            "git" | "lazygit" => self.open_git(),
            "tasks" | "jobs" => self.open_tasks(),
            "terminals" | "shells" => self.open_terminals(),
            "shell" => self.open_shell(),
            "edit" => self.edit_composer(),
            "view" => self.view_conversation(),
            "settled" => self.show_settled = !self.show_settled,
            "settle" => {
                if let Some(id) = thread_id.as_deref() {
                    self.dispatch(commands::simple("thread.settle", id));
                    self.toast("settled", false);
                }
            }
            "unsettle" => {
                if let Some(id) = thread_id.as_deref() {
                    self.dispatch(commands::unsettle(id));
                    self.toast("un-settled", false);
                }
            }
            "wake" | "unsnooze" => {
                if let Some(id) = thread_id.as_deref() {
                    self.dispatch(commands::unsnooze(id));
                }
            }
            "older" => self.load_older(),
            "dismiss" => self.dismiss_question(),
            "answer" | "a" => {
                if !self.begin_answering() {
                    self.toast("no pending question", false);
                }
            }
            _ => self.toast(format!("unknown command :{name}"), true),
        }
    }

    fn load_older(&mut self) {
        if let Some(thread) = &self.thread {
            match (&thread.before_cursor, thread.has_more) {
                (Some(cursor), true) => {
                    self.handle.load_older(cursor);
                    self.toast("loading older turns…", false);
                }
                _ => self.toast("no older turns", false),
            }
        }
    }

    // ── Chat cursor ────────────────────────────────────────────────────

    /// Lines kept between the cursor and the viewport edge while moving.
    const SCROLLOFF: usize = 3;

    fn focus_chat(&mut self) {
        if self.thread.is_none() {
            self.toast("open a thread first (/ or Tab), or n for a new one", false);
            return;
        }
        self.focus = Focus::Chat;
        let (height, total) = self.chat_viewport;
        if self.scroll == Scroll::Follow {
            self.chat_cursor = total.saturating_sub(1);
        } else {
            let offset = self.chat_offset();
            self.chat_cursor = self
                .chat_cursor
                .clamp(offset, (offset + height).saturating_sub(1).max(offset));
        }
    }

    /// Place the cursor and scroll just enough to keep it in view with a margin. Landing on
    /// the last line resumes following new output.
    fn set_chat_cursor(&mut self, line: usize) {
        let (height, total) = self.chat_viewport;
        if total == 0 || height == 0 {
            return;
        }
        let line = line.min(total - 1);
        self.chat_cursor = line;
        let max_offset = total.saturating_sub(height);
        let mut offset = self.chat_offset();
        let margin = Self::SCROLLOFF.min(height.saturating_sub(1) / 2);
        if line < offset + margin {
            offset = line.saturating_sub(margin);
        } else if line + margin >= offset + height {
            offset = (line + margin + 1).saturating_sub(height);
        }
        let offset = offset.min(max_offset);
        self.scroll = if line == total - 1 || offset >= max_offset && line + 1 >= total {
            Scroll::Follow
        } else {
            Scroll::Offset(offset)
        };
    }

    /// Scroll the view and carry the cursor along, like Vim's Ctrl-d and Ctrl-u.
    fn chat_scroll_by(&mut self, delta: isize) {
        let (height, total) = self.chat_viewport;
        let before = self.chat_offset();
        self.scroll_by(delta);
        let after = self.chat_offset();
        let moved = after as isize - before as isize;
        let target = (self.chat_cursor as isize + if moved == 0 { delta } else { moved })
            .clamp(0, total.saturating_sub(1) as isize) as usize;
        self.chat_cursor = target.clamp(after, (after + height).saturating_sub(1).max(after));
        if self.chat_cursor + 1 >= total {
            self.scroll = Scroll::Follow;
        }
    }

    fn take_chat_count(&mut self) -> usize {
        self.chat_count.take().unwrap_or(1).max(1)
    }

    fn on_chat_key(&mut self, key: KeyEvent, prefix: Option<char>) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let (height, total) = self.chat_viewport;
        let cursor = self.chat_cursor;
        match key.code {
            KeyCode::Char(c @ '0'..='9') if c != '0' || self.chat_count.is_some() => {
                let current = self.chat_count.unwrap_or(0);
                self.chat_count = Some((current * 10 + (c as usize - '0' as usize)).min(100_000));
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let n = self.take_chat_count();
                self.set_chat_cursor(cursor + n);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let n = self.take_chat_count();
                self.set_chat_cursor(cursor.saturating_sub(n));
            }
            KeyCode::Char('d') if ctrl => self.chat_scroll_by(height as isize / 2),
            KeyCode::Char('u') if ctrl => self.chat_scroll_by(-(height as isize / 2)),
            KeyCode::Char('f') if ctrl => self.chat_scroll_by(height as isize),
            KeyCode::Char('b') if ctrl => self.chat_scroll_by(-(height as isize)),
            KeyCode::PageDown => self.chat_scroll_by(height as isize),
            KeyCode::PageUp => self.chat_scroll_by(-(height as isize)),
            KeyCode::Char('e') if ctrl => self.chat_scroll_by(1),
            KeyCode::Char('y') if ctrl => self.chat_scroll_by(-1),
            KeyCode::Char('g') if prefix == Some('g') => {
                self.chat_count = None;
                self.set_chat_cursor(0);
                self.scroll = Scroll::Offset(0);
                if self.thread.as_ref().is_some_and(|t| t.has_more) {
                    self.load_older();
                }
            }
            KeyCode::Char('x') if prefix == Some('g') => self.open_pull_request(false),
            KeyCode::Char('t') if prefix == Some('g') => self.switch_tmux_session(),
            KeyCode::Char('y') if prefix == Some('g') => self.yank_last_assistant(),
            KeyCode::Char('s') if prefix == Some('g') => {
                let id = self.current_thread_id.clone();
                self.toggle_settled(id);
            }
            KeyCode::Char('l') if prefix == Some('g') => self.open_git(),
            KeyCode::Char('!') if prefix == Some('g') => self.open_shell(),
            KeyCode::Char('T') if prefix == Some('g') => self.open_tasks(),
            KeyCode::Char('S') if prefix == Some('g') => self.open_terminals(),
            KeyCode::Char('e') if prefix == Some('g') => self.view_at_cursor(),
            KeyCode::Char('E') if prefix == Some('g') => self.view_conversation(),
            KeyCode::Char('a') if prefix == Some('g') => {
                self.begin_answering();
            }
            KeyCode::Char('g') => self.pending_prefix = Some(('g', Instant::now())),
            KeyCode::Char('G') => {
                self.chat_count = None;
                self.chat_cursor = total.saturating_sub(1);
                self.scroll = Scroll::Follow;
            }
            KeyCode::Char('{') => {
                let n = self.take_chat_count();
                let mut target = cursor;
                for _ in 0..n {
                    match self.message_starts.iter().rev().find(|&&s| s < target) {
                        Some(&s) => target = s,
                        None => break,
                    }
                }
                self.set_chat_cursor(target);
            }
            KeyCode::Char('}') => {
                let n = self.take_chat_count();
                let mut target = cursor;
                for _ in 0..n {
                    match self.message_starts.iter().find(|&&s| s > target) {
                        Some(&s) => target = s,
                        None => {
                            target = total.saturating_sub(1);
                            break;
                        }
                    }
                }
                self.set_chat_cursor(target);
            }
            KeyCode::Char('z') => self.pending_prefix = Some(('z', Instant::now())),
            KeyCode::Char('a') if prefix == Some('z') => {
                if let Some(key) = self.toggle_key_at(cursor) {
                    self.toggle_expanded(key);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(key) = self.toggle_key_at(cursor) {
                    self.toggle_expanded(key);
                } else {
                    self.toast("nothing to fold here", false);
                }
            }
            KeyCode::Char('R') if prefix == Some('z') => self.expand_all = true,
            KeyCode::Char('M') if prefix == Some('z') => {
                self.expand_all = false;
                self.expanded.clear();
            }
            KeyCode::Char('V') | KeyCode::Char('v') => {
                self.chat_visual = match self.chat_visual {
                    Some(_) => None,
                    None => Some(cursor),
                };
            }
            KeyCode::Char('y') => {
                let (start, end) = match self.chat_visual.take() {
                    Some(anchor) => (anchor.min(cursor), anchor.max(cursor)),
                    None => {
                        // `yy`: the current line, or `Ny` for several.
                        let n = self.take_chat_count();
                        (cursor, (cursor + n - 1).min(total.saturating_sub(1)))
                    }
                };
                match ui::chat_text(start, end) {
                    Some(text) if !text.trim().is_empty() => {
                        copy_to_clipboard(&text);
                        let lines = end - start + 1;
                        self.toast(
                            if lines == 1 {
                                "yanked line".to_string()
                            } else {
                                format!("yanked {lines} lines")
                            },
                            false,
                        );
                    }
                    _ => self.toast("nothing to yank", true),
                }
            }
            KeyCode::Esc if self.chat_visual.is_some() || self.chat_count.is_some() => {
                self.chat_visual = None;
                self.chat_count = None;
            }
            KeyCode::Esc => {
                self.toast = None;
                self.focus = Focus::Composer;
            }
            KeyCode::Tab => {
                self.chat_visual = None;
                self.sidebar_visible = true;
                self.focus = Focus::Sidebar;
            }
            KeyCode::BackTab => {
                self.chat_visual = None;
                self.focus = Focus::Composer;
            }
            KeyCode::Char('i') => {
                self.chat_visual = None;
                self.focus = Focus::Composer;
                self.composer.checkpoint();
                self.mode = Mode::Insert;
            }
            KeyCode::Char('J') => self.open_relative(1),
            KeyCode::Char('K') => self.open_relative(-1),
            KeyCode::Char('/') => self.start_search(false),
            KeyCode::Char('?') => self.start_search(true),
            KeyCode::Char('n') => self.search_next(false),
            KeyCode::Char('N') => self.search_next(true),
            KeyCode::Char('m') => self.open_picker(PickerKind::Model),
            KeyCode::Char('s') => self.sidebar_visible = !self.sidebar_visible,
            KeyCode::Char('S') => self.show_settled = !self.show_settled,
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.command_line.clear();
            }
            _ => {}
        }
    }

    // ── Chat search ────────────────────────────────────────────────────

    fn start_search(&mut self, backward: bool) {
        self.chat_count = None;
        self.search_input = Some(SearchInput {
            query: String::new(),
            backward,
            origin: self.chat_cursor,
        });
        self.mode = Mode::Search;
    }

    fn on_search_key(&mut self, key: KeyEvent) {
        let Some(input) = self.search_input.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        match key.code {
            KeyCode::Esc => {
                let origin = input.origin;
                self.search_input = None;
                self.mode = Mode::Normal;
                self.set_chat_cursor(origin);
            }
            KeyCode::Enter => {
                let input = self.search_input.take().unwrap();
                self.mode = Mode::Normal;
                if input.query.is_empty() {
                    // Bare Enter repeats the last search, as in Vim.
                    self.search_next(false);
                    return;
                }
                let search = Search {
                    query: input.query,
                    backward: input.backward,
                };
                self.search = Some(search.clone());
                self.jump_to_match(&search, input.origin, false);
            }
            KeyCode::Backspace => {
                input.query.pop();
                self.incremental_search();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.query.clear();
                self.incremental_search();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.query.push(c);
                self.incremental_search();
            }
            _ => {}
        }
    }

    /// While typing, the cursor previews the first match from where the search started.
    fn incremental_search(&mut self) {
        let Some(input) = self.search_input.clone() else {
            return;
        };
        if input.query.is_empty() {
            self.set_chat_cursor(input.origin);
            return;
        }
        let search = Search {
            query: input.query,
            backward: input.backward,
        };
        if let Some((line, _)) = self.find_match(&search, input.origin) {
            self.set_chat_cursor(line);
        }
    }

    /// `n` and `N`: continue the last search from the cursor, `N` against its direction.
    fn search_next(&mut self, reverse: bool) {
        self.chat_count = None;
        let Some(mut search) = self.search.clone() else {
            self.toast("no previous search", true);
            return;
        };
        if reverse {
            search.backward = !search.backward;
        }
        let from = self.chat_cursor;
        self.jump_to_match(&search, from, true);
    }

    fn jump_to_match(&mut self, search: &Search, from: usize, silent_hit: bool) {
        match self.find_match(search, from) {
            Some((line, wrapped)) => {
                self.set_chat_cursor(line);
                if wrapped {
                    self.toast(
                        if search.backward {
                            "search hit TOP, continuing at BOTTOM"
                        } else {
                            "search hit BOTTOM, continuing at TOP"
                        },
                        false,
                    );
                } else if !silent_hit {
                    self.toast = None;
                }
            }
            None => self.toast(format!("pattern not found: {}", search.query), true),
        }
    }

    /// The next matching content line after (or before) `from`, wrapping around the
    /// conversation. Returns the line and whether the search wrapped.
    fn find_match(&self, search: &Search, from: usize) -> Option<(usize, bool)> {
        let lines = ui::chat_lines();
        if lines.is_empty() || search.query.is_empty() {
            return None;
        }
        let matches = |line: &str| line_matches(line, &search.query);
        let len = lines.len();
        if search.backward {
            let from = from.min(len);
            (0..from)
                .rev()
                .find(|&i| matches(&lines[i]))
                .map(|i| (i, false))
                .or_else(|| {
                    (from..len)
                        .rev()
                        .find(|&i| matches(&lines[i]))
                        .map(|i| (i, true))
                })
        } else {
            (from + 1..len)
                .find(|&i| matches(&lines[i]))
                .map(|i| (i, false))
                .or_else(|| {
                    (0..=from.min(len - 1))
                        .find(|&i| matches(&lines[i]))
                        .map(|i| (i, true))
                })
        }
    }

    // ── Scrolling ──────────────────────────────────────────────────────

    fn scroll_by(&mut self, delta: isize) {
        let (height, total) = self.chat_viewport;
        let max_offset = total.saturating_sub(height);
        let current = match self.scroll {
            Scroll::Follow => max_offset,
            Scroll::Offset(o) => o.min(max_offset),
        };
        let next = (current as isize + delta).clamp(0, max_offset as isize) as usize;
        self.scroll = if next >= max_offset {
            Scroll::Follow
        } else {
            Scroll::Offset(next)
        };
        if next == 0 && delta < 0 {
            // Reached the top: pull in older turns if the server has them.
            if self.thread.as_ref().is_some_and(|t| t.has_more) {
                self.load_older();
            }
        }
    }

    /// First content line shown in the chat viewport.
    pub fn chat_offset(&self) -> usize {
        let (height, total) = self.chat_viewport;
        match self.scroll {
            Scroll::Follow => total.saturating_sub(height),
            Scroll::Offset(o) => o.min(total.saturating_sub(height)),
        }
    }

    /// The tightest toggle region covering a content line: a tool row inside an expanded
    /// group wins over the group itself.
    fn toggle_key_at(&self, line: usize) -> Option<String> {
        self.work_ranges
            .iter()
            .filter(|(start, end, _)| *start <= line && line < *end)
            .min_by_key(|(start, end, _)| end - start)
            .map(|(_, _, key)| key.clone())
    }

    fn toggle_expanded(&mut self, key: String) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
    }

    fn toggle_work_group(&mut self) {
        let (height, _) = self.chat_viewport;
        let offset = self.chat_offset();
        let middle = offset + height / 2;
        let key = self
            .toggle_key_at(middle)
            .or_else(|| {
                self.work_ranges
                    .iter()
                    .rev()
                    .find(|(start, _, _)| *start < offset + height)
                    .map(|(_, _, key)| key.clone())
            })
            .or_else(|| self.work_ranges.last().map(|(_, _, key)| key.clone()));
        if let Some(key) = key {
            self.toggle_expanded(key);
        }
    }

    // ── Key handling ───────────────────────────────────────────────────

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return;
        }
        self.selection = None;
        // The attached terminal takes every key, including Ctrl-c, before the global
        // chords: interrupting the shell is the whole point of that key there.
        if self.mode == Mode::TerminalPane {
            self.on_pane_key(key);
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Global chords.
        if ctrl && key.code == KeyCode::Char('q') {
            self.quit = true;
            return;
        }
        if ctrl && key.code == KeyCode::Char('c') {
            if self.thread.as_ref().is_some_and(|t| t.is_running()) {
                self.interrupt();
            } else if self.mode == Mode::Normal {
                self.toast("nothing running · :q to quit", false);
            } else {
                self.mode = Mode::Normal;
                self.picker = None;
                self.command_line.clear();
            }
            return;
        }
        match self.mode {
            Mode::Normal => self.on_normal_key(key),
            Mode::Insert => self.on_insert_key(key),
            Mode::Command => self.on_command_key(key),
            Mode::Search => self.on_search_key(key),
            Mode::Tasks => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter | KeyCode::Char('T')
                ) {
                    self.mode = Mode::Normal;
                }
            }
            Mode::Terminals => self.on_terminals_key(key),
            // Handled above, before the global chords.
            Mode::TerminalPane => {}
            Mode::Picker => self.on_picker_key(key),
            Mode::Question => self.on_question_key(key),
            Mode::QuestionCustom => self.on_question_custom_key(key),
            Mode::Help => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter
                ) {
                    self.mode = Mode::Normal;
                }
            }
        }
    }

    fn take_prefix(&mut self) -> Option<char> {
        let (prefix, at) = self.pending_prefix.take()?;
        (at.elapsed() < PREFIX_TTL).then_some(prefix)
    }

    fn on_normal_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let prefix = self.take_prefix();
        let (height, _) = self.chat_viewport;
        if self.focus == Focus::Sidebar {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => self.sidebar_move(1),
                KeyCode::Char('k') | KeyCode::Up => self.sidebar_move(-1),
                KeyCode::Char('g') if prefix == Some('g') => {
                    self.sidebar_selected = 0;
                    self.sidebar_reveal = true;
                }
                KeyCode::Char('s') if prefix == Some('g') => {
                    let id = self.sidebar_selected_thread();
                    self.toggle_settled(id);
                }
                KeyCode::Char('l') if prefix == Some('g') => self.open_git(),
                KeyCode::Char('!') if prefix == Some('g') => self.open_shell(),
                KeyCode::Char('T') if prefix == Some('g') => self.open_tasks(),
                KeyCode::Char('S') if prefix == Some('g') => self.open_terminals(),
                KeyCode::Char('g') => self.pending_prefix = Some(('g', Instant::now())),
                KeyCode::Char('G') => {
                    self.sidebar_selected = self.sidebar_rows().len().saturating_sub(1);
                    self.sidebar_reveal = true;
                }
                KeyCode::Enter | KeyCode::Char('l') | KeyCode::Char(' ') => self.sidebar_activate(),
                KeyCode::Char('S') => self.show_settled = !self.show_settled,
                KeyCode::Esc | KeyCode::Tab | KeyCode::Char('h') => self.focus = Focus::Composer,
                KeyCode::BackTab => self.focus_chat(),
                KeyCode::Char('/') => self.open_picker(PickerKind::Thread),
                KeyCode::Char('n') => self.open_picker(PickerKind::Project),
                KeyCode::Char(':') => {
                    self.mode = Mode::Command;
                    self.command_line.clear();
                }
                KeyCode::Char('?') => self.mode = Mode::Help,
                _ => {}
            }
            return;
        }
        if self.focus == Focus::Chat {
            self.on_chat_key(key, prefix);
            return;
        }
        let question_pending = self
            .thread
            .as_ref()
            .is_some_and(|t| t.pending_user_input().is_some());
        let approval_pending = self
            .thread
            .as_ref()
            .is_some_and(|t| !t.pending_approvals().is_empty());
        // Chat and app keys first; whatever is left edits the composer.
        match key.code {
            KeyCode::Char('d') if ctrl => return self.scroll_by(height as isize / 2),
            KeyCode::Char('u') if ctrl => return self.scroll_by(-(height as isize / 2)),
            KeyCode::Char('f') if ctrl => return self.scroll_by(height as isize),
            KeyCode::Char('b') if ctrl => return self.scroll_by(-(height as isize)),
            KeyCode::Char('e') if ctrl => return self.scroll_by(1),
            KeyCode::Char('y') if ctrl => return self.scroll_by(-1),
            KeyCode::PageDown => return self.scroll_by(height as isize),
            KeyCode::PageUp => return self.scroll_by(-(height as isize)),
            KeyCode::Char('g') if prefix == Some('g') => return self.composer.vim_top(),
            KeyCode::Char('x') if prefix == Some('g') => return self.open_pull_request(false),
            KeyCode::Char('t') if prefix == Some('g') => return self.switch_tmux_session(),
            KeyCode::Char('y') if prefix == Some('g') => return self.yank_last_assistant(),
            KeyCode::Char('s') if prefix == Some('g') => {
                let id = self.current_thread_id.clone();
                self.toggle_settled(id);
                return;
            }
            KeyCode::Char('l') if prefix == Some('g') => return self.open_git(),
            KeyCode::Char('!') if prefix == Some('g') => return self.open_shell(),
            KeyCode::Char('T') if prefix == Some('g') => return self.open_tasks(),
            KeyCode::Char('S') if prefix == Some('g') => return self.open_terminals(),
            KeyCode::Char('e') if prefix == Some('g') => return self.edit_composer(),
            KeyCode::Char('E') if prefix == Some('g') => return self.view_conversation(),
            KeyCode::Char('a') if prefix == Some('g') => {
                if question_pending {
                    self.begin_answering();
                } else {
                    self.toast("no question pending", false);
                }
                return;
            }
            KeyCode::Char('g') if !self.composer.vim_pending() => {
                self.pending_prefix = Some(('g', Instant::now()));
                return;
            }
            KeyCode::Char('z') => {
                self.pending_prefix = Some(('z', Instant::now()));
                return;
            }
            KeyCode::Char('a') if prefix == Some('z') => return self.toggle_work_group(),
            KeyCode::Char('R') if prefix == Some('z') => return self.expand_all = true,
            KeyCode::Char('M') if prefix == Some('z') => {
                self.expand_all = false;
                self.expanded.clear();
                return;
            }
            KeyCode::Char('J') => return self.open_relative(1),
            KeyCode::Char('K') => return self.open_relative(-1),
            KeyCode::Tab => {
                self.focus_chat();
                return;
            }
            KeyCode::BackTab => {
                self.sidebar_visible = true;
                self.focus = Focus::Sidebar;
                return;
            }
            KeyCode::Char('/') => return self.open_picker(PickerKind::Thread),
            KeyCode::Char('n') => return self.open_picker(PickerKind::Project),
            KeyCode::Char('m') => return self.open_picker(PickerKind::Model),
            KeyCode::Enter if question_pending => {
                self.begin_answering();
                return;
            }
            KeyCode::Enter => {
                if self.thread.is_some() || self.draft.is_some() {
                    self.composer.checkpoint();
                    self.mode = Mode::Insert;
                } else {
                    self.toast("open a thread first (/ or Tab), or n for a new one", false);
                }
                return;
            }
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.command_line.clear();
                return;
            }
            KeyCode::Char('?') => return self.mode = Mode::Help,
            KeyCode::Char('s') if !self.composer.vim_pending() => {
                return self.sidebar_visible = !self.sidebar_visible;
            }
            KeyCode::Char('S') if !self.composer.vim_pending() => {
                return self.show_settled = !self.show_settled;
            }
            KeyCode::Char(c @ '1'..='9') if approval_pending && !self.composer.vim_pending() => {
                return self.respond_approval(c as usize - '1' as usize);
            }
            KeyCode::Esc => {
                self.toast = None;
                self.scroll = Scroll::Follow;
                self.composer.vim_cancel();
                return;
            }
            _ => {}
        }
        match self.composer.vim_key(key) {
            vim::Effect::None => {}
            vim::Effect::EnterInsert => {
                if self.thread.is_some() || self.draft.is_some() {
                    self.mode = Mode::Insert;
                } else {
                    self.toast("open a thread first (/ or Tab), or n for a new one", false);
                }
            }
            vim::Effect::Yanked(text) => {
                copy_to_clipboard(&text);
                let lines = text.lines().count();
                if lines > 1 {
                    self.toast(format!("yanked {lines} lines"), false);
                }
            }
        }
    }

    fn sidebar_move(&mut self, delta: isize) {
        let len = self.sidebar_rows().len();
        if len == 0 {
            return;
        }
        self.sidebar_selected =
            (self.sidebar_selected as isize + delta).clamp(0, len as isize - 1) as usize;
        self.sidebar_reveal = true;
    }

    // ── Mouse ──────────────────────────────────────────────────────────

    fn on_mouse(&mut self, mouse: MouseEvent) {
        // Overlays own the screen; the wheel and clicks would land on hidden widgets.
        if matches!(self.mode, Mode::Picker | Mode::Help) {
            return;
        }
        // The attached terminal scrolls its own scrollback and ignores the rest.
        if self.mode == Mode::TerminalPane {
            if let Some(pane) = self.pane.as_mut() {
                match mouse.kind {
                    MouseEventKind::ScrollUp => pane.scroll(MOUSE_SCROLL_LINES as isize),
                    MouseEventKind::ScrollDown => pane.scroll(-(MOUSE_SCROLL_LINES as isize)),
                    _ => {}
                }
            }
            return;
        }
        let at = Position::new(mouse.column, mouse.row);
        let in_sidebar = self.sidebar_inner.is_some_and(|r| r.contains(at));
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta = if mouse.kind == MouseEventKind::ScrollUp {
                    -(MOUSE_SCROLL_LINES as isize)
                } else {
                    MOUSE_SCROLL_LINES as isize
                };
                if in_sidebar {
                    let len = self.sidebar_rows().len();
                    let height = self.sidebar_inner.map_or(0, |r| r.height as usize);
                    let max = len.saturating_sub(height);
                    self.sidebar_offset =
                        (self.sidebar_offset as isize + delta).clamp(0, max as isize) as usize;
                } else {
                    self.scroll_by(delta);
                }
            }
            MouseEventKind::Down(MouseButton::Left) if in_sidebar => {
                let Some(inner) = self.sidebar_inner else {
                    return;
                };
                let index = self.sidebar_offset + (mouse.row - inner.y) as usize;
                let rows = self.sidebar_rows();
                match rows.get(index) {
                    Some(SidebarRow::Thread { id, .. }) => {
                        let id = id.clone();
                        self.sidebar_selected = index;
                        if self.current_thread_id.as_deref() != Some(id.as_str()) {
                            self.open_thread(&id);
                        }
                        if self.focus == Focus::Sidebar {
                            self.focus = Focus::Composer;
                        }
                    }
                    Some(SidebarRow::Header { section, .. }) => {
                        self.sidebar_selected = index;
                        self.toggle_section(*section);
                    }
                    None => {}
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.focus == Focus::Sidebar {
                    self.focus = Focus::Composer;
                }
                self.selection = if self.chat_area.contains(at) {
                    Some(Selection {
                        anchor: at,
                        head: at,
                        dragging: true,
                    })
                } else {
                    None
                };
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(sel) = self.selection.as_mut()
                    && sel.dragging
                {
                    sel.head = clamp_to(self.chat_area, at);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(sel) = self.selection.as_mut()
                    && sel.dragging
                {
                    sel.dragging = false;
                    if sel.anchor == sel.head {
                        // A plain click: fold or unfold whatever tool row or group is there.
                        let at = sel.anchor;
                        self.selection = None;
                        if self.chat_area.contains(at) {
                            let line = self.chat_offset() + (at.y - self.chat_area.y) as usize;
                            if self.thread.is_some() {
                                self.focus = Focus::Chat;
                                self.chat_visual = None;
                                self.set_chat_cursor(line);
                            }
                            if let Some(key) = self.toggle_key_at(line) {
                                self.toggle_expanded(key);
                            }
                        }
                    } else {
                        // The renderer fills `clipboard_pending` from the drawn cells.
                        self.clipboard_pending = Some(String::new());
                    }
                }
            }
            _ => {}
        }
    }

    /// Called by the event loop once the frame that resolved a selection has been drawn.
    pub fn flush_clipboard(&mut self) {
        if let Some(text) = self.clipboard_pending.take() {
            if text.trim().is_empty() {
                return;
            }
            let lines = text.lines().count();
            copy_to_clipboard(&text);
            self.toast(
                if lines == 1 {
                    "copied selection".to_string()
                } else {
                    format!("copied {lines} lines")
                },
                false,
            );
        }
    }

    fn on_insert_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.composer.leave_insert();
            }
            KeyCode::Enter if alt || shift || ctrl => self.composer.newline(),
            KeyCode::Char('j') if ctrl => self.composer.newline(),
            KeyCode::Enter => self.send_message(),
            KeyCode::Backspace if alt || ctrl => self.composer.kill_word_back(),
            KeyCode::Backspace => self.composer.backspace(),
            KeyCode::Delete => self.composer.delete(),
            KeyCode::Left if alt || ctrl => self.composer.word_left(),
            KeyCode::Right if alt || ctrl => self.composer.word_right(),
            KeyCode::Left => self.composer.left(),
            KeyCode::Right => self.composer.right(),
            KeyCode::Up => {
                if !self.composer.up() {
                    self.composer.history_prev();
                }
            }
            KeyCode::Down => {
                if !self.composer.down() {
                    self.composer.history_next();
                }
            }
            KeyCode::Home => self.composer.home(),
            KeyCode::End => self.composer.end(),
            KeyCode::PageUp => self.scroll_by(-(self.chat_viewport.0 as isize)),
            KeyCode::PageDown => self.scroll_by(self.chat_viewport.0 as isize),
            KeyCode::Char('a') if ctrl => self.composer.home(),
            KeyCode::Char('e') if ctrl => self.composer.end(),
            KeyCode::Char('b') if alt => self.composer.word_left(),
            KeyCode::Char('f') if alt => self.composer.word_right(),
            KeyCode::Char('w') if ctrl => self.composer.kill_word_back(),
            KeyCode::Char('k') if ctrl => self.composer.kill_to_end(),
            KeyCode::Char('u') if ctrl => self.composer.kill_to_start(),
            KeyCode::Char('p') if ctrl => self.composer.history_prev(),
            KeyCode::Char('n') if ctrl => self.composer.history_next(),
            KeyCode::Tab => self.composer.insert_str("    "),
            KeyCode::Char(c) if !ctrl => self.composer.insert_char(c),
            _ => {}
        }
    }

    fn on_command_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.command_line.clear();
            }
            KeyCode::Enter => {
                let line = std::mem::take(&mut self.command_line);
                self.mode = Mode::Normal;
                self.run_command(&line);
            }
            KeyCode::Backspace => {
                if self.command_line.pop().is_none() {
                    self.mode = Mode::Normal;
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.command_line.clear()
            }
            KeyCode::Char(c) => self.command_line.push(c),
            _ => {}
        }
    }

    fn on_picker_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(picker) = self.picker.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        let count = picker.filtered().len();
        match key.code {
            KeyCode::Esc => {
                self.picker = None;
                self.mode = if self.draft.is_some() {
                    Mode::Insert
                } else {
                    Mode::Normal
                };
            }
            KeyCode::Enter => self.picker_select(),
            KeyCode::Down | KeyCode::Tab => {
                picker.selected = (picker.selected + 1).min(count.saturating_sub(1))
            }
            KeyCode::Up | KeyCode::BackTab => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Char('n') if ctrl => {
                picker.selected = (picker.selected + 1).min(count.saturating_sub(1))
            }
            KeyCode::Char('p') if ctrl => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Char('j') if ctrl => {
                picker.selected = (picker.selected + 1).min(count.saturating_sub(1))
            }
            KeyCode::Char('k') if ctrl => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Backspace => {
                picker.query.pop();
                picker.selected = 0;
            }
            KeyCode::Char('u') if ctrl => {
                picker.query.clear();
                picker.selected = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                picker.query.push(c);
                picker.selected = 0;
            }
            _ => {}
        }
    }

    // ── Server updates ─────────────────────────────────────────────────

    fn on_update(&mut self, update: Update) {
        match update {
            Update::Status(status) => {
                if let Status::Reconnecting { attempt, error } = &status
                    && *attempt == 1
                {
                    self.toast(format!("disconnected: {error}"), true);
                }
                // A reconnect drops the attachment with the socket; take it up again.
                if status == Status::Connected
                    && let Some(pane) = &self.pane
                {
                    let (cols, rows) = pane.size();
                    let cwd = self.thread_directory().unwrap_or_default();
                    self.handle.attach_terminal(
                        &pane.thread_id,
                        &pane.terminal_id,
                        &cwd,
                        cols,
                        rows,
                    );
                }
                self.status = status;
            }
            Update::Config(config) => self.config = *config,
            Update::Shell(item) => {
                if let ShellItem::ThreadUpserted { thread, .. } = &item
                    && let Some(open) = self.thread.as_mut().filter(|t| t.id() == thread.id)
                {
                    open.sync_shell(thread);
                }
                let removed = matches!(&item, ShellItem::ThreadRemoved { thread_id, .. } if Some(thread_id) == self.current_thread_id.as_ref());
                self.shell.apply(item);
                if removed {
                    self.thread = None;
                    self.current_thread_id = None;
                    self.toast("thread was removed", false);
                }
                if self.current_thread_id.is_none()
                    && self.draft.is_none()
                    && self.shell.synchronized
                    && let Some(first) = self.visible_threads().first().cloned()
                {
                    self.open_thread(&first);
                }
            }
            Update::Thread { thread_id, item } => {
                if self.current_thread_id.as_deref() != Some(thread_id.as_str()) {
                    return;
                }
                match (&mut self.thread, item) {
                    (None, ThreadItem::Snapshot { snapshot }) => {
                        let mut state = ThreadState::from_snapshot(snapshot);
                        if let Some(shell) = self.shell.threads.get(&thread_id) {
                            state.sync_shell(shell);
                        }
                        self.thread = Some(state);
                    }
                    (Some(thread), item) => thread.apply(item),
                    (None, _) => {}
                }
                self.reconcile_question();
            }
            Update::OlderPage {
                thread_id,
                snapshot,
            } => {
                if self.current_thread_id.as_deref() == Some(thread_id.as_str())
                    && let Some(thread) = self.thread.as_mut()
                {
                    prepend_page(thread, snapshot);
                }
            }
            Update::Terminals(event) => self.apply_terminal_event(event),
            Update::TerminalStream(event) => self.apply_terminal_stream(event),
            Update::ThreadStreamError { error } => {
                self.toast(format!("stream error, resubscribing: {error}"), true)
            }
            Update::Error(error) => self.toast(error, true),
        }
    }
}

fn prepend_page(thread: &mut ThreadState, snapshot: ThreadDetailSnapshot) {
    let page = snapshot.page;
    let older = snapshot.thread;
    let mut messages = older.messages;
    messages.retain(|m| !thread.detail.messages.iter().any(|e| e.id == m.id));
    messages.append(&mut thread.detail.messages);
    thread.detail.messages = messages;
    let mut activities = older.activities;
    activities.retain(|a| !thread.detail.activities.iter().any(|e| e.id == a.id));
    activities.append(&mut thread.detail.activities);
    thread.detail.activities = activities;
    for plan in older.proposed_plans {
        if !thread.detail.proposed_plans.iter().any(|p| p.id == plan.id) {
            thread.detail.proposed_plans.insert(0, plan);
        }
    }
    match page {
        Some(page) => {
            thread.has_more = page.has_more;
            thread.before_cursor = page.before_cursor;
        }
        None => {
            thread.has_more = false;
            thread.before_cursor = None;
        }
    }
    thread.revision += 1;
}

pub fn approval_options(approval: &PendingApproval) -> Vec<ApprovalOption> {
    if !approval.options.is_empty() {
        return approval.options.clone();
    }
    vec![
        ApprovalOption {
            decision: "accept".into(),
            label: "Allow".into(),
        },
        ApprovalOption {
            decision: "acceptForSession".into(),
            label: "Allow for session".into(),
        },
        ApprovalOption {
            decision: "decline".into(),
            label: "Deny".into(),
        },
    ]
}

/// A drag selection over the chat, in screen coordinates. `anchor` is where the button went
/// down and `head` follows the pointer; either may come first in reading order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Position,
    pub head: Position,
    pub dragging: bool,
}

impl Selection {
    /// Start and end in reading order (row-major), both inclusive.
    pub fn ordered(&self) -> (Position, Position) {
        if (self.anchor.y, self.anchor.x) <= (self.head.y, self.head.x) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// Substring match; case-insensitive unless the query has an uppercase letter (smartcase).
pub fn line_matches(line: &str, query: &str) -> bool {
    if query.chars().any(char::is_uppercase) {
        line.contains(query)
    } else {
        line.to_lowercase().contains(query)
    }
}

/// Byte ranges of every match in `line`, with the same case rule as `line_matches`.
pub fn match_ranges(line: &str, query: &str) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    let smart = !query.chars().any(char::is_uppercase);
    let haystack = if smart {
        line.to_lowercase()
    } else {
        line.to_string()
    };
    if haystack.len() != line.len() {
        // Lowercasing changed byte lengths; fall back to char-wise matching.
        let hay: Vec<char> = haystack.chars().collect();
        let needle: Vec<char> = query.chars().collect();
        let mut out = Vec::new();
        let byte_offsets: Vec<usize> = line.char_indices().map(|(i, _)| i).collect();
        let mut i = 0;
        while i + needle.len() <= hay.len() {
            if hay[i..i + needle.len()] == needle[..] {
                let start = byte_offsets[i];
                let end = byte_offsets
                    .get(i + needle.len())
                    .copied()
                    .unwrap_or(line.len());
                out.push((start, end));
                i += needle.len();
            } else {
                i += 1;
            }
        }
        return out;
    }
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(query) {
        let start = from + pos;
        out.push((start, start + query.len()));
        from = start + query.len().max(1);
    }
    out
}

/// A program that takes over the terminal while tria waits.
#[derive(Debug, Clone)]
pub struct ExternalCommand {
    pub command: String,
    pub dir: String,
    pub then: Option<FollowUp>,
}

/// What to do once an external program exits.
#[derive(Debug, Clone)]
pub enum FollowUp {
    /// Read the file back into the composer.
    LoadComposer(std::path::PathBuf),
}

/// Write `contents` to a fresh private file in the temp directory.
fn write_temp_file(name: &str, ext: &str, contents: &str) -> std::io::Result<std::path::PathBuf> {
    let path = std::env::temp_dir().join(format!("tria-{name}-{}.{ext}", commands::new_id()));
    std::fs::write(&path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

/// Hand the terminal to `external`, wait for it, and take the terminal back.
fn run_external(terminal: &mut ratatui::DefaultTerminal, external: &ExternalCommand) -> Result<()> {
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste
    );
    ratatui::restore();
    // crossterm's reader thread may be blocked waiting for input on our behalf and would
    // swallow the child's first keystrokes. A window-size signal wakes it with a resize
    // event instead, and it stays idle until the event loop polls again.
    #[cfg(unix)]
    unsafe {
        libc::raise(libc::SIGWINCH);
    }
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(&external.command)
        .current_dir(&external.dir)
        .status();
    *terminal = ratatui::init();
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableBracketedPaste,
        crossterm::event::EnableMouseCapture
    );
    terminal.clear()?;
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => anyhow::bail!("exited with {status}"),
        Err(err) => Err(err.into()),
    }
}

/// tmux session name for a directory: its last path component, with the characters tmux
/// reserves for target syntax replaced.
pub fn tmux_session_name(dir: &str) -> String {
    let base = std::path::Path::new(dir)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| dir.to_string());
    base.chars()
        .map(|c| if c == '.' || c == ':' { '_' } else { c })
        .collect()
}

fn clamp_to(area: Rect, at: Position) -> Position {
    if area.width == 0 || area.height == 0 {
        return at;
    }
    Position::new(
        at.x.clamp(area.x, area.x + area.width - 1),
        at.y.clamp(area.y, area.y + area.height - 1),
    )
}

/// Push text to the system clipboard through OSC 52, which the terminal forwards.
pub fn copy_to_clipboard(text: &str) {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{encoded}\x07");
    let _ = out.flush();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Pinned,
    Active,
    Snoozed,
    Settled,
}

impl Section {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::Active => "active",
            Self::Snoozed => "snoozed",
            Self::Settled => "settled",
        }
    }
}

#[derive(Debug, Clone)]
pub enum SidebarRow {
    Header {
        section: Section,
        count: usize,
        collapsed: bool,
    },
    Thread {
        id: Id,
        parked: bool,
    },
}

// ── Event loop ─────────────────────────────────────────────────────────

/// External programs tria hands the terminal to, from the config file.
pub struct Launch {
    pub git_command: String,
    pub editor: String,
}

pub async fn run(origin: String, token: String, launch: Launch) -> Result<()> {
    let (handle, mut updates) = session::spawn(origin, token);
    let (events_tx, mut events) = mpsc::unbounded_channel::<AppEvent>();
    let mut app = App::new(handle, events_tx.clone());
    app.git_command = launch.git_command;
    app.editor = launch.editor;

    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableBracketedPaste,
        crossterm::event::EnableMouseCapture
    );
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(TICK);

    let result: Result<()> = loop {
        if let Err(err) = terminal.draw(|frame| ui::draw(frame, &mut app)) {
            break Err(err.into());
        }
        app.flush_clipboard();
        let event = tokio::select! {
            ev = input.next() => match ev {
                Some(Ok(ev)) => AppEvent::Terminal(ev),
                Some(Err(err)) => break Err(err.into()),
                None => break Ok(()),
            },
            update = updates.recv() => match update {
                Some(update) => AppEvent::Update(Box::new(update)),
                None => break Ok(()),
            },
            Some(ev) = events.recv() => ev,
            _ = tick.tick() => AppEvent::Tick,
        };
        match event {
            AppEvent::Terminal(Event::Key(key)) => app.on_key(key),
            AppEvent::Terminal(Event::Mouse(mouse)) => app.on_mouse(mouse),
            AppEvent::Terminal(Event::Paste(text)) => {
                if app.mode == Mode::Insert {
                    app.composer.insert_str(&text);
                } else if app.mode == Mode::Picker {
                    if let Some(p) = app.picker.as_mut() {
                        p.query.push_str(text.trim());
                    }
                } else if app.mode == Mode::Command {
                    app.command_line.push_str(text.trim());
                }
            }
            AppEvent::Terminal(_) => {}
            AppEvent::Tick => {
                app.spinner = app.spinner.wrapping_add(1);
                app.poll_popup();
                if app
                    .toast
                    .as_ref()
                    .is_some_and(|(_, at, _)| at.elapsed() > TOAST_TTL)
                {
                    app.toast = None;
                }
            }
            AppEvent::Update(update) => app.on_update(*update),
            AppEvent::Dispatched(Err(error)) => app.toast(format!("command failed: {error}"), true),
            AppEvent::Dispatched(Ok(())) => {}
            AppEvent::Called { result, ok } => match result {
                Ok(()) => app.toast(ok, false),
                Err(error) => app.toast(error, true),
            },
        }
        if let Some(external) = app.pending_external.take() {
            if let Err(err) = run_external(&mut terminal, &external) {
                app.toast(format!("{}: {err}", external.command), true);
            }
            if let Some(then) = external.then {
                app.follow_up(then);
            }
        }
        if app.quit {
            break Ok(());
        }
    };

    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste
    );
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::tmux_session_name;

    #[test]
    fn search_matching_is_smartcase() {
        use super::{line_matches, match_ranges};
        assert!(line_matches("Nx cache", "nx"));
        assert!(!line_matches("nx cache", "Nx"));
        assert_eq!(match_ranges("a nx b NX", "nx"), vec![(2, 4), (7, 9)]);
        assert_eq!(match_ranges("ÄÖ nx", "nx"), vec![(5, 7)]);
        assert!(match_ranges("abc", "").is_empty());
    }

    #[test]
    fn session_name_is_the_directory_basename() {
        assert_eq!(
            tmux_session_name("/home/me/work/pnpm-hoisted"),
            "pnpm-hoisted"
        );
        assert_eq!(tmux_session_name("/home/me/work/app.v2/"), "app_v2");
        assert_eq!(tmux_session_name("main"), "main");
    }
}

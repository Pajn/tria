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
    picture,
    question::QuestionDraft,
    session::{self, Handle, Status, Update},
    state::{ApprovalOption, PendingApproval, Shell, ThreadState},
    ui, vim,
};

/// Size used when restarting a terminal; the desktop app resizes when it attaches.
const TERMINAL_COLS: u16 = 120;
const TERMINAL_ROWS: u16 = 30;

/// The terminal `g!` reuses, one per thread.
const SHELL_TERMINAL_ID: &str = "tria-shell";

/// Draft key for a thread that does not exist yet.
const NEW_THREAD_DRAFT_KEY: &str = "\0new-thread";

/// Rows moved per mouse wheel notch.
const MOUSE_SCROLL_LINES: usize = 3;
/// The levels the work folds have: the groups of tool calls, and what each call in an
/// open group kept — its input, its output, the files it touched. Opening past this is
/// opening what is already open.
const MOST_OPEN_LEVELS: u8 = 2;
const TICK: Duration = Duration::from_millis(120);

/// How many events waiting at once are applied before the screen is drawn. Enough that a
/// picture arriving in the terminal pane is one screen rather than a hundred, and few
/// enough that a stream which never stops still gets drawn.
const BATCH: usize = 512;
const TOAST_TTL: Duration = Duration::from_secs(6);

/// How often to look for worktrees that have gone from the disk. They only go when
/// something removes one, which is rare and is usually this.
const WORKTREE_REFRESH: Duration = Duration::from_secs(60);

/// How often to ask again for a thread stream that stopped. The supervisor gives up
/// after its own few tries, which is right for a stream that has just gone; what it
/// cannot know is whether the server is back in a minute, and a conversation left
/// frozen for the rest of the session is the one answer that is always wrong.
const STREAM_RETRY: Duration = Duration::from_secs(10);

/// How often to re-read the open thread's checkout. The server does not report edits
/// made outside it, so the working tree counts would otherwise sit still.
const VCS_REFRESH: Duration = Duration::from_secs(20);

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
    /// Listing what the providers' subscriptions have left.
    Usage,
    /// Listing the worktrees the server made for threads, to be rid of them.
    Worktrees,
    /// Listing the subagents the thread has run, with their state and their reports.
    Agents,
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
#[derive(Debug)]
pub struct SearchInput {
    /// A one-line editor rather than a string, for the cursor and the keys that move it.
    pub query: Composer,
    pub backward: bool,
    origin: usize,
}

/// A `Tab` completion running on the command line.
#[derive(Debug)]
pub struct Completing {
    /// Where the word being completed starts, in characters from the start of the line.
    at: usize,
    /// The word as it was typed, to come back to after the last candidate.
    typed: String,
    pub options: Vec<String>,
    /// Which candidate is in the line; one past the last means the typed word is back.
    pub index: usize,
}

/// What the server calls the ways a thread is allowed to act on its own.
const RUNTIME_MODES: &[&str] = &[
    "approval-required",
    "auto-accept-edits",
    "auto",
    "full-access",
];

/// The commands `Tab` offers, one name apiece: the aliases run but are not suggested,
/// since a list with two names for the same thing is a longer list and no more use.
const COMMANDS: &[&str] = &[
    "agents",
    "answer",
    "approve",
    "archive",
    "delete!",
    "dismiss",
    "edit",
    "effort",
    "git",
    "help",
    "mode",
    "model",
    "new",
    "older",
    "perm",
    "pr",
    "project",
    "q",
    "reconnect",
    "rename",
    "reveal",
    "settle",
    "settled",
    "shell",
    "sidebar",
    "split",
    "stop",
    "stop!",
    "tasks",
    "terminals",
    "tmux",
    "unsettle",
    "usage",
    "view",
    "wake",
    "window",
    "worktree",
    "worktrees",
];

/// Where the word the cursor is in begins, in characters from the start of the line.
fn word_start(line: &str, cursor: usize) -> usize {
    line.chars()
        .take(cursor)
        .collect::<Vec<char>>()
        .iter()
        .rposition(|c| c.is_whitespace())
        .map(|i| i + 1)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// Keys edit the composer with Vim motions; the chat scrolls with Ctrl keys.
    Composer,
    /// A cursor moves through the conversation, a line and a character at a time.
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
    /// What is being searched for, or the new title while a project is being renamed.
    /// A one-line editor rather than a string, for the cursor and the keys that move it.
    pub query: Composer,
    pub selected: usize,
    pub items: Vec<PickerItem>,
    /// The project being renamed, while one is: the query line is its new title.
    pub renaming: Option<Id>,
}

impl Picker {
    pub fn filtered(&self) -> Vec<&PickerItem> {
        // While a name is being typed the list is not being searched, and a list that
        // reshuffled under the row being renamed would be reshuffling for nothing.
        if self.renaming.is_some() {
            return self.items.iter().collect();
        }
        let query = self.query.text().to_lowercase();
        let mut scored: Vec<(i64, &PickerItem)> = self
            .items
            .iter()
            .filter_map(|item| fuzzy_score(&query, &item.label, &item.detail).map(|s| (s, item)))
            .collect();
        if !query.is_empty() {
            scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        }
        scored.into_iter().map(|(_, item)| item).collect()
    }
}

/// Fold a paste onto one line, for the fields that are one line. Cutting it off at the
/// first newline would silently lose the rest of what was pasted, and a query or a
/// command spread over lines was never going to be typed that way anyway.
fn one_line(text: &str) -> String {
    text.lines()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
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

/// A thread asked for and not yet made. Kept whole so a refusal can put back what was
/// typed: the message is otherwise gone, and the view is left on a thread that will
/// never exist.
struct PendingCreate {
    thread_id: Id,
    draft: NewThreadDraft,
    text: String,
}

/// The last two parts of a path, which is what tells one worktree from another.
fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

/// A worktree the server made for a thread. Its own checkout, its own branch, and once
/// the thread is done with it, a directory nobody will open again.
pub struct ThreadWorktree {
    pub thread_id: Id,
    pub title: String,
    pub project: String,
    /// The repository the worktree belongs to, which is what git is asked from.
    pub project_cwd: String,
    pub path: String,
    pub branch: Option<String>,
    pub settled: bool,
    pub running: bool,
    /// What the checkout has that is not committed, once the server has said. `None`
    /// until then, and for a worktree it could not read.
    pub changes: Option<bool>,
    /// The files behind that verdict, which is what a forced removal would take.
    pub files: Vec<crate::model::VcsFile>,
}

/// A forced removal waiting to be agreed to. Removing a worktree is ordinarily safe —
/// the branch stays, so nothing committed is lost — and git refuses one with anything
/// uncommitted in it, which is the check worth having. Overriding that check is the one
/// thing in this list that destroys work, and untracked files are not anywhere else, so
/// it says what it would take before it takes it.
pub struct WorktreeConfirm {
    /// The worktree, looked up again each frame so what is listed stays the live list.
    pub path: String,
    pub offset: usize,
}

/// A thread being composed that does not exist on the server yet.
#[derive(Debug, Clone)]
pub struct NewThreadDraft {
    pub project_id: Id,
    pub model_selection: ModelSelection,
    pub runtime_mode: String,
    pub interaction_mode: String,
    /// Start the thread in a fresh worktree rather than the project's own checkout.
    pub worktree: bool,
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
    /// The server has made a thread a new message asked for, so there is now something
    /// to subscribe to.
    ThreadCreated(Id),
    /// What a worktree has uncommitted, once the server has looked.
    WorktreeChecked {
        path: String,
        changes: bool,
        files: Vec<crate::model::VcsFile>,
    },
    /// A worktree was removed, or was not.
    WorktreeRemoved {
        path: String,
        result: Result<(), String>,
    },
    /// The server would not make the thread a new message asked for.
    CreateRefused {
        thread_id: Id,
        error: String,
    },
    /// The server would not take a message sent to a thread that already exists. The
    /// text comes back with it: it left the composer when it was sent.
    SendRefused {
        thread_id: Id,
        text: String,
        error: String,
    },
    /// The config was read again for the usage window, or was not.
    UsageRead(Result<Box<ServerConfig>, String>),
    /// A non-command RPC finished; `ok` is the toast for the success case.
    Called {
        result: Result<(), String>,
        ok: String,
    },
    /// A project's icon, or the news that it has none.
    Favicon {
        project: Id,
        bytes: Option<Vec<u8>>,
    },
    /// A subagent's transcript came back from the machine that ran it.
    Transcript {
        agent_id: String,
        result: Result<TranscriptFile, String>,
    },
}

/// The open thread's stream stopped and could not be got back. What is drawn is as far
/// as it got, which looks exactly like a conversation nobody is writing to, so this
/// lasts as long as the trouble does rather than the six seconds a toast gets.
pub struct LostStream {
    /// The thread it happened to. A thread left since is not the one on the screen.
    pub thread_id: Id,
    pub error: String,
    /// When to ask for the stream again.
    retry_at: Instant,
}

/// A transcript as the server read it off disk.
#[derive(Debug)]
pub struct TranscriptFile {
    pub contents: String,
    /// The server stops reading at a megabyte; the tail of a long run is then missing.
    pub truncated: bool,
}

/// A subagent's own conversation, open over the thread's.
pub struct Transcript {
    pub agent_id: String,
    pub title: String,
    pub subtitle: String,
    /// The transcript rendered as a thread, so the chat draws it like any other.
    pub state: ThreadState,
    pub truncated: bool,
    /// Whether the agent was still working when this was read, so what is shown is as
    /// far as it had got rather than the whole run.
    pub live: bool,
    /// Where the conversation underneath was, to put it back on the way out.
    restore: (Scroll, usize, Focus),
}

pub struct App {
    pub handle: Handle,
    events: mpsc::UnboundedSender<AppEvent>,
    pub shell: Shell,
    /// Threads that have said something since anybody last had them open. This run
    /// only: which threads you have read is not worth keeping on the disk, and a tria
    /// that has just started has nothing to tell you about yet.
    pub unseen: HashSet<Id>,
    /// What each thread looked like when it was last taken in, for telling a thread
    /// that has moved on from one the server merely mentioned again.
    marks: HashMap<Id, String>,
    pub config: ServerConfig,
    /// Whether the config is being read again for the usage window, so that holding `r`
    /// asks once rather than once a keypress.
    usage_pending: bool,
    pub status: Status,
    pub thread: Option<ThreadState>,
    pub current_thread_id: Option<Id>,
    pub draft: Option<NewThreadDraft>,
    pub mode: Mode,
    pub focus: Focus,
    pub composer: Composer,
    /// The `:` line. A one-line editor rather than a string, so that it has the keys
    /// and the history every other field in the client has.
    pub command_line: Composer,
    /// The `Tab` completion running on the command line, if one is.
    pub completing: Option<Completing>,
    pub picker: Option<Picker>,
    pub question: Option<QuestionDraft>,
    /// The one-line field a custom answer is typed into.
    pub custom_answer: Composer,
    pub sidebar_visible: bool,
    /// Index into `sidebar_rows()`.
    pub sidebar_selected: usize,
    /// First visible line of the help, which is taller than most terminals.
    pub help_offset: usize,
    /// Rows the help fits and rows it has, filled by the renderer each frame.
    pub help_viewport: (usize, usize),
    pub show_settled: bool,
    pub show_snoozed: bool,
    pub scroll: Scroll,
    pub expanded: HashSet<String>,
    /// How many levels of the work folds stand open whatever anybody folded by hand:
    /// none, the groups, or the groups and every call inside them. `zr` and `zm` step it
    /// a level at a time, `zR` and `zM` go straight to the ends.
    pub open_levels: u8,
    pub toast: Option<(String, Instant, bool)>,
    pub spinner: usize,
    /// The `g` or `z` waiting for the key that completes it, and when it was pressed.
    pub(crate) pending_prefix: Option<(char, Instant)>,
    /// Filled by the renderer each frame so key handling can page correctly.
    pub chat_viewport: (usize, usize),
    /// First visible sidebar row; the renderer reads and clamps it.
    pub sidebar_offset: usize,
    /// Set by keyboard navigation so the renderer scrolls the selection into view.
    pub sidebar_reveal: bool,
    /// Screen regions from the last frame, for mouse hit testing.
    pub sidebar_inner: Option<Rect>,
    pub chat_area: Rect,
    /// Where the composer's text is drawn, so a click can be turned into a cursor.
    pub composer_area: Rect,
    /// Where the server is, for the files it serves over HTTP rather than the socket.
    pub origin: String,
    /// The icon each project is known by, once asked for. `None` where the server found
    /// the project none.
    favicons: HashMap<Id, Option<Vec<u8>>>,
    /// Line the chat cursor is on, as a content line index. Tracks the last line while
    /// the view follows new output.
    pub chat_cursor: usize,
    /// Character the chat cursor is on, held where it was so a shorter line in passing
    /// does not lose the column.
    pub chat_column: usize,
    /// Where a visual selection in the chat began.
    pub chat_visual: Option<ChatAnchor>,
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
    /// Selection in the subagent roster.
    pub agent_selected: usize,
    /// The subagent transcript being read, in place of the thread's conversation.
    pub transcript: Option<Transcript>,
    /// The subagent whose transcript is on its way, so the row can say so and a second
    /// keypress does not ask for it twice.
    pub transcript_loading: Option<String>,
    /// The attached terminal, when one is open.
    pub pane: Option<crate::term::Pane>,
    /// A command to type into the pane once its shell reports for duty, for `gl`.
    pending_pane_command: Option<String>,
    /// Where the pane's screen is drawn, for turning mouse positions into cells.
    pub pane_area: Rect,
    /// The open thread's checkout, for the branch the thread list does not carry.
    pub vcs: Option<crate::model::VcsLocal>,
    /// How that checkout stands against its upstream.
    pub vcs_remote: Option<crate::model::VcsRemote>,
    /// The directory the watch is on, so it only resubscribes when the thread moves.
    vcs_cwd: Option<String>,
    /// Set between asking for a thread and hearing whether it was made.
    pending_create: Option<PendingCreate>,
    /// Set when the open thread stopped receiving updates, until they come back.
    pub lost_stream: Option<LostStream>,
    /// The worktree list as it was when it was opened, so it does not move under the
    /// cursor while it is being read.
    pub worktrees: Vec<ThreadWorktree>,
    /// How long `g` and `z` wait for the key that completes them. `None` waits for as
    /// long as it takes, which is what the config asks for with `0`.
    pub prefix_timeout: Option<Duration>,
    /// When a thread that has stopped working is announced to the desktop.
    pub notify: crate::notify::When,
    /// Whether the terminal has the focus, as far as it has said so.
    pub terminal_focus: crate::notify::Focus,
    /// What each thread was last doing, so that a change can be told from a repeat.
    statuses: HashMap<Id, crate::model::ThreadStatus>,
    /// Set by `Ctrl-v` in insert mode: the next key goes in as a character.
    pub literal_next: bool,
    /// Set while a forced removal is waiting to be agreed to.
    pub worktree_confirm: Option<WorktreeConfirm>,
    /// Set while `S` in the task list is waiting to be agreed to. Stopping the session
    /// is not stopping a task: everything the agent has running goes with it, and it
    /// sits one shifted keystroke away from the `s` that stops the one task.
    pub confirm_stop_session: bool,
    pub worktree_selected: usize,
    /// The worktrees that are still on the disk, so the sidebar can mark the threads
    /// holding one without asking the disk about every row it draws.
    live_worktrees: HashSet<String>,
    /// `None` until the first look, so the sidebar is marked as soon as there is a
    /// thread list to mark rather than a minute later.
    worktrees_checked: Option<Instant>,
    /// Whether the server's disk is this one.
    pub local_disk: bool,
    /// Whether the open thread was running at the last update, to notice it finishing.
    was_running: bool,
    /// When the checkout was last re-read.
    vcs_refreshed: Instant,
    /// The model to start the next new thread with, remembered from the last choice.
    new_thread_model: Option<ModelSelection>,
    /// Project directory and base branch for a new thread's worktree, resolved as the
    /// message is sent.
    draft_worktree: Option<(String, String)>,
    /// Links on screen, rebuilt each draw, so a click knows what it landed on.
    pub links: Vec<Link>,
    /// Programs bound to `g` and a key, from the config file.
    pub programs: Vec<crate::config::Program>,
    /// How much of the sidebar one thread is given, from the config file.
    pub sidebar_layout: crate::config::SidebarLayout,
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
    /// The directory `tria open` named, held until there is a project for it: the list
    /// of projects arrives after the screen does, and a project tria asks for arrives
    /// after that again.
    open_at: Option<String>,
    /// Whether the project for `open_at` has been asked for, so it is asked for once.
    open_asked: bool,
    pub work_ranges: Vec<crate::timeline::Region>,
    /// Content-line range of every picture a message's own markdown put in the chat. A
    /// work row is found by the region it folds; a message folds nothing, so its pictures
    /// are found by the lines they were given.
    pub picture_ranges: Vec<crate::timeline::Region>,
    /// Every picture the open rows have, under the key the row that has it is keyed by.
    /// Filled by the renderer with each frame, like the regions above it.
    pub chat_pictures: Vec<(String, crate::timeline::Picture)>,
    quit: bool,
}

impl App {
    pub fn new(handle: Handle, events: mpsc::UnboundedSender<AppEvent>) -> Self {
        Self {
            handle,
            events,
            shell: Shell::default(),
            unseen: HashSet::new(),
            marks: HashMap::new(),
            config: ServerConfig::default(),
            usage_pending: false,
            status: Status::Connecting,
            thread: None,
            current_thread_id: None,
            draft: None,
            mode: Mode::Normal,
            focus: Focus::Composer,
            composer: Composer::new(),
            command_line: Composer::new(),
            completing: None,
            picker: None,
            question: None,
            custom_answer: Composer::new(),
            sidebar_visible: true,
            sidebar_selected: 0,
            help_offset: 0,
            help_viewport: (0, 0),
            show_settled: false,
            show_snoozed: true,
            scroll: Scroll::Follow,
            expanded: HashSet::new(),
            open_levels: 0,
            toast: None,
            spinner: 0,
            pending_prefix: None,
            chat_viewport: (0, 0),
            sidebar_offset: 0,
            sidebar_reveal: false,
            sidebar_inner: None,
            chat_area: Rect::default(),
            composer_area: Rect::default(),
            origin: String::new(),
            favicons: HashMap::new(),
            chat_cursor: 0,
            chat_column: 0,
            chat_visual: None,
            chat_count: None,
            message_starts: Vec::new(),
            search: None,
            search_input: None,
            terminals: Vec::new(),
            terminal_selected: 0,
            agent_selected: 0,
            transcript: None,
            transcript_loading: None,
            pane: None,
            pending_pane_command: None,
            pane_area: Rect::default(),
            vcs: None,
            vcs_remote: None,
            vcs_cwd: None,
            pending_create: None,
            lost_stream: None,
            worktrees: Vec::new(),
            prefix_timeout: Some(crate::config::DEFAULT_PREFIX_TIMEOUT),
            notify: crate::notify::When::default(),
            terminal_focus: crate::notify::Focus::default(),
            statuses: HashMap::new(),
            literal_next: false,
            worktree_confirm: None,
            confirm_stop_session: false,
            worktree_selected: 0,
            live_worktrees: HashSet::new(),
            worktrees_checked: None,
            local_disk: false,
            was_running: false,
            vcs_refreshed: Instant::now(),
            new_thread_model: None,
            draft_worktree: None,
            links: Vec::new(),
            drafts: HashMap::new(),
            programs: vec![crate::config::Program {
                key: 'l',
                command: crate::config::DEFAULT_GIT_COMMAND.to_string(),
            }],
            sidebar_layout: crate::config::SidebarLayout::default(),
            editor: "nvim".to_string(),
            pending_external: None,
            popup: None,
            block_ranges: Vec::new(),
            selection: None,
            clipboard_pending: None,
            open_at: None,
            open_asked: false,
            work_ranges: Vec::new(),
            picture_ranges: Vec::new(),
            chat_pictures: Vec::new(),
            quit: false,
        }
    }

    pub fn toast(&mut self, message: impl Into<String>, is_error: bool) {
        let message = message.into();
        // A toast is gone in seconds and the next one takes its place, so the one thing
        // said about a failure is also the first thing lost. Trouble is kept.
        if is_error {
            tracing::info!(%message, "toast");
        }
        self.toast = Some((message, Instant::now(), is_error));
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

    /// How many lines a sidebar row is drawn in. One each, until the two-line layout
    /// gives a thread a second for the branch it is on; a section header keeps its one
    /// wherever it is.
    pub fn sidebar_row_height(&self, row: &SidebarRow) -> usize {
        match (self.sidebar_layout, row) {
            (crate::config::SidebarLayout::TwoLine, SidebarRow::Thread { .. }) => 2,
            _ => 1,
        }
    }

    /// The row drawn `line` lines down the list, for a click to land on.
    pub fn sidebar_row_at(&self, rows: &[SidebarRow], line: usize) -> Option<usize> {
        let mut bottom = 0;
        for (index, row) in rows.iter().enumerate().skip(self.sidebar_offset) {
            bottom += self.sidebar_row_height(row);
            if line < bottom {
                return Some(index);
            }
        }
        None
    }

    /// The furthest the list can be scrolled: the first row that still leaves every row
    /// after it room to be drawn.
    pub fn sidebar_max_offset(&self, rows: &[SidebarRow], height: usize) -> usize {
        let mut used = 0;
        for (index, row) in rows.iter().enumerate().rev() {
            used += self.sidebar_row_height(row);
            if used > height {
                return index + 1;
            }
        }
        0
    }

    /// An offset that has `selected` on screen, moving the list as little as it takes.
    pub fn sidebar_offset_showing(
        &self,
        rows: &[SidebarRow],
        selected: usize,
        height: usize,
    ) -> usize {
        let mut offset = self.sidebar_offset.min(selected);
        while offset < selected {
            let used: usize = rows[offset..=selected]
                .iter()
                .map(|row| self.sidebar_row_height(row))
                .sum();
            if used <= height {
                break;
            }
            offset += 1;
        }
        offset
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
        if let Some(leaving) = self.current_thread_id.clone() {
            self.release_popup_terminals(&leaving);
        }
        // A transcript belongs to the thread that ran the subagent; it does not follow.
        self.transcript = None;
        self.transcript_loading = None;
        self.current_thread_id = Some(thread_id.to_string());
        self.lost_stream = None;
        self.unseen.remove(thread_id);
        self.thread = None;
        self.draft = None;
        self.question = None;
        if matches!(self.mode, Mode::Question | Mode::QuestionCustom) {
            self.mode = Mode::Normal;
        }
        self.scroll = Scroll::Follow;
        self.expanded.clear();
        // The view moving is the one thing a report of trouble always mentions and the
        // one thing the screen keeps no record of.
        tracing::info!(thread = %thread_id, "opening thread");
        // The watch goes with the status: where the thread being opened turns out to
        // sit in the directory the last one did, nothing else would ask for a status
        // to replace the one just dropped, and a quiet checkout sends none by itself.
        self.vcs = None;
        self.vcs_remote = None;
        self.vcs_cwd = None;
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

    /// The project a directory belongs to: the one whose checkout it is, or failing
    /// that the innermost one it sits inside. Opening `src/` of a project is opening
    /// that project, not asking for a second one alongside it.
    fn project_for(&self, root: &str) -> Option<Id> {
        let inside = |project_root: &str| {
            root == project_root
                || root.starts_with(project_root)
                    && root[project_root.len()..].starts_with(std::path::MAIN_SEPARATOR)
        };
        self.shell
            .projects
            .values()
            .filter(|project| inside(&project.workspace_root))
            .max_by_key(|project| project.workspace_root.len())
            .map(|project| project.id.clone())
    }

    /// What `tria open` asked for, once the project list has arrived: a new thread in
    /// the project the directory belongs to. Where it belongs to none, the project is
    /// made first — for the checkout the directory is in, since that is what a project
    /// usually is — and this runs again when the server sends it back.
    fn open_where_asked(&mut self) {
        let Some(root) = self.open_at.clone() else {
            return;
        };
        if let Some(project) = self.project_for(&root) {
            self.open_at = None;
            self.open_asked = false;
            self.start_new_thread(&project);
            return;
        }
        if self.open_asked {
            return;
        }
        self.open_asked = true;
        let root = crate::workspace::checkout_root(&root).unwrap_or(root);
        let title = crate::workspace::title(&root);
        tracing::info!(%root, %title, "adding a project");
        self.toast(format!("adding {title}…"), false);
        self.dispatch(crate::commands::project_create(
            &crate::commands::new_id(),
            &title,
            &root,
        ));
    }

    fn start_new_thread(&mut self, project_id: &str) {
        let project = self.shell.projects.get(project_id);
        // The last model picked wins: it is the most recent statement of intent.
        let model_selection = self
            .new_thread_model
            .clone()
            .or_else(|| self.config.settings.model_selection(project))
            .or_else(|| self.first_usable_model());
        let Some(model_selection) = model_selection else {
            self.toast("no usable provider or model configured on the server", true);
            return;
        };
        // The project's setting wins, then the server's; the server's own default is
        // the current checkout.
        let env_mode = self.config.settings.thread_env_mode(project);
        self.swap_composer_draft(NEW_THREAD_DRAFT_KEY);
        self.draft = Some(NewThreadDraft {
            project_id: project_id.to_string(),
            worktree: env_mode.as_deref() == Some("worktree"),
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
        self.sync_vcs_watch();
        self.scroll = Scroll::Follow;
        self.mode = Mode::Insert;
        self.focus = Focus::Composer;
    }

    /// How much context the open thread would leave behind by compacting before it
    /// carries on, when that is worth saying. The provider has to have a `/compact` to
    /// send, since that is what the offer amounts to.
    pub fn resume_with_less(&self) -> Option<u64> {
        let thread = self.thread.as_ref()?;
        let used = thread.resume_with_less(&commands::now_iso())?;
        let instance = &thread.detail.shell.model_selection.instance_id;
        self.config
            .providers
            .iter()
            .find(|provider| &provider.instance_id == instance)?
            .slash_commands
            .iter()
            .any(|command| command.name == "compact")
            .then_some(used)
    }

    /// Take a thread in as the list now has it, and say whether it has spoken since it
    /// was last looked at.
    ///
    /// Only what the list itself shows is compared: the turn the thread is on, how that
    /// turn ended, and when anybody last wrote to it. A thread that has only just been
    /// heard of is not news, or every thread would be unseen the moment tria connects.
    fn note_thread(&mut self, thread: &crate::model::ThreadShell) {
        let mark = mark_of(thread);
        let moved_on = self
            .marks
            .insert(thread.id.clone(), mark.clone())
            .is_some_and(|known| known != mark);
        // The open thread is being read as it arrives, and a thread still working says
        // so with its own glyph — what it has to show is not there yet.
        if moved_on
            && self.current_thread_id.as_deref() != Some(thread.id.as_str())
            && thread.status() != crate::model::ThreadStatus::Working
        {
            self.unseen.insert(thread.id.clone());
        }
        self.note_status(thread);
    }

    /// Say out loud that a thread has stopped working, to somebody who is not looking
    /// at it. Which thread it is does not come into it — the open one finishing while
    /// you are in another window is the case this is for — but the change does: a
    /// thread already finished when tria first heard of it has nothing to announce, and
    /// a status that has not moved is the same thread saying the same thing again.
    fn note_status(&mut self, thread: &crate::model::ThreadShell) {
        let status = thread.status();
        let Some(was) = self.statuses.insert(thread.id.clone(), status) else {
            // First sight. Every thread would announce itself on the way in.
            return;
        };
        let stopped_working = was == crate::model::ThreadStatus::Working
            && status != crate::model::ThreadStatus::Working;
        if !stopped_working || !self.notify.wants(self.terminal_focus) {
            return;
        }
        let title: String = thread.title.chars().take(60).collect();
        let title = if title.is_empty() { "a thread" } else { &title };
        crate::notify::send(&format!("{title} · {}", status.notice()));
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
        self.dispatch_opening(command, None);
    }

    /// Dispatch a command and, when it is a command that makes a thread, say so once the
    /// server has applied it. A thread is the command's doing, so there is nothing to
    /// subscribe to until the command has been through: asking first is asking for a
    /// thread nobody has yet. A command that fails makes nothing, and says so instead.
    fn dispatch_opening(&self, command: Value, creates: Option<Id>) {
        let handle = self.handle.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let result = handle
                .dispatch(command)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string());
            match (&result, creates) {
                (Ok(()), Some(thread_id)) => {
                    let _ = events.send(AppEvent::ThreadCreated(thread_id));
                }
                (Err(error), Some(thread_id)) => {
                    let _ = events.send(AppEvent::CreateRefused {
                        thread_id,
                        error: error.clone(),
                    });
                    return;
                }
                _ => {}
            }
            let _ = events.send(AppEvent::Dispatched(result));
        });
    }

    /// Send a message to a thread that already exists, keeping hold of the text until
    /// the server has taken it. The composer is emptied when a message goes, because
    /// that is what sending looks like; a message the server refuses has to come back,
    /// or the only copy of it is one keypress of history away and nothing says so.
    fn dispatch_message(&self, command: Value, thread_id: Id, text: String) {
        let handle = self.handle.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            match handle.dispatch(command).await {
                Ok(_) => {}
                Err(error) => {
                    let _ = events.send(AppEvent::SendRefused {
                        thread_id,
                        text,
                        error: error.to_string(),
                    });
                }
            }
        });
    }

    /// A message that was not sent goes back where it was written: into the composer if
    /// that is still where you are and nothing has been written since, and into the
    /// thread's parked draft if you have gone elsewhere. Where neither is free, what was
    /// typed is not lost — it is in the composer's history — and the toast says so
    /// rather than writing over whatever took its place.
    fn on_send_refused(&mut self, thread_id: Id, text: String, error: String) {
        let here = self.current_thread_id.as_deref() == Some(thread_id.as_str());
        if here && self.composer.is_empty() {
            self.composer.set_text(&text);
            // Sending leaves you in insert mode with an empty composer, which is where
            // the text goes back to. Anywhere else — a list, a pane, the chat — the view
            // is left where it is and the text is simply there when you come back to it.
            if matches!(self.mode, Mode::Normal | Mode::Insert) {
                self.focus = Focus::Composer;
            }
            if self.mode != Mode::Insert {
                self.composer.clamp_normal();
            }
            self.toast(format!("not sent: {error}"), true);
            return;
        }
        if !here && !self.drafts.contains_key(thread_id.as_str()) {
            self.drafts.insert(thread_id.to_string(), text);
            self.toast(
                format!("not sent: {error} · the message is back in that thread"),
                true,
            );
            return;
        }
        self.toast(
            format!("not sent: {error} · it is in the composer's history (Ctrl-p)"),
            true,
        );
    }

    /// Subscribe to a thread the server has just made, unless the view has moved on in
    /// the meantime: a thread opened since the message was sent is the one wanted.
    fn on_thread_created(&mut self, thread_id: Id) {
        self.pending_create.take();
        if self.current_thread_id.as_deref() == Some(thread_id.as_str()) {
            self.handle.open_thread(&thread_id);
        }
    }

    /// The server would not make the thread. The view is on one that does not exist and
    /// the message has left the composer, so both go back: what was typed is the work,
    /// and it is the only copy. Staying on the draft also keeps the view still, rather
    /// than falling through to whichever thread happens to be first in the list.
    fn on_create_refused(&mut self, thread_id: Id, error: String) {
        let pending = self
            .pending_create
            .take()
            .filter(|pending| pending.thread_id == thread_id);
        let Some(pending) = pending else {
            self.toast(error, true);
            return;
        };
        // Unless the view has moved on by itself, in which case it is where it is meant
        // to be and the message is still in the composer's history.
        if self.current_thread_id.as_deref() == Some(thread_id.as_str()) {
            self.current_thread_id = None;
            self.thread = None;
            self.draft = Some(pending.draft);
            self.composer.set_text(&pending.text);
            self.handle.close_thread();
            self.sync_vcs_watch();
            self.mode = Mode::Insert;
            self.focus = Focus::Composer;
        }
        self.toast(error, true);
    }

    fn send_message(&mut self) {
        let text = self.composer.text().trim_end().to_string();
        if text.trim().is_empty() {
            return;
        }
        // The message joins the conversation, so that is what to be looking at.
        self.leave_transcript();
        // Read the slot before sending: starting a new thread moves the view to it, and the
        // parked text belongs to the slot the message was written in.
        let draft_key = self.draft_key();
        if self.draft.is_some() && !self.resolve_draft_worktree() {
            return;
        }
        if let Some(draft) = self.draft.take() {
            // Resolved above, while the draft was still in place.
            let worktree = self.draft_worktree.take();
            let thread_id = commands::new_id();
            let worktree_branch = commands::worktree_branch(&thread_id);
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
                    worktree: worktree.as_ref().map(|(cwd, base)| commands::Worktree {
                        project_cwd: cwd,
                        base_branch: base,
                        branch: &worktree_branch,
                        start_from_origin: self.config.settings.new_worktrees_start_from_origin,
                    }),
                }),
            );
            // The view moves to the new thread now, and reads as loading until the
            // server has made it and the subscription has something to say.
            tracing::info!(
                thread = %thread_id,
                project = %draft.project_id,
                worktree = ?worktree,
                "creating a thread"
            );
            self.current_thread_id = Some(thread_id.clone());
            self.thread = None;
            self.pending_create = Some(PendingCreate {
                thread_id: thread_id.clone(),
                draft,
                text: text.clone(),
            });
            self.dispatch_opening(command, Some(thread_id));
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
            self.dispatch_message(command, thread.id().to_string(), text.clone());
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

    /// Stop what the session is doing. Usually that is the turn, and the turn is named.
    /// But a watcher outlives the turn that started it, and then there is no turn to
    /// name: the interrupt goes without one and the session stops the watch loop, which
    /// is what the desktop's `Monitoring · Stop` sends and the only per-watcher stop
    /// there is.
    fn interrupt(&mut self) {
        let Some(thread) = &self.thread else { return };
        let running = thread.is_running();
        let thread_id = thread.id().to_string();
        let turn_id = running
            .then_some(thread.detail.shell.latest_turn.as_ref())
            .flatten()
            .map(|t| t.turn_id.clone());
        if !running && !self.background_alive() {
            self.toast("nothing running", false);
            return;
        }
        self.dispatch(commands::turn_interrupt(&thread_id, turn_id.as_deref()));
        self.toast(
            if running {
                "interrupting…"
            } else {
                "stopping the background work…"
            },
            false,
        );
    }

    /// Whether a digit answers the approval rather than starting a count. Only with
    /// nothing written: a draft in the composer means the digits are being typed at the
    /// composer, and one of the answers is a permission granted for the whole session.
    /// An approval arrives on its own schedule, so the guard has to be the state of the
    /// composer rather than the timing of the key.
    pub fn digits_answer_approval(&self) -> bool {
        self.composer.is_empty()
    }

    fn respond_approval(&mut self, index: usize) {
        let Some(thread) = &self.thread else { return };
        let pending = thread.pending_approvals();
        let Some(approval) = pending.first() else {
            self.toast("no approval pending", true);
            return;
        };
        let options = approval_options(approval);
        let Some(option) = options.get(index) else {
            self.toast(
                format!("this approval has {} answers to pick from", options.len()),
                true,
            );
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
        // The panel answers a question about the thread, so the keys that go and look at
        // the thread — `gy`, `gE`, a program on `g<key>` — are worth having here too. The
        // panel keeps its own letters: only what follows `g` is read this way.
        let prefix = self.take_prefix();
        if prefix == Some('g') {
            if key.code == KeyCode::Char('e') {
                self.edit_composer();
            } else {
                self.global_prefix_key(key);
            }
            return;
        }
        if key.code == KeyCode::Char('g') && !ctrl {
            self.pending_prefix = Some(('g', Instant::now()));
            return;
        }
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
                    // Picking the field up again puts the cursor after what is there.
                    self.custom_answer.set_text(&draft.current_answer().custom);
                    self.mode = Mode::QuestionCustom;
                } else {
                    self.toast("this question does not accept a custom answer", false);
                }
            }
            KeyCode::Char('d') => self.dismiss_question(),
            KeyCode::Char('?') => self.open_help(),
            _ => {}
        }
    }

    fn on_question_custom_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.custom_answer.clear();
                self.mode = Mode::Question;
            }
            KeyCode::Enter => {
                let text = self.custom_answer.text();
                self.custom_answer.clear();
                if let Some(draft) = self.question.as_mut() {
                    draft.set_custom(text);
                }
                self.mode = Mode::Question;
                self.submit_answers();
            }
            // The field holds one line, so the keys that would leave it are not offered.
            _ => {
                edit_key(&mut self.custom_answer, key);
            }
        }
    }

    fn yank_last_assistant(&mut self) {
        let Some(thread) = self.open_conversation() else {
            return;
        };
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
        // Whatever ran in there probably touched the checkout.
        self.handle.refresh_vcs();
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
                let replies = pane.feed(&data);
                // A full-screen program is up, so there is nothing of the shell left to hide.
                if pane.alternate_screen() {
                    pane.starting = None;
                }
                // Answers to capability queries go back the way keystrokes do.
                for reply in replies {
                    self.write_to_pane(reply);
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
                    let warm = self.popup_terminals().contains(&terminal_id);
                    self.end_terminal(thread_id, terminal_id, warm);
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

    /// Text the terminal handed over in one piece. Every place that takes typing takes
    /// a paste as well: dropping it is the one answer that looks like the app is broken,
    /// because nothing on screen says the keystrokes went nowhere.
    fn on_paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.mode {
            Mode::Insert => self.composer.insert_str(text),
            // Normal mode still has the composer under the cursor, so a paste lands
            // there. The mode is left alone: the keys after a paste should mean what
            // they meant before it.
            Mode::Normal => {
                self.composer.vim_cancel();
                self.composer.insert_str(text);
            }
            Mode::QuestionCustom => self.custom_answer.insert_str(&one_line(text)),
            Mode::Command => self.command_line.insert_str(&one_line(text)),
            Mode::Picker => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.query.insert_str(&one_line(text));
                    if picker.renaming.is_none() {
                        picker.selected = 0;
                    }
                }
            }
            Mode::Search => {
                let Some(input) = self.search_input.as_mut() else {
                    return;
                };
                input.query.insert_str(&one_line(text));
                self.incremental_search();
            }
            Mode::TerminalPane => self.paste_into_pane(text),
            _ => {}
        }
    }

    /// Hand a paste to the program in the attached terminal.
    fn paste_into_pane(&mut self, text: &str) {
        let Some(pane) = self.pane.as_mut() else {
            return;
        };
        // As with typing, a paste is a reason to be looking at the live screen again.
        if pane.scrollback() > 0 {
            pane.scroll(isize::MIN / 2);
        }
        let data = crate::term::encode_paste(text, pane.screen().bracketed_paste());
        self.write_to_pane(data);
    }

    /// Re-read the checkout every so often, so the working tree counts stay true even
    /// when the edits came from somewhere the server is not watching.
    /// What is wrong and will not right itself by being ignored, for the line under
    /// the header. A toast says a thing once and is gone in six seconds; this is for
    /// the trouble that is still true a minute later, where the screen otherwise looks
    /// like everything is fine and simply quiet.
    pub fn trouble(&self) -> Option<String> {
        // The connection first: a thread that has stopped updating is what a connection
        // that has stopped being one looks like from the thread's end, and only one of
        // the two is worth telling somebody about.
        if let Status::Failed(why) = &self.status {
            return Some(why.clone());
        }
        let lost = self.lost_stream.as_ref()?;
        if self.current_thread_id.as_deref() != Some(lost.thread_id.as_str()) {
            return None;
        }
        let error = lost.error.lines().next().unwrap_or(&lost.error);
        let error: String = error.chars().take(96).collect();
        Some(format!(
            "this thread has stopped updating: {error} · asking again"
        ))
    }

    /// Ask for a stream that stopped again, every so often, for as long as the thread
    /// stays open. The supervisor has already given up on it, so nothing else will.
    fn retry_lost_stream(&mut self) {
        let Some(thread_id) = self.current_thread_id.clone() else {
            self.lost_stream = None;
            return;
        };
        let Some(lost) = self.lost_stream.as_mut() else {
            return;
        };
        if lost.thread_id != thread_id {
            self.lost_stream = None;
            return;
        }
        if Instant::now() < lost.retry_at {
            return;
        }
        lost.retry_at = Instant::now() + STREAM_RETRY;
        tracing::info!(thread = %thread_id, "asking for the thread's stream again");
        self.handle.open_thread(&thread_id);
    }

    fn refresh_vcs_periodically(&mut self) {
        if self.vcs_cwd.is_none() || self.vcs_refreshed.elapsed() < VCS_REFRESH {
            return;
        }
        self.vcs_refreshed = Instant::now();
        self.handle.refresh_vcs();
    }

    /// Work out where a new thread's worktree would branch from, reporting why not when
    /// it cannot. Returns false when the message should not be sent.
    fn resolve_draft_worktree(&mut self) -> bool {
        self.draft_worktree = None;
        let Some(draft) = self.draft.as_ref() else {
            return true;
        };
        if !draft.worktree {
            return true;
        }
        let Some(project) = self.shell.projects.get(&draft.project_id) else {
            self.toast("unknown project", true);
            return false;
        };
        // The worktree branches off whatever the project's checkout has now, which is
        // what the watch reports.
        let base = self
            .vcs
            .as_ref()
            .filter(|vcs| vcs.is_repo)
            .and_then(|vcs| vcs.ref_name.clone());
        let Some(base) = base else {
            let message = if self.vcs.is_none() {
                "still reading the project's checkout; send again in a moment"
            } else {
                "no branch to base a worktree on; gw starts in the checkout instead"
            };
            self.toast(message, true);
            return false;
        };
        self.draft_worktree = Some((project.workspace_root.clone(), base));
        true
    }

    /// The link drawn at a screen position, if any.
    fn link_at(&self, at: Position) -> Option<String> {
        self.links
            .iter()
            .find(|link| link.row == at.y && at.x >= link.start && at.x < link.end)
            .map(|link| link.url.clone())
    }

    /// The links on the cursor's line, for opening one without the mouse.
    fn link_at_cursor(&self) -> Option<String> {
        if self.focus != Focus::Chat {
            return None;
        }
        let offset = self.chat_offset();
        let row = self.chat_area.y + self.chat_cursor.checked_sub(offset)? as u16;
        self.links
            .iter()
            .find(|link| link.row == row)
            .map(|link| link.url.clone())
    }

    /// `gw`: start the new thread in a fresh worktree, or in the project's checkout.
    fn toggle_draft_worktree(&mut self) {
        let Some(draft) = self.draft.as_mut() else {
            self.toast("only a new thread can choose; press n first", true);
            return;
        };
        draft.worktree = !draft.worktree;
        let message = if draft.worktree {
            "new thread starts in a fresh worktree"
        } else {
            "new thread starts in the project's checkout"
        };
        self.toast(message, false);
    }

    /// Follow the open thread's checkout, so the header can show its branch. The thread
    /// list only carries a branch for threads the server made one for.
    fn sync_vcs_watch(&mut self) {
        let cwd = self.watch_directory();
        if cwd == self.vcs_cwd {
            return;
        }
        self.vcs = None;
        self.vcs_remote = None;
        self.vcs_cwd = cwd.clone();
        self.handle.watch_vcs(cwd);
    }

    /// The mouse in the pane: to the program when it has asked for it, otherwise the
    /// wheel moves our own scrollback. Shift takes the wheel back from the program,
    /// which is what terminals do.
    fn on_pane_mouse(&mut self, mouse: MouseEvent) {
        let area = self.pane_area;
        let Some(pane) = self.pane.as_mut() else {
            return;
        };
        let at = Position::new(mouse.column, mouse.row);
        if !area.contains(at) {
            return;
        }
        let (col, row) = (mouse.column - area.x, mouse.row - area.y);
        let (mode, encoding) = pane.mouse_protocol();
        let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
        if !shift && let Some(data) = crate::term::encode_mouse(&mouse, col, row, mode, encoding) {
            self.write_to_pane(data);
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => pane.scroll(MOUSE_SCROLL_LINES as isize),
            MouseEventKind::ScrollDown => pane.scroll(-(MOUSE_SCROLL_LINES as isize)),
            _ => {}
        }
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

    /// Close a session tria opened, and for a popup leave a fresh shell waiting in its
    /// place. Opening a popup costs a shell's whole startup — profiles, prompt, whatever
    /// the directory arranges — and none of that is a wait when it is paid with the popup
    /// shut. The two calls share a task because the order is the point: the other way
    /// round the close takes the shell that was just warmed.
    fn end_terminal(&self, thread_id: String, terminal_id: String, warm: bool) {
        let warm = warm.then(|| self.thread_directory()).flatten();
        let (cols, rows) = self.pane_size();
        let handle = self.handle.clone();
        tokio::spawn(async move {
            let _ = handle
                .call(
                    "terminal.close",
                    json!({ "threadId": thread_id, "terminalId": terminal_id }),
                )
                .await;
            let Some(cwd) = warm else { return };
            // Nothing waits on this. A popup opened before the shell is up starts one
            // itself, which is what every opening used to do.
            if let Err(err) = handle
                .call(
                    "terminal.open",
                    json!({
                        "threadId": thread_id,
                        "terminalId": terminal_id,
                        "cwd": cwd,
                        "cols": cols,
                        "rows": rows,
                    }),
                )
                .await
            {
                tracing::info!(%err, "could not leave a shell waiting");
            }
        });
    }

    /// Let go of the popup shells kept warm for a thread being left. They are scratch,
    /// and one per thread visited is a pile of shells nobody asked for. One with
    /// something running in it is not idle and not ours to end.
    fn release_popup_terminals(&self, thread_id: &str) {
        for terminal_id in self.popup_terminals() {
            // By thread as well as by name: every thread has a `tria-git` of its own.
            let idle = self.terminals.iter().any(|t| {
                t.thread_id == thread_id
                    && t.terminal_id == *terminal_id
                    && t.is_live()
                    && !t.has_running_subprocess
            });
            if idle {
                self.end_terminal(thread_id.to_string(), terminal_id, false);
            }
        }
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

    // ── Worktrees ──────────────────────────────────────────────────────

    /// Every worktree the server made for a thread and that is still on the disk. A
    /// thread that runs in the project's own checkout has none, so having one is what
    /// makes it ours to offer; a worktree that is some project's root is not, since a
    /// project is a place to work rather than the leavings of a thread.
    ///
    /// Threads keep the path of a worktree long after it has gone, and most of them have
    /// gone — so where the disk is ours to ask, it is asked. Where it is not, the
    /// server's word is all there is.
    fn collect_worktrees(&self) -> Vec<ThreadWorktree> {
        let roots: Vec<&str> = self
            .shell
            .projects
            .values()
            .map(|p| p.workspace_root.as_str())
            .collect();
        let mut out: Vec<ThreadWorktree> = self
            .shell
            .threads
            .values()
            .filter_map(|thread| {
                let path = thread.worktree_path.clone()?;
                if roots.contains(&path.as_str()) {
                    return None;
                }
                if self.local_disk && !std::path::Path::new(&path).is_dir() {
                    return None;
                }
                let project = self.shell.projects.get(&thread.project_id)?;
                Some(ThreadWorktree {
                    thread_id: thread.id.clone(),
                    title: thread.title.clone(),
                    project: project.title.clone(),
                    project_cwd: project.workspace_root.clone(),
                    path,
                    branch: thread.branch.clone(),
                    settled: thread.is_settled(),
                    running: thread.is_running(),
                    changes: None,
                    files: Vec::new(),
                })
            })
            .collect();
        // The ones there is nothing left to wait for first, since they are the point.
        out.sort_by(|a, b| {
            (!a.settled, &a.project, &a.title).cmp(&(!b.settled, &b.project, &b.title))
        });
        out
    }

    /// Which of the worktrees the server says exist are still there. Threads keep the
    /// path of a worktree that has been removed, so the answer is the disk's — where the
    /// disk is ours to ask. Where it is not, the server's word is all there is.
    fn refresh_live_worktrees(&mut self) {
        self.worktrees_checked = Some(Instant::now());
        self.live_worktrees = self
            .collect_worktrees()
            .into_iter()
            .map(|worktree| worktree.path)
            .collect();
    }

    fn refresh_worktrees_periodically(&mut self) {
        if !self.shell.synchronized {
            return;
        }
        match self.worktrees_checked {
            Some(at) if at.elapsed() < WORKTREE_REFRESH => {}
            _ => self.refresh_live_worktrees(),
        }
    }

    /// Whether a thread is holding a worktree that is still on the disk.
    pub fn holds_worktree(&self, thread: &crate::model::ThreadShell) -> bool {
        thread
            .worktree_path
            .as_deref()
            .is_some_and(|path| self.live_worktrees.contains(path))
    }

    /// Settled threads still holding one, which is the pile worth clearing.
    pub fn settled_worktrees(&self) -> usize {
        self.shell
            .threads
            .values()
            .filter(|thread| thread.is_settled() && self.holds_worktree(thread))
            .count()
    }

    fn open_worktrees(&mut self) {
        self.worktrees = self.collect_worktrees();
        self.worktree_selected = 0;
        if self.worktrees.is_empty() {
            self.toast("no thread has a worktree of its own", false);
            return;
        }
        self.mode = Mode::Worktrees;
        // What each one has uncommitted, asked for all at once: the answer decides
        // whether it can go, and a list of this size is a handful of calls.
        let paths: Vec<String> = self.worktrees.iter().map(|w| w.path.clone()).collect();
        for path in paths {
            self.check_worktree(path);
        }
    }

    /// Ask the server what a worktree is holding. The status behind this is cached and
    /// can be behind the disk, so it is asked rather than read, and it is asked again
    /// before anything is destroyed on the strength of it.
    fn check_worktree(&self, path: String) {
        let handle = self.handle.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let result = handle
                .call("vcs.refreshStatus", json!({ "cwd": path.clone() }))
                .await;
            let Ok(status) = result else { return };
            let Ok(local) = serde_json::from_value::<crate::model::VcsLocal>(status) else {
                return;
            };
            let _ = events.send(AppEvent::WorktreeChecked {
                path,
                changes: local.has_working_tree_changes,
                files: local.working_tree.files,
            });
        });
    }

    fn on_worktrees_key(&mut self, key: KeyEvent) {
        if self.worktree_confirm.is_some() {
            return self.on_worktree_confirm_key(key);
        }
        let count = self.worktrees.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down => {
                self.worktree_selected = (self.worktree_selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.worktree_selected = self.worktree_selected.saturating_sub(1);
            }
            KeyCode::Char('g') => self.worktree_selected = 0,
            KeyCode::Char('G') => self.worktree_selected = count.saturating_sub(1),
            KeyCode::Char('x') | KeyCode::Char('d') => self.remove_worktree(false),
            KeyCode::Char('X') | KeyCode::Char('D') => self.confirm_force_remove(),
            KeyCode::Enter | KeyCode::Char('l') => {
                if let Some(worktree) = self.worktrees.get(self.worktree_selected) {
                    let thread_id = worktree.thread_id.clone();
                    self.mode = Mode::Normal;
                    self.open_thread(&thread_id);
                }
            }
            _ => {}
        }
    }

    /// The box is a question with two answers, and the rest of its keys move through
    /// the list of what would be lost, which can be longer than the box is tall.
    fn on_worktree_confirm_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some((_, worktree)) = self.confirming_worktree() else {
            // The worktree went while the question was up; there is nothing to answer.
            self.worktree_confirm = None;
            return;
        };
        let last = worktree.files.len().saturating_sub(1);
        let Some(confirm) = self.worktree_confirm.as_mut() else {
            return;
        };
        let by = |offset: usize, delta: isize| {
            (offset as isize + delta).clamp(0, last as isize) as usize
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.worktree_confirm = None,
            KeyCode::Enter => {
                if let Some(confirm) = self.worktree_confirm.take() {
                    self.remove_worktree_at(&confirm.path, true);
                }
            }
            KeyCode::Char('j') | KeyCode::Down => confirm.offset = by(confirm.offset, 1),
            KeyCode::Char('k') | KeyCode::Up => confirm.offset = by(confirm.offset, -1),
            KeyCode::Char('d') if ctrl => confirm.offset = by(confirm.offset, 5),
            KeyCode::Char('u') if ctrl => confirm.offset = by(confirm.offset, -5),
            KeyCode::Char('g') | KeyCode::Home => confirm.offset = 0,
            KeyCode::Char('G') | KeyCode::End => confirm.offset = last,
            _ => {}
        }
    }

    /// `X`: the removal git would refuse. Nothing uncommitted means there is nothing
    /// for it to refuse over and nothing to lose, so that one goes straight through —
    /// the question is only worth asking where there is an answer worth having.
    fn confirm_force_remove(&mut self) {
        let Some(worktree) = self.worktrees.get(self.worktree_selected) else {
            return;
        };
        // The reasons it cannot go at all are worth giving before the question rather
        // than after it: agreeing to lose the files and then being told no is a worse
        // conversation than being told no.
        if let Some(refusal) = self.worktree_held(worktree) {
            return self.toast(refusal, true);
        }
        if worktree.changes == Some(false) {
            return self.remove_worktree(true);
        }
        let path = worktree.path.clone();
        // Asked again on the way in: the server's status is cached, and a list of files
        // somebody is about to agree to lose should be the one that is there now.
        self.check_worktree(path.clone());
        self.worktree_confirm = Some(WorktreeConfirm { path, offset: 0 });
    }

    /// Why a worktree is not tria's to remove, whatever git thinks of it.
    fn worktree_held(&self, worktree: &ThreadWorktree) -> Option<&'static str> {
        if worktree.running {
            return Some("that thread is still running");
        }
        // A shell sitting in it would be left in a directory that is not there.
        if self
            .terminals
            .iter()
            .any(|t| t.is_live() && t.cwd == worktree.path)
        {
            return Some("a terminal is open in it");
        }
        None
    }

    /// Hand a worktree back. Without `force` git refuses one with anything uncommitted
    /// in it, which is the check worth having and is git's to make: it counts what is
    /// not tracked as well, which a status does not.
    fn remove_worktree(&mut self, force: bool) {
        if let Some(worktree) = self.worktrees.get(self.worktree_selected) {
            let path = worktree.path.clone();
            self.remove_worktree_at(&path, force);
        }
    }

    /// Remove the worktree at `path`, named rather than pointed at: a question that was
    /// answered about one worktree must not be carried out on another.
    fn remove_worktree_at(&mut self, path: &str, force: bool) {
        let Some(worktree) = self.worktrees.iter().find(|w| w.path == path) else {
            return;
        };
        if let Some(refusal) = self.worktree_held(worktree) {
            self.toast(refusal, true);
            return;
        }
        let path = worktree.path.clone();
        let handle = self.handle.clone();
        let events = self.events.clone();
        let payload = json!({ "cwd": worktree.project_cwd, "path": path, "force": force });
        self.toast(format!("removing {}", short_path(&path)), false);
        tokio::spawn(async move {
            let result = handle
                .call("vcs.removeWorktree", payload)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string());
            let _ = events.send(AppEvent::WorktreeRemoved { path, result });
        });
    }

    fn on_worktree_checked(
        &mut self,
        path: String,
        changes: bool,
        files: Vec<crate::model::VcsFile>,
    ) {
        if let Some(worktree) = self.worktrees.iter_mut().find(|w| w.path == path) {
            worktree.changes = Some(changes);
            worktree.files = files;
        }
    }

    /// The worktree a forced removal is waiting on, if one is.
    pub fn confirming_worktree(&self) -> Option<(&WorktreeConfirm, &ThreadWorktree)> {
        let confirm = self.worktree_confirm.as_ref()?;
        let worktree = self.worktrees.iter().find(|w| w.path == confirm.path)?;
        Some((confirm, worktree))
    }

    fn on_worktree_removed(&mut self, path: String, result: Result<(), String>) {
        match result {
            Ok(()) => {
                self.worktrees.retain(|w| w.path != path);
                self.live_worktrees.remove(&path);
                if self
                    .worktree_confirm
                    .as_ref()
                    .is_some_and(|c| c.path == path)
                {
                    self.worktree_confirm = None;
                }
                self.worktree_selected = self
                    .worktree_selected
                    .min(self.worktrees.len().saturating_sub(1));
                self.toast(format!("removed {}", short_path(&path)), false);
                if self.worktrees.is_empty() && self.mode == Mode::Worktrees {
                    self.mode = Mode::Normal;
                }
            }
            // Git's own refusal, which is a whole paragraph of advice about --force.
            Err(error) if error.contains("modified or untracked") => {
                self.toast("it has uncommitted work in it · X removes it anyway", true)
            }
            Err(error) => self.toast(error, true),
        }
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
        self.confirm_stop_session = false;
    }

    /// What the server says the session is still running after the turn settled:
    /// `working`, `monitoring`, or nothing. The server keeps this register itself,
    /// which is the whole of its worth: a watch loop started hours ago has long since
    /// fallen off the end of the activities this client loaded, so its rows are no
    /// answer to whether it is still going.
    ///
    /// It is read from the thread list, which is where the server puts it. A thread
    /// detail snapshot carries no liveness at all — the open thread only has one once a
    /// list update has been folded into it, which may be long after it was opened.
    pub fn background_liveness(&self) -> Option<&str> {
        self.current_thread_id
            .as_deref()
            .and_then(|id| self.shell.threads.get(id))
            .and_then(|thread| thread.background_liveness.as_deref())
            .or_else(|| {
                self.thread
                    .as_ref()
                    .and_then(|thread| thread.background_liveness())
            })
    }

    /// Whether anything is running in the background: what the server's register says,
    /// or failing that a task in the loaded history with no reported end.
    fn background_alive(&self) -> bool {
        self.background_liveness().is_some()
            || self
                .thread
                .as_ref()
                .is_some_and(|thread| !thread.running_tasks().is_empty())
    }

    /// Hand the session the harder stop. The interrupt asks it to stop what it is doing;
    /// this ends the session outright, which is the only thing left when a watcher will
    /// not let go — every process it started goes with it. The conversation stays, and
    /// the next message starts a session again.
    fn stop_session(&mut self) {
        let Some(thread) = &self.thread else {
            self.toast("no thread open", true);
            return;
        };
        let thread_id = thread.id().to_string();
        self.dispatch(commands::session_stop(&thread_id));
        self.toast("stopping the session…", false);
    }

    /// The task list is a list to read; what there is to do from it is stop the work it
    /// shows. It stays open afterwards, because the rows clearing is the confirmation.
    ///
    /// `S` asks first. It ends the provider session and every process the agent started
    /// with it, it cannot be taken back, and it is `s` with a finger on shift — which is
    /// too near for something that big to happen on the first press.
    fn on_tasks_key(&mut self, key: KeyEvent) {
        if self.confirm_stop_session {
            // Anything that is not the answer is a no, including Esc: the question is
            // in front of the list until it has one, so no key meant for the list can
            // be read as agreeing to this.
            self.confirm_stop_session = false;
            if matches!(key.code, KeyCode::Char('S') | KeyCode::Char('y')) {
                self.stop_session();
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter | KeyCode::Char('T') => {
                self.mode = Mode::Normal
            }
            KeyCode::Char('s') => self.interrupt(),
            KeyCode::Char('S') => self.confirm_stop_session = true,
            _ => {}
        }
    }

    // ── Usage limits ───────────────────────────────────────────────────

    /// What each signed-in account has left of its subscription. The server probes the
    /// providers and puts the answer on the config, so this is the config's own copy,
    /// as old as the last time it was read.
    pub fn usage_accounts(&self) -> Vec<&crate::model::Provider> {
        self.config
            .providers
            .iter()
            .filter(|provider| provider.usage_limits.is_some())
            .collect()
    }

    /// The provider instance the next message would be spent from: the open thread's,
    /// or the draft's when a new thread is being written.
    pub fn current_provider_instance(&self) -> Option<&str> {
        self.current_model_selection()
            .map(|selection| selection.instance_id.as_str())
    }

    /// Show them, and ask for them again while they are being read: the config is
    /// fetched once per connection, so by the time anybody asks, the figures on it are
    /// as old as the session.
    fn open_usage(&mut self) {
        self.mode = Mode::Usage;
        self.refresh_usage();
    }

    /// Read the config again for the sake of the quota on it. The answer arrives as the
    /// config update it is, so everything else the config says is refreshed with it.
    fn refresh_usage(&mut self) {
        if self.usage_pending {
            return;
        }
        self.usage_pending = true;
        let handle = self.handle.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let read = handle
                .call("server.getConfig", json!({}))
                .await
                .and_then(|value| Ok(serde_json::from_value::<ServerConfig>(value)?))
                .map(Box::new)
                .map_err(|error| error.to_string());
            let _ = events.send(AppEvent::UsageRead(read));
        });
    }

    /// The config as it is now, or the news that it could not be read. Everything else
    /// the config says is taken with it: it is one answer and it is all of it.
    fn on_usage_read(&mut self, read: Result<Box<ServerConfig>, String>) {
        self.usage_pending = false;
        match read {
            Ok(config) => self.config = *config,
            Err(error) => self.toast(format!("usage: {error}"), true),
        }
    }

    fn on_usage_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Char('r') => self.refresh_usage(),
            _ => {}
        }
    }

    // ── Subagents ──────────────────────────────────────────────────────

    /// The conversation on screen: a subagent's transcript when one is open, otherwise
    /// the thread's own. The renderer reads the same two fields directly, because taking
    /// a reference to the whole of `self` here would borrow the fields it writes.
    pub fn open_conversation(&self) -> Option<&ThreadState> {
        self.transcript
            .as_ref()
            .map(|transcript| &transcript.state)
            .or(self.thread.as_ref())
    }

    /// The subagents the open thread has run, newest work last.
    pub fn subagents(&self) -> Vec<crate::subagent::Subagent> {
        match &self.thread {
            Some(thread) => thread.subagents(),
            None => Vec::new(),
        }
    }

    fn open_agents(&mut self) {
        if self.thread.is_none() {
            self.toast("no thread open", true);
            return;
        }
        let agents = self.subagents();
        if agents.is_empty() {
            self.toast("no subagents in this thread", false);
            return;
        }
        // Land on something worth watching: the transcript being read, else the first
        // one still working, else the last to finish, which is the one the conversation
        // has just been talking about.
        let reading = self
            .transcript
            .as_ref()
            .and_then(|transcript| agents.iter().position(|a| a.id == transcript.agent_id));
        self.agent_selected = reading
            .or_else(|| agents.iter().position(|agent| agent.status.is_active()))
            .unwrap_or(agents.len() - 1);
        self.mode = Mode::Agents;
    }

    fn on_agents_key(&mut self, key: KeyEvent) {
        let agents = self.subagents();
        let count = agents.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('A') => self.mode = Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down => {
                self.agent_selected = (self.agent_selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.agent_selected = self.agent_selected.saturating_sub(1);
            }
            KeyCode::Char('g') => self.agent_selected = 0,
            KeyCode::Char('G') => self.agent_selected = count.saturating_sub(1),
            KeyCode::Enter | KeyCode::Char('l') => self.read_transcript(),
            KeyCode::Char('y') => match agents.get(self.agent_selected) {
                Some(agent) => match agent.result.as_deref().or(agent.error.as_deref()) {
                    Some(report) => {
                        copy_to_clipboard(report);
                        self.toast("yanked the report", false);
                    }
                    None => self.toast("it has not reported back yet", false),
                },
                None => self.toast("no subagent selected", true),
            },
            _ => {}
        }
    }

    /// Where the provider is writing a subagent's transcript. It tells the server the
    /// path only when the task finishes, so for one still working it is taken from a
    /// task in the same thread that has: they are written side by side in one directory
    /// and named after the task, and the file is there from the moment the agent starts.
    pub fn transcript_path(&self, agent: &crate::subagent::Subagent) -> Option<String> {
        if let Some(path) = &agent.output_file {
            return Some(path.clone());
        }
        let thread = self.thread.as_ref()?;
        let known = thread
            .detail
            .activities
            .iter()
            .rev()
            .find_map(|activity| activity.str("outputFile"))?;
        crate::transcript::sibling_path(known, &agent.id)
    }

    /// Fetch the selected subagent's transcript from the machine that ran it. The path
    /// is the provider's own, outside any workspace, which is what `projects.readFile`
    /// takes an absolute path for.
    fn read_transcript(&mut self) {
        let agents = self.subagents();
        let Some(agent) = agents.get(self.agent_selected) else {
            self.toast("no subagent selected", true);
            return;
        };
        let Some(path) = self.transcript_path(agent) else {
            self.toast("no transcript for this subagent yet", false);
            return;
        };
        if self.transcript_loading.is_some() {
            return;
        }
        let Some(cwd) = self.thread_directory() else {
            self.toast("thread has no directory", true);
            return;
        };
        self.transcript_loading = Some(agent.id.clone());
        let agent_id = agent.id.clone();
        let handle = self.handle.clone();
        let events = self.events.clone();
        let payload = json!({ "cwd": cwd, "relativePath": path });
        tokio::spawn(async move {
            let result = handle
                .call("projects.readFile", payload)
                .await
                .map_err(|e| e.to_string())
                .map(|value| TranscriptFile {
                    contents: value
                        .get("contents")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    truncated: value
                        .get("truncated")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                });
            let _ = events.send(AppEvent::Transcript { agent_id, result });
        });
    }

    fn on_transcript(&mut self, agent_id: String, result: Result<TranscriptFile, String>) {
        if self.transcript_loading.as_deref() != Some(agent_id.as_str()) {
            return;
        }
        self.transcript_loading = None;
        let file = match result {
            Ok(file) => file,
            Err(error) => return self.toast(format!("reading the transcript: {error}"), true),
        };
        let agents = self.subagents();
        let Some(agent) = agents.iter().find(|agent| agent.id == agent_id) else {
            return;
        };
        let (state, summary) =
            match crate::transcript::parse(&file.contents, &agent.id, &agent.title) {
                Ok(parsed) => parsed,
                Err(error) => return self.toast(format!("{error}"), true),
            };
        let mut subtitle = format!(
            "{} message{} · {} tool call{}",
            summary.messages,
            if summary.messages == 1 { "" } else { "s" },
            summary.tools,
            if summary.tools == 1 { "" } else { "s" },
        );
        if let Some(role) = &agent.role {
            subtitle = format!("{role} · {subtitle}");
        }
        // Re-reading a running agent replaces what is open, so the way back is the one
        // taken on the way in, not wherever the reading had got to.
        let reread = self
            .transcript
            .as_ref()
            .is_some_and(|open| open.agent_id == agent_id);
        let restore = match &self.transcript {
            Some(open) if reread => open.restore,
            _ => (self.scroll, self.chat_cursor, self.focus),
        };
        let live = !agent.status.is_terminal();
        self.transcript = Some(Transcript {
            agent_id,
            title: agent.title.clone(),
            subtitle,
            state,
            truncated: file.truncated,
            live,
            restore,
        });
        // The transcript is read, not written to, so the cursor goes where the reading
        // is done and the conversation's own place is kept for the way back. A re-read
        // of a run still going lands at the end, which is the part that is new.
        self.mode = Mode::Normal;
        self.focus = Focus::Chat;
        if reread && live {
            self.scroll = Scroll::Follow;
        } else {
            self.scroll = Scroll::Offset(0);
            self.chat_cursor = 0;
        }
        self.chat_visual = None;
        self.search = None;
    }

    /// Read the open transcript again: an agent still working has written more since.
    fn reload_transcript(&mut self) {
        let Some(open) = self.transcript.as_ref().map(|t| t.agent_id.clone()) else {
            return;
        };
        let agents = self.subagents();
        let Some(index) = agents.iter().position(|agent| agent.id == open) else {
            self.toast("that subagent is no longer in this thread", true);
            return;
        };
        self.agent_selected = index;
        self.read_transcript();
    }

    /// Leave the transcript and put the conversation back where it was. Where to go next
    /// is the caller's: `q` returns to the list the transcript was opened from, while
    /// sending a message simply carries on.
    fn leave_transcript(&mut self) {
        let Some(transcript) = self.transcript.take() else {
            return;
        };
        let (scroll, cursor, focus) = transcript.restore;
        self.scroll = scroll;
        self.chat_cursor = cursor;
        self.focus = focus;
        self.chat_visual = None;
        self.search = None;
    }

    // ── Git popup ──────────────────────────────────────────────────────

    /// `gl` and `:git`: run the git command in the thread's directory. Inside tmux it opens
    /// as a popup over the pane and tria keeps running; elsewhere tria steps aside until
    /// the command exits.
    /// `gl` and `:git`: run the git command in the thread's own terminal, in the pane.
    /// Reuses one terminal per thread, so leaving and coming back finds it where it was.
    fn open_program(&mut self, key: char) {
        let Some(program) = self.programs.iter().find(|p| p.key == key).cloned() else {
            // Only `:git` reaches this: the key itself is not a key until it is bound.
            self.toast(format!("nothing is bound to g{key}"), true);
            return;
        };
        let Some(dir) = self.thread_directory() else {
            self.toast("no thread open", true);
            return;
        };
        let Some(thread_id) = self.current_thread_id.clone() else {
            self.toast("no thread open", true);
            return;
        };
        let command = program.command.clone();
        // `exec` replaces the shell, so quitting the program ends the session and the
        // popup closes with it. It also means a program bound here should be one that
        // holds the terminal until you leave it.
        self.pending_pane_command = Some(format!("exec {command}\r"));
        self.open_pane(thread_id, program.terminal_id(), command.clone(), dir);
        if let Some(pane) = self.pane.as_mut() {
            pane.starting = Some(command);
        }
    }

    /// Whether `g` and this key run a program.
    fn bound(&self, key: char) -> bool {
        self.programs.iter().any(|program| program.key == key)
    }

    /// The terminals the popups run in: one per thread and binding, scratch by nature,
    /// and the ones worth keeping a shell warm in.
    fn popup_terminals(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.programs.iter().map(|p| p.terminal_id()).collect();
        ids.push(SHELL_TERMINAL_ID.to_string());
        ids
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
        let key = self.region_at(line).map(|(key, _)| key).or_else(|| {
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
    /// The checkout the header describes: the open thread's, or the project a new
    /// thread would start in.
    fn watch_directory(&self) -> Option<String> {
        if let Some(draft) = &self.draft {
            return self
                .shell
                .projects
                .get(&draft.project_id)
                .map(|p| p.workspace_root.clone());
        }
        self.thread_directory()
    }

    fn thread_directory(&self) -> Option<String> {
        let shell = self.thread.as_ref().map(|t| &t.detail.shell)?;
        shell.worktree_path.clone().or_else(|| {
            self.shell
                .projects
                .get(&shell.project_id)
                .map(|p| p.workspace_root.clone())
        })
    }

    /// `gD` and `:reveal`: show the thread's directory in whatever this machine browses
    /// files with — the Finder, the file manager, the explorer. The path is the server's,
    /// so it means nothing when the server is on another machine: there it is either not
    /// a directory here or, worse, a different one.
    fn reveal_directory(&mut self) {
        let Some(dir) = self.thread_directory() else {
            self.toast("no thread open", true);
            return;
        };
        if !self.local_disk {
            self.toast("the thread's directory is on the server's machine", true);
            return;
        }
        self.open_url(&dir);
    }

    /// The thread's directory, for the keys that hand it to tmux: this has to be running
    /// inside tmux, and the directory has to be one this machine has. The path is the
    /// server's, so on another machine it is either not a directory here or, worse, a
    /// different one — and tmux would open something at the wrong place rather than fail.
    fn tmux_directory(&mut self) -> Option<String> {
        if std::env::var_os("TMUX").is_none() {
            self.toast("not running inside tmux", true);
            return None;
        }
        let Some(dir) = self.thread_directory() else {
            self.toast("no thread open", true);
            return None;
        };
        if !self.local_disk {
            self.toast("the thread's directory is on the server's machine", true);
            return None;
        }
        Some(dir)
    }

    /// `gP` and `:split`: open a tmux pane beside this one, in the thread's directory.
    ///
    /// It splits tria's own pane, so the shell arrives in the session and the window
    /// already on screen — which is the whole difference between this and `gt`, where
    /// the thread's session is somewhere else to be switched to and back from. tmux
    /// moves the focus to a pane it has just made, and says nothing afterwards: the new
    /// pane is right there with the cursor in it, and a toast would be announcing what
    /// is already on screen.
    fn split_tmux_pane(&mut self) {
        let Some(dir) = self.tmux_directory() else {
            return;
        };
        // Tria's own pane, rather than whichever one tmux last called the active one.
        let pane = std::env::var("TMUX_PANE").ok();
        self.run_tmux(&split_args(&dir, pane.as_deref()), "split-window");
    }

    /// Run a tmux command that is expected to say nothing, and pass on what it said if
    /// it failed. tmux writes the reason to its standard error and nowhere else, so
    /// without this a key that did nothing would look like a key that does nothing.
    fn run_tmux(&mut self, args: &[String], what: &str) {
        match std::process::Command::new("tmux").args(args).output() {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                self.toast(format!("tmux {what} failed: {err}"), true);
            }
            Err(err) => self.toast(format!("could not run tmux: {err}"), true),
        }
    }

    /// `gN` and `:window`: open a tmux window in this session, in the thread's directory
    /// — the tab at the bottom of the screen, which is what `prefix c` makes, opened
    /// where the thread works rather than wherever the session was started.
    ///
    /// Which session that is comes from the environment tria was started in, so it is
    /// the one on screen. tmux moves to a window it has just made, and says nothing
    /// afterwards for the same reason the split does not: it is already there.
    fn new_tmux_window(&mut self) {
        let Some(dir) = self.tmux_directory() else {
            return;
        };
        self.run_tmux(&window_args(&dir), "new-window");
    }

    /// `gt` and `:tmux`: switch the tmux client to the session named after the thread's
    /// directory, creating it there first when it does not exist.
    fn switch_tmux_session(&mut self) {
        let Some(dir) = self.tmux_directory() else {
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

    /// `gx`: the link under the chat cursor when there is one, then the picture the row
    /// under it has, and failing both the thread's pull request. Vim opens the link under
    /// the cursor with this key, and a chat line with a link on it is the case where that
    /// is what you meant; a picture is the other thing on a line that is somewhere else
    /// really, and the terminal only ever shows a thumbnail of it.
    fn open_under_cursor(&mut self, pick: bool) {
        if let Some(url) = self.link_at_cursor() {
            self.open_url(&url);
            return;
        }
        if let Some(picture) = self.picture_at_cursor() {
            self.open_picture(picture);
            return;
        }
        self.open_pull_request(pick);
    }

    /// The picture under the chat cursor: the one the row it is in has, open or shut, or
    /// the one a message drew there. The cursor is inside the row wherever it is on the
    /// picture itself, since the lines it was drawn over belong to the row that opened it.
    fn picture_at_cursor(&self) -> Option<crate::timeline::Picture> {
        let key = match self.region_at(self.chat_cursor) {
            Some((key, _)) => key,
            None => self.picture_range_at(self.chat_cursor)?,
        };
        self.chat_pictures
            .iter()
            .find(|(row, _)| *row == key)
            .map(|(_, picture)| picture.clone())
    }

    /// What a message's picture is known by, where one was drawn on this line. The
    /// smallest range wins, so two pictures on one line are told apart by the lines they
    /// were each drawn on.
    fn picture_range_at(&self, line: usize) -> Option<String> {
        self.picture_ranges
            .iter()
            .filter(|region| region.first <= line && line < region.end)
            .min_by_key(|region| region.end - region.first)
            .map(|region| region.key.clone())
    }

    /// Hand a picture to whatever this machine opens pictures with. One the provider
    /// wrote into a transcript is not a file anywhere, so it is written out first: a
    /// viewer opens paths, and the temporary file is named after what is in it, so
    /// opening the same picture twice writes it once.
    fn open_picture(&mut self, picture: crate::timeline::Picture) {
        use crate::timeline::Picture;
        match picture {
            Picture::File(path) => self.open_url(&path),
            Picture::Data(data) => match write_picture(&data) {
                Some(path) => self.open_url(&path),
                None => self.toast("that picture is not one tria can write out", true),
            },
        }
    }

    /// Open the thread's pull request in the browser. With several linked pull requests,
    /// `:pr` offers a picker.
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

    /// Fetch the icon of every project that has not been asked about yet.
    ///
    /// The looking is the server's: the path the project names, then the one its `t3.json`
    /// names, then the usual places a favicon lives, then whatever the project's
    /// `index.html` links to. It answers with a URL good for a few minutes, signed, and
    /// the bytes come over HTTP — so a server on another machine works like this one,
    /// which reading the path ourselves would not.
    fn ask_favicons(&mut self) {
        let wanted: Vec<(Id, String)> = self
            .shell
            .projects
            .values()
            .filter(|p| !self.favicons.contains_key(&p.id))
            .map(|p| (p.id.clone(), p.workspace_root.clone()))
            .collect();
        for (project, cwd) in wanted {
            // Marked before the answer so a picker opened twice asks once.
            self.favicons.insert(project.clone(), None);
            let handle = self.handle.clone();
            let events = self.events.clone();
            let origin = self.origin.clone();
            tokio::spawn(async move {
                let bytes = favicon_bytes(&handle, &origin, &cwd).await;
                let _ = events.send(AppEvent::Favicon { project, bytes });
            });
        }
    }

    fn on_favicon(&mut self, project: Id, bytes: Option<Vec<u8>>) {
        if bytes.is_some() {
            self.favicons.insert(project, bytes);
        }
    }

    /// The icon a project is known by, for whoever is drawing its name.
    pub fn favicon(&self, project: &str) -> Option<&[u8]> {
        self.favicons.get(project)?.as_deref()
    }

    /// The emoji a project was given, which stands in front of it wherever it is named.
    /// Chosen by hand, so it wins over the icon the server went looking for.
    pub fn project_emoji(&self, project: &str) -> Option<&str> {
        self.shell.projects.get(project)?.emoji()
    }

    /// The drawn icon a project was given, as its name and colour. Chosen by hand like
    /// the emoji, so it too wins over the icon the server went looking for.
    pub fn project_lucide(&self, project: &str) -> Option<(&str, Option<&str>)> {
        self.shell.projects.get(project)?.lucide()
    }

    /// The icon a project falls back to, which is a guess at what it is from its name.
    /// Nobody sends this: the desktop app works it out for itself, and so does this, out
    /// of the same name and by the same rules, so that a project nobody has given an
    /// icon still looks like itself in both.
    pub fn project_guessed_icon(&self, project: &str) -> Option<(&'static str, &'static str)> {
        let project = self.shell.projects.get(project)?;
        Some(crate::lucide::guess(
            &project.title,
            &project.workspace_root,
        ))
    }

    /// Hand a project an icon, for a test that draws one.
    #[cfg(test)]
    pub fn give_favicon(&mut self, project: &str, bytes: Vec<u8>) {
        self.favicons.insert(project.to_string(), Some(bytes));
    }

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
            // The project list is the one that can be empty on a server that is working
            // perfectly well, and it is the first thing somebody meets. So it says what
            // to do about it rather than only that there is nothing here.
            self.toast(
                match kind {
                    PickerKind::Project => {
                        "no projects yet · run `tria open` in one to add it".to_string()
                    }
                    _ => "nothing to pick from".to_string(),
                },
                true,
            );
            return;
        }
        self.picker = Some(Picker {
            kind,
            query: Composer::new(),
            selected: 0,
            items,
            renaming: None,
        });
        self.mode = Mode::Picker;
    }

    /// Give a project a new name. The name is the server's, so this is the name the
    /// project has everywhere — the sidebar here, the desktop app, the next client.
    fn rename_project(&mut self, project: &str, title: &str) {
        if title.is_empty() {
            self.toast("a project needs a name", true);
            return;
        }
        if !self.shell.projects.contains_key(project) {
            self.toast("that project is no longer here", true);
            return;
        }
        self.dispatch(commands::project_rename(project, title));
        self.toast(format!("renamed the project to {title}"), false);
    }

    /// The project a rename with no project named is about: the open thread's, or the
    /// one a draft is being written for.
    fn current_project(&self) -> Option<Id> {
        if let Some(draft) = &self.draft {
            return Some(draft.project_id.clone());
        }
        let thread = self.thread.as_ref()?;
        Some(thread.detail.shell.project_id.clone())
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
        if let Some(project) = picker.renaming {
            self.mode = Mode::Normal;
            self.rename_project(&project, picker.query.text().trim());
            return;
        }
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
        // Whatever was picked last is what the next new thread starts with.
        self.new_thread_model = Some(selection.clone());
        crate::config::Config::remember_model(&selection);
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
            "help" | "h" => self.open_help(),
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
                if !RUNTIME_MODES.contains(&arg) {
                    self.toast(format!("usage: :perm {}", RUNTIME_MODES.join("|")), true);
                } else if let Some(id) = thread_id.as_deref() {
                    self.dispatch(commands::runtime_mode_set(id, arg));
                } else if let Some(d) = self.draft.as_mut() {
                    d.runtime_mode = arg.into();
                }
            }
            "project" => match arg.split_once(char::is_whitespace) {
                Some(("rename", title)) if !title.trim().is_empty() => {
                    match self.current_project() {
                        Some(project) => self.rename_project(&project, title.trim()),
                        None => self.toast("no project open", true),
                    }
                }
                _ => self.toast("usage: :project rename <name>", true),
            },
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
            // Always available, because the digits are not: a draft in the composer
            // takes them, and an approval still has to be answerable then.
            "approve" | "allow" => match if arg.is_empty() {
                Some(1)
            } else {
                arg.parse::<usize>().ok().filter(|n| *n >= 1)
            } {
                Some(n) => self.respond_approval(n - 1),
                None => self.toast("usage: :approve [n], the number beside the answer", true),
            },
            "stop" | "interrupt" => self.interrupt(),
            // The harder one, for when the interrupt does not take: the session itself.
            "stop!" => self.stop_session(),
            // The way back from a connection that has settled into refusing, and a way
            // to start again with one that is up but has stopped being any use.
            "reconnect" | "connect" => {
                self.handle.reconnect();
                if let Some(lost) = self.lost_stream.as_mut() {
                    lost.retry_at = Instant::now();
                }
                self.toast("connecting again…", false);
            }
            "sidebar" => self.sidebar_visible = !self.sidebar_visible,
            "pr" | "pull" => self.open_pull_request(true),
            "tmux" => self.switch_tmux_session(),
            "split" => self.split_tmux_pane(),
            "window" | "tab" => self.new_tmux_window(),
            "reveal" | "dir" => self.reveal_directory(),
            // `:git` is what the `l` binding has always been called, whatever is on it.
            "git" | "lazygit" => self.open_program('l'),
            "usage" | "limits" => self.open_usage(),
            "tasks" | "jobs" => self.open_tasks(),
            "agents" | "subagents" => self.open_agents(),
            "terminals" | "shells" => self.open_terminals(),
            "worktrees" => self.open_worktrees(),
            "shell" => self.open_shell(),
            "worktree" | "wt" => self.toggle_draft_worktree(),
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
            // A program bound to a key answers to its own name as well.
            _ => match self
                .programs
                .iter()
                .find(|p| p.name() == name)
                .map(|p| p.key)
            {
                Some(key) => self.open_program(key),
                None => self.toast(format!("unknown command :{name}"), true),
            },
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
        // Whatever was half typed or half selected at the composer was meant for the
        // composer, and drawing a selection nothing is about to act on is a lie.
        self.composer.vim_cancel();
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

    /// The chat cursor as a line and the character it is on. The column is held where it
    /// was put, so passing a short line does not pull the cursor left for good.
    pub fn chat_spot(&self) -> (usize, usize) {
        let line = self.chat_cursor;
        (
            line,
            self.chat_column.min(ui::chat_len(line).saturating_sub(1)),
        )
    }

    /// `w` and `b` over the characters of the chat, carrying on into the line above or
    /// below when the one under the cursor runs out.
    fn chat_word(&mut self, forward: bool) {
        let n = self.take_chat_count();
        let (_, total) = self.chat_viewport;
        for _ in 0..n {
            let (mut line, mut column) = self.chat_spot();
            let mut chars: Vec<char> = ui::chat_row(line).chars().collect();
            if forward {
                let from = word_class(chars.get(column).copied());
                while column < chars.len() && word_class(Some(chars[column])) == from {
                    column += 1;
                }
                loop {
                    while chars.get(column).is_some_and(|c| c.is_whitespace()) {
                        column += 1;
                    }
                    if column < chars.len() || line + 1 >= total {
                        break;
                    }
                    line += 1;
                    column = 0;
                    chars = ui::chat_row(line).chars().collect();
                    // A blank line is a stop of its own, as it is in Vim.
                    if chars.is_empty() {
                        break;
                    }
                }
            } else {
                loop {
                    while column > 0 && chars[column - 1].is_whitespace() {
                        column -= 1;
                    }
                    if column > 0 || line == 0 {
                        break;
                    }
                    line -= 1;
                    chars = ui::chat_row(line).chars().collect();
                    column = chars.len();
                    if chars.is_empty() {
                        break;
                    }
                }
                let to = word_class(chars.get(column.wrapping_sub(1)).copied());
                while column > 0 && word_class(Some(chars[column - 1])) == to {
                    column -= 1;
                }
            }
            self.set_chat_cursor(line);
            self.chat_column = column.min(chars.len().saturating_sub(1));
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
        // A transcript is read with the ordinary chat keys. `q` and `Esc` put it away
        // and go back to the list it was opened from; anything that moves the focus
        // elsewhere closes it too, so the keyboard and the screen never disagree about
        // which conversation is in front.
        if self.transcript.is_some() && prefix.is_none() {
            let idle = self.chat_visual.is_none() && self.chat_count.is_none();
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc if idle => {
                    self.leave_transcript();
                    self.mode = Mode::Agents;
                    return;
                }
                KeyCode::Tab | KeyCode::BackTab => self.leave_transcript(),
                // The file grows while the agent works, and nothing pushes it to us.
                KeyCode::Char('r') if idle => {
                    self.reload_transcript();
                    return;
                }
                _ => {}
            }
        }
        let (height, total) = self.chat_viewport;
        let cursor = self.chat_cursor;
        // A key that moves the cursor within a line is only itself: `gl` opens git and
        // Ctrl-b pages up.
        let plain = prefix.is_none() && !ctrl;
        if prefix == Some('g') && self.global_prefix_key(key) {
            return;
        }
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
            KeyCode::Char('h') | KeyCode::Left if plain => {
                let n = self.take_chat_count();
                self.chat_column = self.chat_spot().1.saturating_sub(n);
            }
            KeyCode::Char('l') | KeyCode::Right if plain => {
                let n = self.take_chat_count();
                let last = ui::chat_len(cursor).saturating_sub(1);
                self.chat_column = (self.chat_spot().1 + n).min(last);
            }
            KeyCode::Char('0') if plain => self.chat_column = 0,
            // Held past the end, so the cursor stays there down a ragged block.
            KeyCode::Char('$') if plain => self.chat_column = usize::MAX,
            KeyCode::Char('w') if plain => self.chat_word(true),
            KeyCode::Char('b') if plain => self.chat_word(false),
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
                // A transcript is whole as it was read; there is nothing older to fetch,
                // and the thread underneath is not what the top of the view belongs to.
                if self.transcript.is_none() && self.thread.as_ref().is_some_and(|t| t.has_more) {
                    self.load_older();
                }
            }
            KeyCode::Char('e') if prefix == Some('g') => self.view_at_cursor(),
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
            KeyCode::Char('a') if prefix == Some('z') => self.fold_at(cursor, true),
            KeyCode::Enter | KeyCode::Char(' ') => self.fold_at(cursor, false),
            KeyCode::Char('R') if prefix == Some('z') => self.open_levels = MOST_OPEN_LEVELS,
            KeyCode::Char('r') if prefix == Some('z') => {
                self.open_levels = (self.open_levels + 1).min(MOST_OPEN_LEVELS);
            }
            KeyCode::Char('m') if prefix == Some('z') => {
                self.open_levels = self.open_levels.saturating_sub(1);
            }
            KeyCode::Char('M') if prefix == Some('z') => {
                self.open_levels = 0;
                self.expanded.clear();
            }
            // `v` takes the text a character at a time, `V` whole lines; either one
            // pressed again lets the selection go, and each takes over from the other.
            KeyCode::Char(key @ ('v' | 'V')) => {
                let whole_lines = key == 'V';
                self.chat_visual = match self.chat_visual {
                    Some(anchor) if anchor.whole_lines == whole_lines => None,
                    Some(anchor) => Some(ChatAnchor {
                        whole_lines,
                        ..anchor
                    }),
                    None => Some(ChatAnchor {
                        line: cursor,
                        column: self.chat_spot().1,
                        whole_lines,
                    }),
                };
            }
            KeyCode::Char('y') => {
                let spot = self.chat_spot();
                let (start, end) = match self.chat_visual.take() {
                    // A character-wise selection reaches through the character the
                    // cursor is on, which is where the block cursor is drawn.
                    Some(anchor) if !anchor.whole_lines => {
                        let head = (anchor.line, anchor.column.min(ui::chat_len(anchor.line)));
                        let (first, last) = if head <= spot {
                            (head, spot)
                        } else {
                            (spot, head)
                        };
                        (first, (last.0, last.1 + 1))
                    }
                    Some(anchor) => (
                        (anchor.line.min(cursor), 0),
                        (anchor.line.max(cursor), usize::MAX),
                    ),
                    None => {
                        // `yy`: the current line, or `Ny` for several.
                        let n = self.take_chat_count();
                        (
                            (cursor, 0),
                            ((cursor + n - 1).min(total.saturating_sub(1)), usize::MAX),
                        )
                    }
                };
                match ui::chat_span(start, end) {
                    Some(text) if !text.trim().is_empty() => {
                        let lines = text.lines().count();
                        copy_to_clipboard(&text);
                        self.toast(
                            match lines {
                                1 if start.0 == end.0 && end.1 != usize::MAX => {
                                    "yanked selection".to_string()
                                }
                                1 => "yanked line".to_string(),
                                lines => format!("yanked {lines} lines"),
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
            KeyCode::Char(':') => self.open_command_line(),
            _ => {}
        }
    }

    // ── Chat search ────────────────────────────────────────────────────

    fn start_search(&mut self, backward: bool) {
        self.chat_count = None;
        self.search_input = Some(SearchInput {
            query: Composer::new(),
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
                let query = input.query.text();
                if query.is_empty() {
                    // Bare Enter repeats the last search, as in Vim.
                    self.search_next(false);
                    return;
                }
                let search = Search {
                    query,
                    backward: input.backward,
                };
                self.search = Some(search.clone());
                self.jump_to_match(&search, input.origin, false);
            }
            // Everything else edits the line, with the same keys as every other field.
            _ => {
                edit_key(&mut input.query, key);
                self.incremental_search();
            }
        }
    }

    /// While typing, the cursor previews the first match from where the search started.
    fn incremental_search(&mut self) {
        let Some(input) = self.search_input.as_ref() else {
            return;
        };
        let (query, backward, origin) = (input.query.text(), input.backward, input.origin);
        if query.is_empty() {
            self.set_chat_cursor(origin);
            return;
        }
        let search = Search { query, backward };
        if let Some((line, _)) = self.find_match(&search, origin) {
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

    /// The tightest toggle region covering a content line, with whether folding it shows
    /// anything: a tool row inside an expanded group wins over the group itself, even
    /// when the row has nothing to unfold — folding its group instead is not what the
    /// click asked for.
    fn region_at(&self, line: usize) -> Option<(String, bool)> {
        self.work_ranges
            .iter()
            .filter(|region| region.first <= line && line < region.end)
            .min_by_key(|region| region.end - region.first)
            .map(|region| (region.key.clone(), region.foldable))
    }

    /// Fold what is under a content line, saying so when there is nothing to fold.
    fn fold_at(&mut self, line: usize, quiet: bool) {
        match self.region_at(line) {
            Some((key, true)) => self.toggle_expanded(key),
            Some((_, false)) => self.toast("the server kept nothing more for this row", false),
            None if !quiet => self.toast("nothing to fold here", false),
            None => {}
        }
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
            .region_at(middle)
            .filter(|(_, foldable)| *foldable)
            .map(|(key, _)| key)
            .or_else(|| {
                self.work_ranges
                    .iter()
                    .rev()
                    .find(|region| region.first < offset + height)
                    .map(|region| region.key.clone())
            })
            .or_else(|| self.work_ranges.last().map(|region| region.key.clone()));
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
        // A pending `Ctrl-v` belongs to the message being written and to nothing else,
        // so it does not follow the focus out of insert mode.
        if self.mode != Mode::Insert {
            self.literal_next = false;
        }
        // The attached terminal takes every key, including Ctrl-c, before the global
        // chords: interrupting the shell is the whole point of that key there.
        if self.mode == Mode::TerminalPane {
            self.on_pane_key(key);
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl-c, the one global chord. In vim it is Esc under another name: it leaves
        // what is being typed. So it does here, whatever is being typed — a message, a
        // command, a search, a list — rather than reaching past the thing in front of
        // you to the turn behind it. Normal mode is where there is nothing to leave,
        // and that is where it stops the turn.
        if ctrl && key.code == KeyCode::Char('c') {
            if self.mode != Mode::Normal {
                return self.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            }
            let running = self.thread.as_ref().is_some_and(|t| t.is_running());
            if running || self.background_alive() {
                self.interrupt();
            } else {
                self.toast("nothing running · :q to quit", false);
            }
            return;
        }
        match self.mode {
            Mode::Normal => self.on_normal_key(key),
            Mode::Insert => self.on_insert_key(key),
            Mode::Command => self.on_command_key(key),
            Mode::Search => self.on_search_key(key),
            Mode::Tasks => self.on_tasks_key(key),
            Mode::Usage => self.on_usage_key(key),
            Mode::Agents => self.on_agents_key(key),
            Mode::Terminals => self.on_terminals_key(key),
            Mode::Worktrees => self.on_worktrees_key(key),
            // Handled above, before the global chords.
            Mode::TerminalPane => {}
            Mode::Picker => self.on_picker_key(key),
            Mode::Question => self.on_question_key(key),
            Mode::QuestionCustom => self.on_question_custom_key(key),
            Mode::Help => self.on_help_key(key),
        }
    }

    fn open_help(&mut self) {
        self.help_offset = 0;
        self.mode = Mode::Help;
    }

    /// The help is longer than it is tall, so it scrolls with the keys the chat uses.
    fn on_help_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let (height, total) = self.help_viewport;
        let max = total.saturating_sub(height);
        let by =
            |offset: usize, delta: isize| (offset as isize + delta).clamp(0, max as isize) as usize;
        self.help_offset = match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter => {
                self.mode = Mode::Normal;
                return;
            }
            KeyCode::Char('j') | KeyCode::Down => by(self.help_offset, 1),
            KeyCode::Char('k') | KeyCode::Up => by(self.help_offset, -1),
            KeyCode::Char('d') if ctrl => by(self.help_offset, height as isize / 2),
            KeyCode::Char('u') if ctrl => by(self.help_offset, -(height as isize) / 2),
            KeyCode::Char('f') | KeyCode::PageDown => by(self.help_offset, height as isize),
            KeyCode::Char('b') | KeyCode::PageUp => by(self.help_offset, -(height as isize)),
            KeyCode::Char('g') | KeyCode::Home => 0,
            KeyCode::Char('G') | KeyCode::End => max,
            _ => return,
        };
    }

    /// The `g` motions that mean the same thing wherever they are pressed: they look
    /// past whatever has the focus at the thread, the session, or the machine behind it.
    /// Returns whether the key was one of them, so a caller can go on to the motions its
    /// own focus is what gives meaning — `gg`, `ge`, `gJ`.
    ///
    /// Every letter here is spoken for in [`crate::config::TAKEN_KEYS`], so a program
    /// bound to `g<key>` can never be one of them and is safe to try last.
    fn global_prefix_key(&mut self, key: KeyEvent) -> bool {
        // A chord after `g` is still the chord: `g` then Ctrl-y scrolls a line, it does
        // not yank.
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        let KeyCode::Char(c) = key.code else {
            return false;
        };
        match c {
            'x' => self.open_under_cursor(false),
            't' => self.switch_tmux_session(),
            'P' => self.split_tmux_pane(),
            'N' => self.new_tmux_window(),
            'D' => self.reveal_directory(),
            'y' => self.yank_last_assistant(),
            'E' => self.view_conversation(),
            '!' => self.open_shell(),
            'w' => self.toggle_draft_worktree(),
            'T' => self.open_tasks(),
            'A' => self.open_agents(),
            'S' => self.open_terminals(),
            'W' => self.open_worktrees(),
            's' => {
                let id = self.current_thread_id.clone();
                self.toggle_settled(id);
            }
            'a' => {
                if !self.begin_answering() {
                    self.toast("no question pending", false);
                }
            }
            bound if self.bound(bound) => self.open_program(bound),
            _ => return false,
        }
        true
    }

    fn take_prefix(&mut self) -> Option<char> {
        let (prefix, at) = self.pending_prefix.take()?;
        match self.prefix_timeout {
            Some(ttl) => (at.elapsed() < ttl).then_some(prefix),
            None => Some(prefix),
        }
    }

    /// The prefix waiting for the key that completes it, for the status bar. `g` on its
    /// own is a key that has not finished being pressed, and a key that is waiting is
    /// worth seeing — the more so where it waits indefinitely, since then the only sign
    /// of one pressed by mistake is the one on the screen.
    pub fn waiting_prefix(&self) -> Option<char> {
        let (prefix, at) = self.pending_prefix?;
        match self.prefix_timeout {
            Some(ttl) => (at.elapsed() < ttl).then_some(prefix),
            None => Some(prefix),
        }
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
                KeyCode::Char(key) if prefix == Some('g') && self.bound(key) => {
                    self.open_program(key)
                }
                KeyCode::Char('!') if prefix == Some('g') => self.open_shell(),
                KeyCode::Char('w') if prefix == Some('g') => self.toggle_draft_worktree(),
                KeyCode::Char('D') if prefix == Some('g') => self.reveal_directory(),
                KeyCode::Char('T') if prefix == Some('g') => self.open_tasks(),
                KeyCode::Char('A') if prefix == Some('g') => self.open_agents(),
                KeyCode::Char('S') if prefix == Some('g') => self.open_terminals(),
                KeyCode::Char('W') if prefix == Some('g') => self.open_worktrees(),
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
                KeyCode::Char(':') => self.open_command_line(),
                KeyCode::Char('?') => self.open_help(),
                _ => {}
            }
            return;
        }
        if self.focus == Focus::Chat {
            self.on_chat_key(key, prefix);
            return;
        }
        // A key that is part of something the composer has already begun — a count, an
        // operator waiting for its motion, a selection being made — belongs to the
        // composer. The app only takes a letter that starts nothing.
        let editing = self.composer.vim_busy();
        let question_pending = self
            .thread
            .as_ref()
            .is_some_and(|t| t.pending_user_input().is_some());
        let approval_pending = self
            .thread
            .as_ref()
            .is_some_and(|t| !t.pending_approvals().is_empty());
        // Chat and app keys first; whatever is left edits the composer.
        if prefix == Some('g') && self.global_prefix_key(key) {
            return;
        }
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
            KeyCode::Char('e') if prefix == Some('g') => return self.edit_composer(),
            // `J` is the next thread, so the join Vim puts there is on `gJ`.
            KeyCode::Char('J') if prefix == Some('g') => return self.composer.vim_join(2),
            KeyCode::Char('g') if !editing => {
                self.pending_prefix = Some(('g', Instant::now()));
                return;
            }
            KeyCode::Char('z') if !editing => {
                self.pending_prefix = Some(('z', Instant::now()));
                return;
            }
            KeyCode::Char('a') if prefix == Some('z') => return self.toggle_work_group(),
            KeyCode::Char('R') if prefix == Some('z') => {
                return self.open_levels = MOST_OPEN_LEVELS;
            }
            KeyCode::Char('r') if prefix == Some('z') => {
                return self.open_levels = (self.open_levels + 1).min(MOST_OPEN_LEVELS);
            }
            KeyCode::Char('m') if prefix == Some('z') => {
                return self.open_levels = self.open_levels.saturating_sub(1);
            }
            KeyCode::Char('M') if prefix == Some('z') => {
                self.open_levels = 0;
                self.expanded.clear();
                return;
            }
            KeyCode::Char('J') if !editing => return self.open_relative(1),
            KeyCode::Char('K') if !editing => return self.open_relative(-1),
            KeyCode::Tab => {
                self.focus_chat();
                return;
            }
            // What was half typed at the composer, or half selected in it, was meant for
            // the composer; it does not follow the focus out.
            KeyCode::BackTab => {
                self.composer.vim_cancel();
                self.sidebar_visible = true;
                self.focus = Focus::Sidebar;
                return;
            }
            KeyCode::Char('/') if !editing => return self.open_picker(PickerKind::Thread),
            KeyCode::Char('n') if !editing => return self.open_picker(PickerKind::Project),
            KeyCode::Char('m') if !editing => return self.open_picker(PickerKind::Model),
            KeyCode::Enter if question_pending => {
                self.begin_answering();
                return;
            }
            KeyCode::Enter => {
                if self.thread.is_some() || self.draft.is_some() {
                    self.composer.vim_cancel();
                    self.composer.checkpoint();
                    self.mode = Mode::Insert;
                } else {
                    self.toast("open a thread first (/ or Tab), or n for a new one", false);
                }
                return;
            }
            KeyCode::Char(':') => {
                self.open_command_line();
                return;
            }
            KeyCode::Char('?') if !editing => return self.open_help(),
            KeyCode::Char('s') if !editing => {
                return self.sidebar_visible = !self.sidebar_visible;
            }
            KeyCode::Char('S') if !editing => {
                return self.show_settled = !self.show_settled;
            }
            KeyCode::Char(c @ '1'..='9')
                if approval_pending && !editing && self.digits_answer_approval() =>
            {
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
        // All of them, not only the two: a list drawn over the chat is as much in the
        // way as the picker is, and one of them is a question about destroying work.
        if matches!(
            self.mode,
            Mode::Picker
                | Mode::Help
                | Mode::Tasks
                | Mode::Usage
                | Mode::Agents
                | Mode::Terminals
                | Mode::Worktrees
        ) {
            return;
        }
        if self.mode == Mode::TerminalPane {
            self.on_pane_mouse(mouse);
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
                    let rows = self.sidebar_rows();
                    let height = self.sidebar_inner.map_or(0, |r| r.height as usize);
                    let max = self.sidebar_max_offset(&rows, height);
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
                let rows = self.sidebar_rows();
                let index = match self.sidebar_row_at(&rows, (mouse.row - inner.y) as usize) {
                    Some(index) => index,
                    None => return,
                };
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
            // Clicking in the composer is asking to write there, which means putting
            // the cursor where the click landed and being in insert mode, since that is
            // what a click in a text box does everywhere else.
            MouseEventKind::Down(MouseButton::Left)
                if self.composer_area.contains(at)
                    && matches!(self.mode, Mode::Normal | Mode::Insert) =>
            {
                self.selection = None;
                self.chat_visual = None;
                self.focus = Focus::Composer;
                if self.mode != Mode::Insert {
                    self.composer.checkpoint();
                    self.mode = Mode::Insert;
                }
                self.composer.click(self.composer_area, at.x, at.y);
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
                                self.composer.vim_cancel();
                                self.focus = Focus::Chat;
                                self.chat_visual = None;
                                self.set_chat_cursor(line);
                                self.chat_column = ui::chat_index(line, at.x - self.chat_area.x);
                            }
                            // A link takes the click; folding would be a surprise.
                            if let Some(url) = self.link_at(at) {
                                self.open_url(&url);
                            } else {
                                self.fold_at(line, true);
                            }
                        }
                    } else {
                        let (from, to) = sel.ordered();
                        self.clipboard_pending = self.chat_under(from, to);
                    }
                }
            }
            _ => {}
        }
    }

    /// What a drag over the chat covers, the marks and indents it was drawn with left out.
    fn chat_under(&self, from: Position, to: Position) -> Option<String> {
        let offset = self.chat_offset();
        let spot = |at: Position, past: usize| {
            let line = offset + at.y.saturating_sub(self.chat_area.y) as usize;
            (line, ui::chat_index(line, at.x - self.chat_area.x) + past)
        };
        ui::chat_span(spot(from, 0), spot(to, 1))
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
        if std::mem::take(&mut self.literal_next) {
            return self.insert_literal(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            // As in vim, where `Ctrl-q` is `Ctrl-v` under another name: the next key
            // goes in as the character it stands for rather than as the key it is.
            // `Enter` is the one worth having — it sends a message, and this is how
            // you put one inside a message instead.
            KeyCode::Char('v') | KeyCode::Char('q') if ctrl => self.literal_next = true,
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.composer.leave_insert();
            }
            KeyCode::Enter if alt || shift || ctrl => self.composer.newline(),
            KeyCode::Char('j') if ctrl => self.composer.newline(),
            KeyCode::Enter => self.send_message(),
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
            KeyCode::PageUp => self.scroll_by(-(self.chat_viewport.0 as isize)),
            KeyCode::PageDown => self.scroll_by(self.chat_viewport.0 as isize),
            KeyCode::Char('p') if ctrl => self.composer.history_prev(),
            KeyCode::Char('n') if ctrl => self.composer.history_next(),
            KeyCode::Tab => self.composer.insert_str("    "),
            _ => {
                edit_key(&mut self.composer, key);
            }
        }
    }

    /// The key after `Ctrl-v`, as a character. Only the ones that stand for one: a
    /// message is text, and a control character written into it is no use to anybody
    /// reading it at the other end. Anything else drops the literal and does nothing,
    /// `Esc` included, which is how to change your mind about it.
    fn insert_literal(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(c) => self.composer.insert_char(c),
            KeyCode::Enter => self.composer.newline(),
            KeyCode::Tab => self.composer.insert_char('\t'),
            _ => {}
        }
    }

    /// Start a command, on an empty line with no completion carried over from the last.
    fn open_command_line(&mut self) {
        self.mode = Mode::Command;
        self.command_line.clear();
        self.completing = None;
    }

    /// The `:` line takes the same editing keys as every other field — `Ctrl-w`,
    /// `Ctrl-a`, `Ctrl-u`, the arrows, the word motions — with history on the arrows
    /// and `Tab` completion of the command's name and of the arguments that come from
    /// a fixed list.
    fn on_command_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Only `Tab` carries anything over from the key before it; anything else is a
        // line that has changed under the completion, so the candidates are dropped.
        if !matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.completing = None;
        }
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.command_line.clear();
            }
            KeyCode::Enter => {
                let line = self.command_line.text();
                self.mode = Mode::Normal;
                if !line.trim().is_empty() {
                    self.command_line.push_history(line.clone());
                }
                self.command_line.clear();
                self.run_command(&line);
            }
            // An empty line with nothing left to rub out is a command given up on.
            KeyCode::Backspace if self.command_line.text().is_empty() => self.mode = Mode::Normal,
            KeyCode::Tab => self.complete_command(false),
            KeyCode::BackTab => self.complete_command(true),
            KeyCode::Up => self.command_line.history_prev(),
            KeyCode::Down => self.command_line.history_next(),
            KeyCode::Char('p') if ctrl => self.command_line.history_prev(),
            KeyCode::Char('n') if ctrl => self.command_line.history_next(),
            _ => edit_key(&mut self.command_line, key),
        }
    }

    /// `Tab` on the command line. The first one puts the first candidate in, and each
    /// one after moves to the next, coming back round through what was typed — so a
    /// completion that guessed wrong is undone by tabbing past the end of the list
    /// rather than by rubbing the word out.
    fn complete_command(&mut self, backward: bool) {
        if let Some(running) = self.completing.as_mut() {
            // One stop past the last candidate is the word as it was typed.
            let stops = running.options.len() + 1;
            running.index = if backward {
                (running.index + stops - 1) % stops
            } else {
                (running.index + 1) % stops
            };
            let at = running.at;
            let word = running
                .options
                .get(running.index)
                .unwrap_or(&running.typed)
                .clone();
            self.set_command_word(at, &word);
            return;
        }
        let line = self.command_line.text();
        let cursor = self.command_line.column();
        let at = word_start(&line, cursor);
        let typed: String = line.chars().take(cursor).skip(at).collect();
        let options = self.command_candidates(&line, at, &typed);
        let Some(first) = options.first().cloned() else {
            return;
        };
        self.completing = Some(Completing {
            at,
            typed,
            options,
            index: 0,
        });
        self.set_command_word(at, &first);
    }

    /// Put `word` in place of the one between `at` and the cursor, and leave the cursor
    /// at the end of it.
    fn set_command_word(&mut self, at: usize, word: &str) {
        let line = self.command_line.text();
        let cursor = self.command_line.column();
        let before: String = line.chars().take(at).collect();
        let after: String = line.chars().skip(cursor).collect();
        self.command_line
            .set_text(&format!("{before}{word}{after}"));
        self.command_line.set_column(at + word.chars().count());
    }

    /// What the word starting at `at` could be, in the order they are offered.
    fn command_candidates(&self, line: &str, at: usize, typed: &str) -> Vec<String> {
        let before: String = line.chars().take(at).collect();
        let mut pool: Vec<String> = if before.trim().is_empty() {
            // The first word names the command, and every program bound to a key
            // answers to its own name as well.
            COMMANDS
                .iter()
                .map(|name| (*name).to_string())
                .chain(self.programs.iter().map(|p| p.name().to_string()))
                .collect()
        } else {
            self.argument_candidates(before.trim())
        };
        pool.sort();
        pool.dedup();
        let typed = typed.to_lowercase();
        pool.retain(|option| option.to_lowercase().starts_with(&typed));
        pool
    }

    /// What can follow a command, for the ones whose argument comes from a list this
    /// client knows. The rest take a name or a title that nothing here can guess, and
    /// are left alone rather than offered a wrong guess.
    fn argument_candidates(&self, before: &str) -> Vec<String> {
        let mut words = before.split_whitespace();
        let name = words.next().unwrap_or_default();
        // Only the first argument: past that these all take free text.
        if words.next().is_some() {
            return Vec::new();
        }
        let fixed: &[&str] = match name {
            "mode" => &["plan", "default"],
            "perm" | "permissions" => RUNTIME_MODES,
            "project" => &["rename"],
            // The efforts are the model's own, and a model that has none offers none.
            "effort" | "e" => {
                return self
                    .effort_descriptor()
                    .map(|d| d.options.iter().map(|o| o.id.clone()).collect())
                    .unwrap_or_default();
            }
            _ => &[],
        };
        fixed.iter().map(|value| (*value).to_string()).collect()
    }

    fn on_picker_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(picker) = self.picker.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        let count = picker.filtered().len();
        match key.code {
            KeyCode::Esc if picker.renaming.is_some() => {
                picker.renaming = None;
                picker.query.clear();
            }
            KeyCode::Esc => {
                self.picker = None;
                self.mode = if self.draft.is_some() {
                    Mode::Insert
                } else {
                    Mode::Normal
                };
            }
            KeyCode::Enter => self.picker_select(),
            // Ctrl-r rather than r: the letters go to the search, which is what the
            // list is driven by. The name being changed takes the search's place, so
            // there is nowhere else the typing could go.
            KeyCode::Char('r')
                if ctrl && picker.kind == PickerKind::Project && picker.renaming.is_none() =>
            {
                let chosen = picker
                    .filtered()
                    .get(picker.selected)
                    .map(|item| (item.key.clone(), item.label.clone()));
                match chosen {
                    Some((project, title)) => {
                        picker.renaming = Some(project);
                        picker.query.set_text(&title);
                    }
                    None => self.toast("no project under the cursor", true),
                }
            }
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
            // Everything else edits the line, with the same keys as every other field.
            // `Ctrl-k` is the exception above: in a list it moves the cursor up, which
            // is worth more here than killing to the end of a line this short.
            _ => {
                let before = picker.query.text();
                edit_key(&mut picker.query, key);
                // A list filtered by something else is a list whose rows have moved, so
                // the row under the cursor is not the one that was under it.
                if picker.query.text() != before && picker.renaming.is_none() {
                    picker.selected = 0;
                }
            }
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
                match &item {
                    // The first list is what is already there, so none of it is news.
                    ShellItem::Snapshot { snapshot } => {
                        for thread in &snapshot.threads {
                            self.marks.insert(thread.id.clone(), mark_of(thread));
                            self.statuses.insert(thread.id.clone(), thread.status());
                        }
                    }
                    ShellItem::ThreadUpserted { thread, .. } => self.note_thread(thread),
                    ShellItem::ThreadRemoved { thread_id, .. } => {
                        self.marks.remove(thread_id);
                        self.statuses.remove(thread_id);
                        self.unseen.remove(thread_id);
                    }
                    _ => {}
                }
                let removed = matches!(&item, ShellItem::ThreadRemoved { thread_id, .. } if Some(thread_id) == self.current_thread_id.as_ref());
                self.shell.apply(item);
                if removed {
                    tracing::info!("the open thread was removed by the server");
                    self.thread = None;
                    self.current_thread_id = None;
                    self.toast("thread was removed", false);
                }
                if self.shell.synchronized {
                    self.open_where_asked();
                }
                if self.current_thread_id.is_none()
                    && self.draft.is_none()
                    // A directory was named and its project is on its way; whatever
                    // thread happens to be first is not what was asked for.
                    && self.open_at.is_none()
                    && self.shell.synchronized
                    && let Some(first) = self.visible_threads().first().cloned()
                {
                    tracing::info!("nothing open, falling back to the first thread");
                    self.open_thread(&first);
                }
                // The sidebar draws a thread with its project's icon, so they are asked
                // for as the projects arrive rather than when a list of them is opened.
                self.ask_favicons();
            }
            Update::Thread { thread_id, item } => {
                if self.current_thread_id.as_deref() != Some(thread_id.as_str()) {
                    return;
                }
                // The thread speaking is the whole of what "the updates are back" means.
                self.lost_stream = None;
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
                self.sync_vcs_watch();
                // A finished turn has usually changed the checkout, and the server's
                // own status cache can be behind the disk.
                let running = self.thread.as_ref().is_some_and(|t| t.is_running());
                if self.was_running && !running {
                    self.handle.refresh_vcs();
                }
                self.was_running = running;
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
            Update::Vcs { cwd, event } => {
                use crate::model::VcsEvent;
                // A status for a directory that is no longer the one being watched is
                // the previous thread's checkout answering for this one's. It arrives
                // because the watch is asked for through a queue the status is already
                // in, and taking it would put the wrong branch under a new worktree.
                if self.vcs_cwd.as_deref() != Some(cwd.as_str()) {
                    return;
                }
                match event {
                    VcsEvent::Snapshot { local, remote } => {
                        self.vcs = Some(local);
                        self.vcs_remote = remote;
                    }
                    VcsEvent::LocalUpdated { local } => self.vcs = Some(local),
                    VcsEvent::RemoteUpdated { remote } => self.vcs_remote = remote,
                    VcsEvent::Unknown => {}
                }
            }
            Update::ThreadStreamError { thread_id, error } => {
                if self.current_thread_id.as_deref() != Some(thread_id.as_str()) {
                    return;
                }
                // Said once as it happens, and then left standing under the header:
                // the toast is what catches the eye, the line is what is still there
                // when you look up two minutes later.
                if self.lost_stream.is_none() {
                    self.toast(format!("lost this thread's updates: {error}"), true);
                }
                self.lost_stream = Some(LostStream {
                    thread_id,
                    error,
                    retry_at: Instant::now() + STREAM_RETRY,
                });
            }
            Update::Error(error) => self.toast(error, true),
        }
    }
}

/// The keys that edit text wherever it is typed, so a message and a custom answer are
/// written the same way. Keys that only make sense in one of them — a newline, prompt
/// history, sending — belong to the caller and are handled before this.
fn edit_key(field: &mut Composer, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Backspace if alt || ctrl => field.kill_word_back(),
        KeyCode::Backspace => field.backspace(),
        KeyCode::Delete => field.delete(),
        KeyCode::Left if alt || ctrl => field.word_left(),
        KeyCode::Right if alt || ctrl => field.word_right(),
        KeyCode::Left => field.left(),
        KeyCode::Right => field.right(),
        KeyCode::Home => field.home(),
        KeyCode::End => field.end(),
        KeyCode::Char('a') if ctrl => field.home(),
        KeyCode::Char('e') if ctrl => field.end(),
        KeyCode::Char('b') if alt => field.word_left(),
        KeyCode::Char('f') if alt => field.word_right(),
        KeyCode::Char('w') if ctrl => field.kill_word_back(),
        KeyCode::Char('k') if ctrl => field.kill_to_end(),
        KeyCode::Char('u') if ctrl => field.kill_to_start(),
        KeyCode::Char(c) if !ctrl => field.insert_char(c),
        _ => {}
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
/// A link as drawn: the row and columns it occupies on screen, and where it points.
/// A link that wraps occupies one of these per screen row.
#[derive(Debug, Clone)]
pub struct Link {
    pub row: u16,
    pub start: u16,
    pub end: u16,
    pub url: String,
}

/// The http links in a line of text, as byte ranges.
///
/// Trailing punctuation is left out, since a URL at the end of a sentence is usually
/// followed by one, and a closing bracket only counts when the URL opened it.
pub fn link_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < text.len() {
        let Some(found) = text[at..]
            .find("http://")
            .into_iter()
            .chain(text[at..].find("https://"))
            .min()
        else {
            break;
        };
        let start = at + found;
        let mut end = start;
        while end < text.len() {
            let byte = bytes[end];
            if byte.is_ascii_whitespace() || matches!(byte, b'<' | b'>' | b'"' | b'`' | b'|') {
                break;
            }
            end += 1;
        }
        while end > start {
            let last = bytes[end - 1];
            let trailing = match last {
                b'.' | b',' | b';' | b':' | b'!' | b'?' | b'\'' => true,
                b')' => count(&text[start..end], b')') > count(&text[start..end], b'('),
                b']' => count(&text[start..end], b']') > count(&text[start..end], b'['),
                _ => false,
            };
            if !trailing {
                break;
            }
            end -= 1;
        }
        // Nothing past the scheme is not a link.
        if text[start..end].len() > "https://".len() {
            ranges.push((start, end));
        }
        at = end.max(start + 1);
    }
    ranges
}

fn count(text: &str, byte: u8) -> usize {
    text.bytes().filter(|b| *b == byte).count()
}

/// Where a visual selection in the chat began, and whether it takes whole lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChatAnchor {
    pub line: usize,
    pub column: usize,
    pub whole_lines: bool,
}

/// A drag over the chat, in screen cells: `anchor` is where the button went down and
/// `head` follows the pointer; either may come first in reading order.
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

/// What kind of run a character belongs to, so a word motion stops where Vim's does:
/// blank, word, or punctuation.
fn word_class(ch: Option<char>) -> u8 {
    match ch {
        None => 0,
        Some(ch) if ch.is_whitespace() => 0,
        Some(ch) if ch.is_alphanumeric() || ch == '_' => 1,
        Some(_) => 2,
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
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableFocusChange
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
        crossterm::event::EnableMouseCapture,
        // Whether the terminal has the focus, which is what decides whether a thread
        // finishing is worth a notification. A terminal that does not answer this
        // leaves it unheard, and unheard errs towards saying something.
        crossterm::event::EnableFocusChange
    );
    // Whatever the terminal was holding for us went out with the other program's screen.
    picture::forget();
    terminal.clear()?;
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => anyhow::bail!("exited with {status}"),
        Err(err) => Err(err.into()),
    }
}

/// The tmux command that opens the pane, as its arguments.
///
/// Side by side rather than one above the other: tria is a tall window and a shell
/// under it would have a dozen rows, where beside it both halves keep their height.
fn split_args(dir: &str, pane: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "split-window".to_string(),
        "-h".to_string(),
        "-c".to_string(),
        dir.to_string(),
    ];
    if let Some(pane) = pane {
        args.push("-t".to_string());
        args.push(pane.to_string());
    }
    args
}

/// The tmux command that opens the window, as its arguments.
///
/// No target: tria was started inside the session it is in, so that is the one tmux
/// resolves from the environment. No position either, so the window lands where this
/// session puts a new one — the point is that it is `prefix c` with a directory, and
/// anything else would be tria having an opinion about somebody's window numbering.
fn window_args(dir: &str) -> Vec<String> {
    vec!["new-window".to_string(), "-c".to_string(), dir.to_string()]
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
    pub programs: Vec<crate::config::Program>,
    pub sidebar_layout: crate::config::SidebarLayout,
    /// Bindings the config asked for that tria could not give, to be said out loud once
    /// the screen exists to say them on.
    pub refused: Vec<String>,
    pub editor: String,
    /// The model remembered from the last choice, for the next new thread.
    pub model: Option<ModelSelection>,
    /// Whether tria had to start the server it is about to talk to.
    pub started_server: bool,
    /// How long `g` and `z` wait for the key after them; `None` is indefinitely.
    pub prefix_timeout: Option<Duration>,
    /// When a thread that has stopped working is announced to the desktop.
    pub notify: crate::notify::When,
    /// The directory `tria open` named, if it was `tria open`.
    pub open_at: Option<String>,
}

/// Ask where a project's icon is and fetch it. `sourcePath` is the server saying it found
/// one; without it the URL leads to nothing and there is no icon to draw.
/// Write a picture that came as base64 to a file, and say where. Named after the bytes
/// themselves so that the same picture is the same file: a viewer left open on it sees
/// the same path the next time, and the temporary directory does not fill up with copies.
fn write_picture(data: &str) -> Option<String> {
    use base64::Engine;
    use std::hash::{Hash, Hasher};

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .ok()?;
    // What kind of picture it is is in the bytes, and the viewer is likely to want it in
    // the name: a file called `.bin` opens in a text editor.
    let format = image::guess_format(&bytes).ok()?;
    let extension = format.extensions_str().first().copied()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let path = std::env::temp_dir().join(format!("tria-{:016x}.{extension}", hasher.finish()));
    if !path.exists() {
        std::fs::write(&path, &bytes).ok()?;
    }
    Some(path.to_string_lossy().into_owned())
}

async fn favicon_bytes(handle: &session::Handle, origin: &str, cwd: &str) -> Option<Vec<u8>> {
    let answer = handle
        .call(
            "assets.createUrl",
            json!({ "resource": { "_tag": "project-favicon", "cwd": cwd } }),
        )
        .await
        .ok()?;
    answer.get("sourcePath")?;
    let url = answer.get("relativeUrl")?.as_str()?;
    let response = reqwest::get(format!("{origin}{url}")).await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    Some(response.bytes().await.ok()?.to_vec())
}

/// What a thread looks like from the list: which turn it is on, how that turn ended,
/// when it was last written to, and what its background work is up to. Anything else
/// the server touches is bookkeeping and is not a thread saying something.
///
/// Background work counts because a thread can go quiet without the turn changing at
/// all: the answer was written long ago and what is left is a watcher, which settling
/// into monitoring is the whole of the news.
fn mark_of(thread: &crate::model::ThreadShell) -> String {
    let turn = thread.latest_turn.as_ref();
    format!(
        "{}/{}/{}/{}",
        turn.map(|t| t.turn_id.as_str()).unwrap_or_default(),
        turn.map(|t| t.state.as_str()).unwrap_or_default(),
        thread.latest_user_message_at.as_deref().unwrap_or_default(),
        thread.background_liveness.as_deref().unwrap_or_default(),
    )
}

/// Do what an event asks of the app. Drawing is the caller's, so that a burst of them
/// costs one screen rather than one each.
fn apply(app: &mut App, event: AppEvent) {
    match event {
        AppEvent::Terminal(Event::Key(key)) => app.on_key(key),
        AppEvent::Terminal(Event::Mouse(mouse)) => app.on_mouse(mouse),
        AppEvent::Terminal(Event::Paste(text)) => app.on_paste(&text),
        AppEvent::Terminal(Event::FocusGained) => {
            app.terminal_focus = crate::notify::Focus::Focused
        }
        AppEvent::Terminal(Event::FocusLost) => {
            app.terminal_focus = crate::notify::Focus::Unfocused
        }
        AppEvent::Terminal(_) => {}
        AppEvent::Tick => {
            app.spinner = app.spinner.wrapping_add(1);
            app.poll_popup();
            app.retry_lost_stream();
            app.refresh_vcs_periodically();
            app.refresh_worktrees_periodically();
            if app
                .toast
                .as_ref()
                .is_some_and(|(_, at, _)| at.elapsed() > TOAST_TTL)
            {
                app.toast = None;
            }
        }
        AppEvent::Update(update) => app.on_update(*update),
        AppEvent::ThreadCreated(thread_id) => app.on_thread_created(thread_id),
        AppEvent::CreateRefused { thread_id, error } => app.on_create_refused(thread_id, error),
        AppEvent::SendRefused {
            thread_id,
            text,
            error,
        } => app.on_send_refused(thread_id, text, error),
        AppEvent::UsageRead(read) => app.on_usage_read(read),
        AppEvent::WorktreeChecked {
            path,
            changes,
            files,
        } => app.on_worktree_checked(path, changes, files),
        AppEvent::WorktreeRemoved { path, result } => app.on_worktree_removed(path, result),
        AppEvent::Dispatched(Err(error)) => app.toast(format!("command failed: {error}"), true),
        AppEvent::Dispatched(Ok(())) => {}
        AppEvent::Called { result, ok } => match result {
            Ok(()) => app.toast(ok, false),
            Err(error) => app.toast(error, true),
        },
        AppEvent::Transcript { agent_id, result } => app.on_transcript(agent_id, result),
        AppEvent::Favicon { project, bytes } => app.on_favicon(project, bytes),
    }
}

pub async fn run(origin: String, token: String, launch: Launch) -> Result<()> {
    // A thread's images are files on the server's disk, which are ours to read only when
    // that disk is this one.
    let local_files = crate::server::is_local(&origin);
    let (handle, mut updates) = session::spawn(origin.clone(), token);
    let (events_tx, mut events) = mpsc::unbounded_channel::<AppEvent>();
    let mut app = App::new(handle, events_tx.clone());
    app.local_disk = local_files;
    app.origin = origin;
    app.programs = launch.programs;
    app.sidebar_layout = launch.sidebar_layout;
    if !launch.refused.is_empty() {
        app.toast(format!("config: {}", launch.refused.join(", ")), true);
    }
    app.editor = launch.editor;
    app.prefix_timeout = launch.prefix_timeout;
    app.notify = launch.notify;
    app.new_thread_model = launch.model;
    app.open_at = launch.open_at;
    if launch.started_server {
        // Said again here because the line printed before the screen was taken over is
        // gone, and starting a server that outlives tria is worth knowing about.
        app.toast("started the T3 Code server", false);
    }

    let mut terminal = ratatui::init();
    // Before the event reader starts: the terminal answers what it can draw on stdin,
    // and the reader would take the answer for typing.
    picture::ask_terminal(local_files);
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableBracketedPaste,
        crossterm::event::EnableMouseCapture,
        // Whether the terminal has the focus, which is what decides whether a thread
        // finishing is worth a notification. A terminal that does not answer this
        // leaves it unheard, and unheard errs towards saying something.
        crossterm::event::EnableFocusChange
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
        apply(&mut app, event);
        // Whatever else is already waiting is applied before the screen is drawn again.
        // One picture reaching the pane arrives as a hundred messages, and a screen
        // drawn between two of them is a screen nobody sees.
        for _ in 0..BATCH {
            let waiting = updates
                .try_recv()
                .ok()
                .map(|update| AppEvent::Update(Box::new(update)))
                .or_else(|| events.try_recv().ok());
            match waiting {
                Some(event) => apply(&mut app, event),
                None => break,
            }
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
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableFocusChange
    );
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::model::{Project, VcsLocal, VcsWorkingTree};

    fn selection() -> ModelSelection {
        ModelSelection {
            instance_id: "instance".into(),
            model: "a-model".into(),
            options: vec![],
        }
    }

    fn project(app: &mut App) -> Id {
        app.shell.projects.insert(
            "p".into(),
            Project {
                id: "p".into(),
                title: "p".into(),
                workspace_root: "/src/p".into(),
                project_icon: None,
                default_model_selection: None,
                default_thread_env_mode: None,
            },
        );
        app.new_thread_model = Some(selection());
        "p".into()
    }

    fn status(ref_name: &str) -> crate::model::VcsLocal {
        VcsLocal {
            is_repo: true,
            ref_name: Some(ref_name.into()),
            is_default_ref: true,
            has_working_tree_changes: false,
            working_tree: VcsWorkingTree::default(),
        }
    }

    /// The watch is asked for through a queue, so the status of the directory just left
    /// can arrive after the view has moved to another. Taken, it would answer for a
    /// checkout it does not describe — and a new worktree would be branched off the
    /// previous thread's branch, which the server may not be able to find at all.
    #[test]
    fn a_status_for_a_directory_no_longer_watched_is_not_taken() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.vcs_cwd = Some("/src/p".into());

        app.on_update(crate::session::Update::Vcs {
            cwd: "/src/p/../worktree".into(),
            event: crate::model::VcsEvent::LocalUpdated {
                local: status("a-worktree-branch"),
            },
        });
        assert!(app.vcs.is_none(), "a status from elsewhere was taken");

        app.on_update(crate::session::Update::Vcs {
            cwd: "/src/p".into(),
            event: crate::model::VcsEvent::LocalUpdated {
                local: status("main"),
            },
        });
        assert_eq!(
            app.vcs.as_ref().and_then(|vcs| vcs.ref_name.as_deref()),
            Some("main")
        );
    }

    /// A refused create used to leave the view on a thread that was never made and the
    /// message nowhere at all.
    #[tokio::test]
    async fn a_thread_the_server_refuses_gives_the_message_back() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        let project_id = project(&mut app);
        app.start_new_thread(&project_id);
        app.composer.set_text("the message");
        app.send_message();

        let thread_id = app
            .current_thread_id
            .clone()
            .expect("a thread was asked for");
        assert!(app.draft.is_none() && app.composer.text().is_empty());

        app.on_create_refused(thread_id, "git worktree add failed".into());

        assert!(
            app.current_thread_id.is_none(),
            "still on a thread that is not there"
        );
        assert_eq!(app.draft.map(|d| d.project_id), Some(project_id));
        assert_eq!(app.composer.text(), "the message");
        assert_eq!(
            app.toast.map(|t| t.0),
            Some("git worktree add failed".to_string())
        );
    }

    fn shell_thread(id: &str, project: &str, worktree: Option<&str>) -> crate::model::ThreadShell {
        serde_json::from_value(json!({
            "id": id,
            "projectId": project,
            "title": id,
            "modelSelection": { "instanceId": "i", "model": "m", "options": [] },
            "worktreePath": worktree,
            "settledAt": "2026-01-01T00:00:00Z",
        }))
        .expect("a thread the server could have sent")
    }

    /// A thread can go quiet without its turn changing at all: the answer was written
    /// long ago and what is left is a watcher. Settling into monitoring is the news.
    #[test]
    fn background_work_settling_down_is_a_thread_speaking() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        upsert(&mut app, watching("a", "working"));
        assert!(app.unseen.is_empty(), "a thread first heard of is not news");

        // Still working, and the list already says so.
        upsert(&mut app, watching("a", "working"));
        assert!(app.unseen.is_empty());

        // Dropping to a watcher is what there was to hear.
        upsert(&mut app, watching("a", "monitoring"));
        assert!(app.unseen.contains("a"));
    }

    /// `g` and a key run whatever the config file put there, in the thread's own
    /// terminal — the binding tria ships with is only the first entry in that list.
    #[tokio::test]
    async fn a_program_the_config_bound_runs_on_its_key() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        project(&mut app);
        app.programs = vec![crate::config::Program {
            key: 'b',
            command: "yazi".into(),
        }];
        app.current_thread_id = Some("t1".into());
        app.thread = Some(crate::state::ThreadState::from_snapshot(
            serde_json::from_value(json!({
                "snapshotSequence": 1,
                "thread": {
                    "id": "t1", "projectId": "p", "title": "Test",
                    "modelSelection": {"instanceId": "i", "model": "m"},
                    "messages": [], "activities": []
                }
            }))
            .unwrap(),
        ));

        let press = |app: &mut App, c: char| {
            app.on_key(crossterm::event::KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            ))
        };
        press(&mut app, 'g');
        press(&mut app, 'b');
        assert_eq!(app.pending_pane_command.as_deref(), Some("exec yazi\r"));
        let pane = app.pane.as_ref().expect("the popup is open");
        assert_eq!(pane.terminal_id, "tria-gb");
        assert_eq!(pane.label, "yazi");

        // A key the config did not bind is still not a key.
        app.pending_pane_command = None;
        press(&mut app, 'g');
        press(&mut app, 'q');
        assert_eq!(app.pending_pane_command, None);
    }

    /// The figures on the config are as old as the connection that fetched them, so
    /// asking to see them asks the server for them again.
    #[tokio::test]
    async fn opening_the_usage_window_reads_the_quota_again() {
        let (handle, mut requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);

        app.run_command("usage");
        tokio::task::yield_now().await;
        assert_eq!(app.mode, Mode::Usage);

        let asked = std::iter::from_fn(|| requests.try_recv().ok()).filter(|request| {
            matches!(request, crate::session::Request::Call { tag, .. } if tag == "server.getConfig")
        });
        assert_eq!(asked.count(), 1, "the config was not read again");
        // The window is drawn from the config, so asking again while the first ask is
        // still out would only be a second answer to the same question.
        app.on_usage_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        tokio::task::yield_now().await;
        let again = std::iter::from_fn(|| requests.try_recv().ok()).count();
        assert_eq!(again, 0, "it asked twice for one answer");

        // Once the answer is in, it can be asked for again.
        app.on_usage_read(Ok(Box::default()));
        app.on_usage_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        tokio::task::yield_now().await;
        assert_eq!(std::iter::from_fn(|| requests.try_recv().ok()).count(), 1);
    }

    /// A thread the server says is monitoring. The liveness sits on the list row,
    /// which is the only place the server puts it, and the thread's own history holds no
    /// row for the watcher — the usual shape of one left running for hours, since the
    /// loaded history only reaches so far back.
    fn watched(app: &mut App, liveness: Option<&str>, session: &str) {
        let mut row = listed("t1", Some(("turn", "completed")));
        row["session"] = json!({ "status": session });
        if let Some(liveness) = liveness {
            row["backgroundLiveness"] = json!(liveness);
        }
        upsert(app, row);
        app.current_thread_id = Some("t1".into());
        app.thread = Some(crate::state::ThreadState::from_snapshot(
            serde_json::from_value(json!({
                "snapshotSequence": 1,
                "thread": {
                    "id": "t1", "projectId": "p", "title": "t1",
                    "modelSelection": {"instanceId": "i", "model": "m"},
                    "session": {"status": session},
                    "latestTurn": {
                        "turnId": "turn", "state": "completed",
                        "requestedAt": "2026-01-01T00:00:00Z"
                    },
                    "messages": [], "activities": []
                }
            }))
            .unwrap(),
        ));
    }

    fn dispatched(
        requests: &mut mpsc::UnboundedReceiver<crate::session::Request>,
    ) -> Vec<serde_json::Value> {
        std::iter::from_fn(|| requests.try_recv().ok())
            .filter_map(|request| match request {
                crate::session::Request::Dispatch { command, .. } => Some(command),
                _ => None,
            })
            .collect()
    }

    /// A watcher outlives the turn that started it, so by the time it is worth stopping
    /// there is no turn to name and nothing is "running". The interrupt still goes —
    /// without a turn id, which is what the desktop's `Monitoring · Stop` sends — and
    /// the session stops the watch loop.
    #[tokio::test]
    async fn a_thread_only_monitoring_can_still_be_stopped() {
        let (handle, mut requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        watched(&mut app, Some("monitoring"), "idle");
        assert!(!app.thread.as_ref().unwrap().is_running());
        // The detail snapshot carries no liveness; the list row is where it lives.
        assert_eq!(app.background_liveness(), Some("monitoring"));

        app.on_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT));
        assert_eq!(app.mode, Mode::Tasks);
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        tokio::task::yield_now().await;

        let commands = dispatched(&mut requests);
        let interrupt = commands
            .iter()
            .find(|command| command["type"] == "thread.turn.interrupt")
            .expect("the watcher was never stopped");
        assert_eq!(interrupt["threadId"], "t1");
        // No turn is running, and naming one that is not would be naming the wrong one.
        assert!(interrupt["turnId"].is_null());
        // The list stays open: the rows clearing is what says it worked.
        assert_eq!(app.mode, Mode::Tasks);
    }

    /// With nothing running and nothing watching, the interrupt is not sent at all.
    #[tokio::test]
    async fn a_quiet_thread_has_nothing_to_interrupt() {
        let (handle, mut requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        watched(&mut app, None, "ready");

        app.run_command("stop");
        tokio::task::yield_now().await;
        assert!(dispatched(&mut requests).is_empty());
    }

    /// The harder stop, for a watcher that will not let go of the session.
    #[tokio::test]
    async fn the_session_itself_can_be_stopped() {
        let (handle, mut requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        watched(&mut app, Some("monitoring"), "idle");

        app.run_command("stop!");
        tokio::task::yield_now().await;

        let stopped = dispatched(&mut requests)
            .iter()
            .any(|command| command["type"] == "thread.session.stop" && command["threadId"] == "t1");
        assert!(stopped, "the session was never stopped");
    }

    /// `S` in the task list is `s` with shift held, and it ends the whole session
    /// rather than the one task. It asks before it does, and only the answer answers.
    #[tokio::test]
    async fn stopping_the_session_from_the_list_is_asked_about_first() {
        let (handle, mut requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        watched(&mut app, Some("monitoring"), "idle");

        app.on_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT));
        app.on_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT));
        tokio::task::yield_now().await;
        assert!(app.confirm_stop_session, "it asks");
        assert_eq!(app.mode, Mode::Tasks, "and stays on the list to ask on it");
        assert!(
            !stopped(&mut requests),
            "the session is still running until it is answered"
        );

        // Anything but the answer is a no, and leaves the list as it was.
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        tokio::task::yield_now().await;
        assert!(!app.confirm_stop_session);
        assert_eq!(app.mode, Mode::Tasks);
        assert!(!stopped(&mut requests), "a stray key does not stop it");

        // Pressing it again is the answer.
        app.on_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT));
        app.on_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT));
        tokio::task::yield_now().await;
        assert!(!app.confirm_stop_session);
        assert!(stopped(&mut requests), "the session was never stopped");
    }

    /// Whether a session stop went out.
    fn stopped(requests: &mut mpsc::UnboundedReceiver<crate::session::Request>) -> bool {
        dispatched(requests)
            .iter()
            .any(|command| command["type"] == "thread.session.stop")
    }

    /// A thread as the list has it, on a given turn in a given state.
    fn listed(id: &str, turn: Option<(&str, &str)>) -> serde_json::Value {
        json!({
            "id": id,
            "projectId": "p",
            "title": id,
            "modelSelection": { "instanceId": "i", "model": "m", "options": [] },
            "latestTurn": turn.map(|(turn_id, state)| json!({
                "turnId": turn_id, "state": state, "requestedAt": "2026-01-01T00:00:00Z"
            })),
        })
    }

    /// The same, with a title of its own and, when it has stopped, what it stopped for.
    fn titled(id: &str, title: &str, state: &str, approval: bool) -> serde_json::Value {
        let mut thread = listed(id, Some(("turn", state)));
        thread["title"] = json!(title);
        thread["hasPendingApprovals"] = json!(approval);
        thread
    }

    /// The same, with background work still alive after the turn.
    fn watching(id: &str, liveness: &str) -> serde_json::Value {
        let mut thread = listed(id, Some(("t1", "completed")));
        thread["backgroundLiveness"] = json!(liveness);
        thread
    }

    fn upsert(app: &mut App, thread: serde_json::Value) {
        app.on_update(crate::session::Update::Shell(
            crate::model::ShellItem::ThreadUpserted {
                sequence: 1,
                thread: serde_json::from_value(thread).expect("a thread the server could send"),
            },
        ));
    }

    /// Which threads have spoken while you were looking elsewhere is only worth knowing
    /// about the ones that spoke while you were there to miss it.
    #[test]
    fn a_thread_that_speaks_while_you_are_elsewhere_is_marked() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.on_update(crate::session::Update::Shell(
            crate::model::ShellItem::Snapshot {
                snapshot: serde_json::from_value(json!({
                    "snapshotSequence": 1,
                    "threads": [listed("a", Some(("t1", "completed"))), listed("b", None)],
                }))
                .unwrap(),
            },
        ));
        // The list as it stands when tria connects is not news.
        assert!(app.unseen.is_empty());
        app.current_thread_id = Some("a".into());

        // A turn finishing in a thread nobody is looking at is.
        upsert(&mut app, listed("b", Some(("t2", "completed"))));
        assert!(app.unseen.contains("b"));

        // The open thread is being read as it arrives.
        upsert(&mut app, listed("a", Some(("t3", "completed"))));
        assert!(!app.unseen.contains("a"));

        // Opening it is having seen it.
        app.open_thread("b");
        assert!(app.unseen.is_empty());

        // A thread that has only started working has nothing to show yet: the list
        // already says it is working.
        upsert(&mut app, listed("c", None));
        upsert(&mut app, listed("c", Some(("t4", "running"))));
        assert!(!app.unseen.contains("c"));
        // And says so when it stops.
        upsert(&mut app, listed("c", Some(("t4", "completed"))));
        assert!(app.unseen.contains("c"));

        // The server mentioning a thread that has not moved is not a thread speaking.
        app.unseen.clear();
        upsert(&mut app, listed("c", Some(("t4", "completed"))));
        assert!(app.unseen.is_empty());
    }

    /// A worktree is offered when a thread has one of its own and it is still there. A
    /// project rooted in a worktree is not: it is a place to work, and taking it away
    /// would take the project with it.
    #[test]
    fn only_the_worktrees_a_thread_is_holding_are_offered() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.local_disk = false;
        project(&mut app);
        app.shell.projects.insert(
            "rooted".into(),
            Project {
                id: "rooted".into(),
                title: "rooted".into(),
                // A project whose root is itself a worktree of another repository.
                workspace_root: "/worktrees/rooted".into(),
                project_icon: None,
                default_model_selection: None,
                default_thread_env_mode: None,
            },
        );
        for thread in [
            shell_thread("has-one", "p", Some("/worktrees/p/one")),
            shell_thread("in-the-checkout", "p", None),
            shell_thread("is-a-project", "rooted", Some("/worktrees/rooted")),
        ] {
            app.shell.threads.insert(thread.id.clone(), thread);
        }

        let offered: Vec<String> = app
            .collect_worktrees()
            .into_iter()
            .map(|worktree| worktree.thread_id)
            .collect();
        assert_eq!(offered, ["has-one"]);

        // Where the disk is ours, one that has already gone is not offered either —
        // which is most of them, since a thread keeps the path long after the worktree.
        let here = std::env::temp_dir().join("tria-a-worktree-that-is-here");
        std::fs::create_dir_all(&here).unwrap();
        app.local_disk = true;
        app.shell
            .threads
            .insert("here".into(), shell_thread("here", "p", here.to_str()));
        let offered: Vec<String> = app
            .collect_worktrees()
            .into_iter()
            .map(|worktree| worktree.thread_id)
            .collect();
        assert_eq!(offered, ["here"]);
        std::fs::remove_dir(&here).unwrap();
    }

    /// Clicking into the composer is how a text box is asked for: the cursor goes where
    /// the click was and typing works, without a trip through normal mode first.
    #[test]
    fn clicking_the_composer_puts_the_cursor_there_and_writes() {
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.composer_area = Rect::new(2, 10, 20, 2);
        app.composer.set_text("hello world");
        app.focus = Focus::Chat;

        let click = |column, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(click(2 + 6, 10));

        assert_eq!(app.mode, Mode::Insert);
        assert_eq!(app.focus, Focus::Composer);
        assert_eq!((app.composer.row, app.composer.col), (0, 6));
        app.composer.insert_str("wide ");
        assert_eq!(app.composer.text(), "hello wide world");

        // Outside it, the composer is left alone.
        app.mode = Mode::Normal;
        app.on_mouse(click(2, 20));
        assert_eq!(app.mode, Mode::Normal);
    }

    /// The thread's directory is a path on the machine the server runs on, so where that
    /// is another machine it is not a directory to open here — it is either nothing or
    /// somebody else's directory of the same name.
    #[tokio::test]
    async fn a_directory_on_another_machine_is_not_opened() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        project(&mut app);
        watched(&mut app, None, "running");
        app.local_disk = false;

        app.reveal_directory();
        let said = app.toast.as_ref().expect("it says why not").0.clone();
        assert!(said.contains("server's machine"), "{said}");

        // And with no thread at all there is no directory to mean.
        app.thread = None;
        app.reveal_directory();
        assert!(app.toast.unwrap().0.contains("no thread open"));
    }

    /// `gx` on a row with a picture opens the picture, which is the one thing on a chat
    /// line that is really somewhere else: the terminal only ever drew a thumbnail.
    #[test]
    fn the_picture_under_the_cursor_is_the_one_that_opens() {
        use crate::timeline::{Picture, Region};
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.work_ranges = vec![
            Region {
                first: 0,
                end: 9,
                key: "work-1".into(),
                foldable: true,
            },
            Region {
                first: 2,
                end: 8,
                key: "work-1/t1".into(),
                foldable: true,
            },
        ];
        app.chat_pictures = vec![("work-1/t1".into(), Picture::File("/tmp/shot.png".into()))];

        // Anywhere in the row, including the lines the picture was drawn over.
        app.chat_cursor = 5;
        assert_eq!(
            app.picture_at_cursor(),
            Some(Picture::File("/tmp/shot.png".into()))
        );
        // The group around it is not the row, and has no picture of its own.
        app.chat_cursor = 1;
        assert_eq!(app.picture_at_cursor(), None);

        // A picture a message drew belongs to no row at all: it answers for the caption
        // and the lines under it, and `gx` opens it from any of them.
        app.picture_ranges = vec![Region {
            first: 20,
            end: 26,
            key: "msg:m1/0".into(),
            foldable: false,
        }];
        app.chat_pictures
            .push(("msg:m1/0".into(), Picture::File("/tmp/shown.png".into())));
        for line in [20, 25] {
            app.chat_cursor = line;
            assert_eq!(
                app.picture_at_cursor(),
                Some(Picture::File("/tmp/shown.png".into())),
                "line {line}"
            );
        }
        app.chat_cursor = 26;
        assert_eq!(app.picture_at_cursor(), None, "and no further");
    }

    /// A picture that came inside a transcript is not a file anywhere, so opening it
    /// means writing it out — and writing the same picture twice is the same file.
    #[test]
    fn a_picture_with_no_file_is_given_one() {
        use base64::Engine;
        let data = crate::picture::test_png(8, 8);
        let path = write_picture(&data).expect("a png is a picture");
        assert!(path.ends_with(".png"), "{path}");
        assert_eq!(write_picture(&data).as_deref(), Some(path.as_str()));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            base64::engine::general_purpose::STANDARD
                .decode(&data)
                .unwrap()
        );
        std::fs::remove_file(&path).unwrap();
        // Something that is not a picture is not written out at all.
        assert_eq!(write_picture("bm90IGEgcGljdHVyZQ=="), None);
    }

    /// A turn takes minutes and the whole point of the client is not having to watch it.
    /// What is announced is a thread that has stopped working, whichever thread it is —
    /// the open one finishing while you are in another window is the case this is for.
    #[test]
    fn a_thread_that_stops_working_says_so() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.terminal_focus = crate::notify::Focus::Unfocused;

        // First sight is not news: every thread would announce itself on the way in.
        upsert(
            &mut app,
            titled("t1", "Fix the auth redirect", "running", false),
        );
        assert!(crate::notify::sent().is_empty());

        // Working to anything else is.
        upsert(
            &mut app,
            titled("t1", "Fix the auth redirect", "completed", false),
        );
        assert_eq!(
            crate::notify::sent(),
            vec!["Fix the auth redirect · finished"]
        );

        // The same status again is the same thread saying the same thing.
        upsert(
            &mut app,
            titled("t1", "Fix the auth redirect", "completed", false),
        );
        assert!(crate::notify::sent().is_empty());

        // What it stopped for is what it says.
        upsert(
            &mut app,
            titled("t1", "Fix the auth redirect", "running", false),
        );
        upsert(
            &mut app,
            titled("t1", "Fix the auth redirect", "running", true),
        );
        assert_eq!(
            crate::notify::sent(),
            vec!["Fix the auth redirect · needs approval"]
        );
    }

    /// The default is not to talk over something already on the screen in front of you.
    #[test]
    fn nothing_is_announced_while_the_terminal_has_the_focus() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);

        for (focus, expected) in [
            (crate::notify::Focus::Focused, 0),
            (crate::notify::Focus::Unfocused, 1),
            // A terminal that never mentions focus is told anyway, since the other way
            // round is the feature silently not working.
            (crate::notify::Focus::Unheard, 1),
        ] {
            app.statuses.clear();
            app.terminal_focus = focus;
            upsert(&mut app, titled("t1", "a thread", "running", false));
            upsert(&mut app, titled("t1", "a thread", "completed", false));
            assert_eq!(crate::notify::sent().len(), expected, "{focus:?}");
        }

        // And `never` is never, focus or no focus.
        app.notify = crate::notify::When::Never;
        for focus in [
            crate::notify::Focus::Focused,
            crate::notify::Focus::Unfocused,
            crate::notify::Focus::Unheard,
        ] {
            app.statuses.clear();
            app.terminal_focus = focus;
            upsert(&mut app, titled("t1", "a thread", "running", false));
            upsert(&mut app, titled("t1", "a thread", "completed", false));
            assert!(crate::notify::sent().is_empty(), "{focus:?}");
        }
    }

    /// The list that arrives on connecting is what is already there, and a reconnection
    /// brings it again. Neither is a pile of threads finishing.
    #[test]
    fn the_list_arriving_announces_nothing() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.terminal_focus = crate::notify::Focus::Unfocused;

        let snapshot = |threads: serde_json::Value| {
            Update::Shell(
                serde_json::from_value(serde_json::json!({
                    "kind": "snapshot",
                    "snapshot": {
                        "snapshotSequence": 1, "projects": [], "threads": threads
                    }
                }))
                .expect("a shell snapshot the server could have sent"),
            )
        };
        app.on_update(snapshot(serde_json::json!([
            titled("t1", "one", "running", false),
            titled("t2", "two", "completed", false),
        ])));
        assert!(crate::notify::sent().is_empty(), "nothing on the way in");

        // The running one finishing after that is news.
        upsert(&mut app, titled("t1", "one", "completed", false));
        assert_eq!(crate::notify::sent(), vec!["one · finished"]);
    }

    /// `g` waits for the key that finishes it, and how long is the config's to say —
    /// including forever, for anyone who would rather `gA` never came out as `A`.
    #[test]
    fn how_long_g_waits_is_the_config_s_to_say() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        let pressed_g_a_moment_ago = |app: &mut App| {
            app.pending_prefix = Some(('g', Instant::now() - Duration::from_secs(5)))
        };

        // The default gives it back after its moment has passed, so the key that
        // follows is the key it is rather than half of a pair.
        pressed_g_a_moment_ago(&mut app);
        assert_eq!(app.waiting_prefix(), None, "nothing is shown as waiting");
        assert_eq!(app.take_prefix(), None);

        // Waiting indefinitely is what `prefix_timeout_ms = 0` asks for.
        app.prefix_timeout = None;
        pressed_g_a_moment_ago(&mut app);
        assert_eq!(app.waiting_prefix(), Some('g'), "and it says it is waiting");
        assert_eq!(app.take_prefix(), Some('g'));
        assert_eq!(app.waiting_prefix(), None, "taking it is the end of it");
    }

    /// The pair only has to hold together for as long as it is configured to. With no
    /// timeout at all, a `g` pressed and left is still a `g` whenever the next key lands.
    #[test]
    fn a_prefix_that_waits_is_still_there_to_be_completed() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.prefix_timeout = None;
        app.focus = Focus::Chat;

        app.on_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
        assert_eq!(app.waiting_prefix(), Some('z'));
        // Long enough that the default would have given up on it.
        app.pending_prefix = Some(('z', Instant::now() - Duration::from_secs(60)));
        app.on_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
        assert_eq!(app.open_levels, MOST_OPEN_LEVELS, "zR, not R");
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// Type a line, character by character, as somebody at the keyboard would.
    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    fn plain(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// The `:` line was a string with a push and a pop: no cursor to move, no way to
    /// rub out a word, and every command typed out in full every time. It is the same
    /// one-line editor the rest of the client uses now, so it has all of that.
    #[test]
    fn the_command_line_edits_like_every_other_field() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);

        typed(&mut app, ":project rename a name");
        assert_eq!(app.mode, Mode::Command);
        app.on_key(ctrl('w'));
        assert_eq!(app.command_line.text(), "project rename a ");

        // The cursor moves, and what is typed goes in where it is.
        app.on_key(ctrl('a'));
        typed(&mut app, "x");
        assert_eq!(app.command_line.text(), "xproject rename a ");
        app.on_key(plain(KeyCode::Delete));
        app.on_key(plain(KeyCode::Backspace));
        assert_eq!(app.command_line.text(), "roject rename a ");
        app.on_key(ctrl('e'));
        app.on_key(ctrl('u'));
        assert!(app.command_line.text().is_empty());

        // And an empty line with nothing left to rub out is a command given up on.
        app.on_key(plain(KeyCode::Backspace));
        assert_eq!(app.mode, Mode::Normal);
    }

    /// The picker's query and the chat search were strings with a push and a pop too.
    /// They are the same one-line editor as everything else now, less the keys the list
    /// under the query has already claimed.
    #[test]
    fn a_picker_query_and_a_search_edit_like_every_other_field() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        project(&mut app);

        app.open_picker(PickerKind::Project);
        typed(&mut app, "one two");
        app.on_key(ctrl('w'));
        assert_eq!(app.picker.as_ref().unwrap().query.text(), "one ");
        app.on_key(ctrl('a'));
        typed(&mut app, "x");
        assert_eq!(app.picker.as_ref().unwrap().query.text(), "xone ");
        // Ctrl-k belongs to the list, which has a row above the one selected.
        app.picker.as_mut().unwrap().selected = 1;
        app.on_key(ctrl('k'));
        assert_eq!(app.picker.as_ref().unwrap().selected, 0);
        assert_eq!(
            app.picker.as_ref().unwrap().query.text(),
            "xone ",
            "and takes nothing off the line"
        );

        // A filter that changed puts the cursor back on the first row of the answer.
        app.picker.as_mut().unwrap().selected = 1;
        app.on_key(plain(KeyCode::Backspace));
        assert_eq!(app.picker.as_ref().unwrap().selected, 0);

        app.on_key(plain(KeyCode::Esc));
        app.thread = Some(running_thread());
        app.focus = Focus::Chat;
        app.start_search(false);
        typed(&mut app, "one two");
        app.on_key(ctrl('w'));
        app.on_key(ctrl('a'));
        typed(&mut app, "x");
        assert_eq!(
            app.search_input
                .as_ref()
                .expect("a search is open")
                .query
                .text(),
            "xone "
        );
    }

    /// A command run once is on the arrow keys, so the long ones are typed once.
    #[test]
    fn the_command_line_remembers_what_was_run() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);

        typed(&mut app, ":sidebar");
        app.on_key(plain(KeyCode::Enter));
        typed(&mut app, ":settled");
        app.on_key(plain(KeyCode::Enter));

        typed(&mut app, ":");
        app.on_key(plain(KeyCode::Up));
        assert_eq!(app.command_line.text(), "settled");
        app.on_key(plain(KeyCode::Up));
        assert_eq!(app.command_line.text(), "sidebar");
        app.on_key(plain(KeyCode::Down));
        assert_eq!(app.command_line.text(), "settled");
        app.on_key(plain(KeyCode::Down));
        assert_eq!(
            app.command_line.text(),
            "",
            "and back to what was being typed"
        );
    }

    /// `Tab` offers the commands that start the way the word does, and tabbing past the
    /// last one gives back what was typed rather than leaving a wrong guess in the line.
    #[test]
    fn tab_completes_a_command_and_cycles_through_the_rest() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);

        typed(&mut app, ":sett");
        app.on_key(plain(KeyCode::Tab));
        assert_eq!(app.command_line.text(), "settle");
        app.on_key(plain(KeyCode::Tab));
        assert_eq!(app.command_line.text(), "settled");
        app.on_key(plain(KeyCode::Tab));
        assert_eq!(app.command_line.text(), "sett", "round to what was typed");
        app.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.command_line.text(), "settled", "and back the other way");

        // A word that is nobody's prefix is left exactly as it is.
        app.on_key(plain(KeyCode::Esc));
        typed(&mut app, ":zzz");
        app.on_key(plain(KeyCode::Tab));
        assert_eq!(app.command_line.text(), "zzz");
        assert!(app.completing.is_none());
    }

    /// The arguments that come from a fixed list are completed too; the ones that are a
    /// name or a title are left alone, since guessing at one would only be in the way.
    #[test]
    fn tab_completes_the_arguments_that_come_from_a_list() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);

        typed(&mut app, ":perm auto");
        app.on_key(plain(KeyCode::Tab));
        assert_eq!(app.command_line.text(), "perm auto");
        app.on_key(plain(KeyCode::Tab));
        assert_eq!(app.command_line.text(), "perm auto-accept-edits");

        app.on_key(plain(KeyCode::Esc));
        typed(&mut app, ":mode p");
        app.on_key(plain(KeyCode::Tab));
        assert_eq!(app.command_line.text(), "mode plan");

        // `:rename` takes a title, which nothing here can guess at.
        app.on_key(plain(KeyCode::Esc));
        typed(&mut app, ":rename some");
        app.on_key(plain(KeyCode::Tab));
        assert_eq!(app.command_line.text(), "rename some");
    }

    /// `Ctrl-c` is how a great many people leave insert mode, and it used to reach past
    /// the message being written to the turn behind it: a key pressed to stop typing
    /// stopped the agent instead. In vim it is Esc under another name, so it is here.
    #[tokio::test]
    async fn ctrl_c_leaves_what_is_being_typed_rather_than_stopping_the_turn() {
        let (handle, mut requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.thread = Some(running_thread());
        app.mode = Mode::Insert;
        app.composer.set_text("half a message");

        app.on_key(ctrl('c'));
        assert_eq!(app.mode, Mode::Normal, "it leaves insert mode");
        assert_eq!(
            app.composer.text(),
            "half a message",
            "and keeps the message"
        );
        assert!(asked_nothing(&mut requests).await, "the turn is untouched");

        // From normal mode, where there is nothing to leave, it stops the turn.
        app.on_key(ctrl('c'));
        assert!(matches!(
            asked(&mut requests).await,
            Some(crate::session::Request::Dispatch { .. })
        ));
    }

    /// Everything else being typed leaves the same way, rather than the picker staying
    /// up while the turn behind it is interrupted.
    #[tokio::test]
    async fn ctrl_c_closes_whatever_is_in_front_of_the_conversation() {
        let (handle, mut requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.thread = Some(running_thread());

        app.mode = Mode::Command;
        app.command_line.set_text("delete!");
        app.on_key(ctrl('c'));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.command_line.is_empty(), "and takes the command with it");

        project(&mut app);
        app.open_picker(PickerKind::Project);
        assert_eq!(app.mode, Mode::Picker, "the picker is up to be closed");
        app.on_key(ctrl('c'));
        assert!(app.picker.is_none());
        assert_eq!(app.mode, Mode::Normal);

        assert!(asked_nothing(&mut requests).await, "the turn is untouched");
    }

    /// `Ctrl-q` quit the whole client on one keystroke, from any mode, undocumented,
    /// taking every unsent draft with it. In vim it is `Ctrl-v`: the next key as the
    /// character it stands for. `Enter` is the one worth having, since `Enter` sends.
    #[test]
    fn ctrl_q_writes_the_next_key_instead_of_quitting() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.thread = Some(running_thread());
        app.mode = Mode::Insert;
        app.composer.set_text("one");

        app.on_key(ctrl('q'));
        assert!(!app.quit, "it does not quit");
        assert!(app.literal_next, "the next key is spoken for");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.composer.text(),
            "one\n",
            "a newline, not a sent message"
        );
        assert!(!app.literal_next);

        // Ctrl-v is the same key under vim's own name, and a tab is a tab.
        app.on_key(ctrl('v'));
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.composer.text(), "one\n\t");

        // And nothing quits on a keystroke any more, in any mode.
        for mode in [Mode::Normal, Mode::Insert, Mode::Picker, Mode::Worktrees] {
            app.mode = mode;
            app.on_key(ctrl('q'));
            assert!(!app.quit, "{mode:?}");
        }
    }

    /// A paste is one event carrying the whole text, and every mode that takes typing
    /// has to take it. Dropping it looks to somebody using the client like the paste
    /// key is broken, because nothing on screen says where the text went.
    #[test]
    fn a_paste_lands_wherever_typing_would() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.thread = Some(running_thread());

        app.mode = Mode::Insert;
        app.on_paste("first\nsecond");
        assert_eq!(
            app.composer.text(),
            "first\nsecond",
            "the composer keeps lines"
        );

        // Normal mode still has the composer under the cursor, and stays normal mode.
        app.composer.clear();
        app.mode = Mode::Normal;
        app.on_paste("from normal");
        assert_eq!(app.composer.text(), "from normal");
        assert_eq!(app.mode, Mode::Normal);

        // The one-line fields take the whole paste, folded rather than cut short.
        app.mode = Mode::Command;
        app.command_line.clear();
        app.on_paste("model\nsonnet\n");
        assert_eq!(app.command_line.text(), "model sonnet");

        app.mode = Mode::QuestionCustom;
        app.on_paste("do it\nthis way\n");
        assert_eq!(app.custom_answer.text(), "do it this way");

        project(&mut app);
        app.open_picker(PickerKind::Project);
        app.on_paste("a query\n");
        assert_eq!(
            app.picker.as_ref().expect("the picker is up").query.text(),
            "a query"
        );

        app.mode = Mode::Normal;
        app.start_search(false);
        app.on_paste("needle\n");
        assert_eq!(
            app.search_input
                .as_ref()
                .expect("a search is being typed")
                .query
                .text(),
            "needle"
        );
    }

    /// The attached terminal gets a paste as a paste: the program on the other end can
    /// tell it from typing, and a shell holds a multi-line one back instead of running
    /// every line but the last.
    #[tokio::test]
    async fn a_paste_reaches_the_attached_terminal() {
        let (handle, mut requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.mode = Mode::TerminalPane;
        let mut pane = crate::term::Pane::new("t1".into(), "term".into(), "shell".into(), 80, 24);
        // The shell says it wants pastes bracketed, the way a shell with a line editor does.
        let _ = pane.feed("\x1b[?2004h");
        app.pane = Some(pane);

        app.on_paste("one\ntwo");
        let Some(crate::session::Request::Call { tag, payload, .. }) = asked(&mut requests).await
        else {
            panic!("the paste was not written to the terminal");
        };
        assert_eq!(tag, "terminal.write");
        assert_eq!(
            payload["data"].as_str(),
            Some("\x1b[200~one\rtwo\x1b[201~"),
            "between the markers, with a carriage return for the line"
        );
    }

    /// A thread sitting on an approval, in the shape the server sends one.
    /// A thread with a turn going, in the shape the server sends one.
    fn running_thread() -> ThreadState {
        let snapshot: ThreadDetailSnapshot = serde_json::from_value(serde_json::json!({
            "snapshotSequence": 1,
            "thread": {
                "id": "t1", "projectId": "p", "title": "Test",
                "modelSelection": {"instanceId": "instance", "model": "a-model"},
                "runtimeMode": "full-access", "session": {"status": "running"},
                "latestTurn": {"turnId": "turn", "state": "running"},
                "messages": [], "activities": []
            }
        }))
        .expect("a running turn the server could have sent");
        ThreadState::from_snapshot(snapshot)
    }

    /// A thread sitting on a question, in the shape the server sends one.
    fn awaiting_question() -> ThreadState {
        let snapshot: ThreadDetailSnapshot = serde_json::from_value(serde_json::json!({
            "snapshotSequence": 1,
            "thread": {
                "id": "t1", "projectId": "p", "title": "Test",
                "modelSelection": {"instanceId": "instance", "model": "a-model"},
                "runtimeMode": "full-access", "latestTurn": null, "session": null,
                "messages": [{"id": "m1", "role": "assistant", "text": "the answer"}],
                "activities": [{
                    "id": "a1", "kind": "user-input.requested",
                    "payload": {
                        "requestId": "r1",
                        "questions": [{
                            "id": "q1", "question": "Which one?",
                            "options": [{"label": "this one"}, {"label": "that one"}]
                        }]
                    }
                }]
            }
        }))
        .expect("a question the server could have sent");
        ThreadState::from_snapshot(snapshot)
    }

    /// The question panel is a panel over a thread, not a room with the door shut. The
    /// `g` motions that go and look at the thread still work while it is up, so an
    /// answer can be checked against what was said before it is given.
    #[test]
    fn the_question_panel_lets_the_g_motions_past() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.thread = Some(awaiting_question());
        app.begin_answering();
        assert_eq!(app.mode, Mode::Question);

        typed(&mut app, "g");
        assert_eq!(app.waiting_prefix(), Some('g'), "`g` waits for its motion");
        typed(&mut app, "y");
        assert_eq!(app.mode, Mode::Question, "yanking leaves the panel up");
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|(t, _, _)| t.contains("yanked")),
            "gy yanked the last assistant message"
        );

        // A motion that opens something else takes the panel down, the way it does from
        // normal mode, and `ga` brings it back.
        typed(&mut app, "gT");
        assert_eq!(app.mode, Mode::Tasks);

        // The panel's own letters are still its own: `y` on its own is not a yank.
        app.mode = Mode::Question;
        typed(&mut app, "j");
        assert_eq!(
            app.question.as_ref().expect("a draft").highlight,
            1,
            "j still moves the highlight"
        );
    }

    fn awaiting_approval() -> ThreadState {
        let snapshot: ThreadDetailSnapshot = serde_json::from_value(serde_json::json!({
            "snapshotSequence": 1,
            "thread": {
                "id": "t1", "projectId": "p", "title": "Test",
                "modelSelection": {"instanceId": "instance", "model": "a-model"},
                "runtimeMode": "approval-required", "latestTurn": null, "session": null,
                "messages": [],
                "activities": [{
                    "id": "a1", "kind": "approval.requested",
                    "payload": {"requestId": "r1", "requestKind": "bash"}
                }]
            }
        }))
        .expect("an approval the server could have sent");
        ThreadState::from_snapshot(snapshot)
    }

    fn shell_with(app: &mut App, projects: serde_json::Value) {
        app.on_update(crate::session::Update::Shell(
            crate::model::ShellItem::Snapshot {
                snapshot: serde_json::from_value(json!({
                    "snapshotSequence": 1,
                    "projects": projects,
                    "threads": [],
                }))
                .expect("a shell the server could send"),
            },
        ));
        app.on_update(crate::session::Update::Shell(
            crate::model::ShellItem::Synchronized,
        ));
    }

    fn project_json(id: &str, title: &str, root: &str) -> serde_json::Value {
        json!({"id": id, "title": title, "workspaceRoot": root})
    }

    /// `tria open` in a directory the server already has a project for goes straight to
    /// a new thread in it — and so does one run inside that project, since `src/` of a
    /// project is that project rather than a second one beside it.
    #[tokio::test]
    async fn opening_a_directory_starts_a_thread_in_its_project() {
        let (handle, mut requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.new_thread_model = Some(crate::model::ModelSelection {
            instance_id: "instance".into(),
            model: "a-model".into(),
            options: Vec::new(),
        });
        app.open_at = Some("/src/tria/src".into());

        shell_with(
            &mut app,
            json!([
                project_json("p1", "tria", "/src/tria"),
                project_json("p2", "other", "/src/other"),
                // A project further in wins over one it sits inside.
                project_json("p3", "inner", "/src/tria/src/inner"),
            ]),
        );

        let draft = app.draft.as_ref().expect("a new thread is waiting");
        assert_eq!(draft.project_id, "p1");
        assert_eq!(app.mode, Mode::Insert);
        assert!(app.open_at.is_none(), "the directory has been dealt with");
        assert!(
            sent_no_command(&mut requests).await,
            "a project that exists is not made again"
        );
    }

    /// A directory with no project of its own gets one — for the checkout it is in,
    /// named after it — and the thread waits for the server to send the project back.
    #[tokio::test]
    async fn opening_a_directory_that_is_not_a_project_adds_one() {
        let (handle, mut requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.new_thread_model = Some(crate::model::ModelSelection {
            instance_id: "instance".into(),
            model: "a-model".into(),
            options: Vec::new(),
        });
        // The repository tria itself is in, so the checkout is a real one.
        let here = std::env::current_dir().unwrap().display().to_string();
        app.open_at = Some(format!("{here}/src"));

        shell_with(&mut app, json!([project_json("p2", "other", "/src/other")]));
        assert!(app.draft.is_none(), "there is no project to draft in yet");

        let command = sent_command(&mut requests)
            .await
            .expect("it asks for the project");
        assert_eq!(command["type"], "project.create");
        assert_eq!(command["workspaceRoot"], here, "the checkout, not src/");
        assert_eq!(command["title"], "tria");

        // Asked for once, however many times the list changes in the meantime.
        shell_with(&mut app, json!([project_json("p2", "other", "/src/other")]));
        assert!(sent_no_command(&mut requests).await);

        // And when the server sends it back, that is the thread.
        app.on_update(crate::session::Update::Shell(
            crate::model::ShellItem::ProjectUpserted {
                sequence: 2,
                project: serde_json::from_value(project_json("p9", "tria", &here)).unwrap(),
            },
        ));
        assert_eq!(
            app.draft.as_ref().map(|d| d.project_id.as_str()),
            Some("p9")
        );
    }

    /// Sending empties the composer, which is what sending looks like — but a message
    /// the server would not take never went anywhere, and what was typed is the work.
    /// It comes back to where it was written.
    #[tokio::test]
    async fn a_message_the_server_refuses_comes_back() {
        let (handle, requests) = crate::session::Handle::detached();
        let (events, mut sent) = mpsc::unbounded_channel();
        // Nothing is listening for commands, so the send fails the way a send fails.
        drop(requests);
        let mut app = App::new(handle, events);
        app.thread = Some(running_thread());
        app.current_thread_id = Some("t1".into());
        app.mode = Mode::Insert;
        app.composer.set_text("the message");

        app.send_message();
        assert!(app.composer.is_empty(), "the message left on its way out");

        let refusal = tokio::time::timeout(Duration::from_millis(500), sent.recv())
            .await
            .expect("the refusal comes back")
            .expect("an event");
        assert!(
            matches!(&refusal, AppEvent::SendRefused { thread_id, text, .. }
                if thread_id == "t1" && text == "the message"),
            "the refusal carries the message it refused"
        );
        apply(&mut app, refusal);
        assert_eq!(app.composer.text(), "the message");
        assert_eq!(app.focus, Focus::Composer);
        let said = app.toast.as_ref().expect("it says why").0.clone();
        assert!(said.starts_with("not sent:"), "{said}");
    }

    /// Where the composer is not free, putting the message back would write over
    /// something else somebody typed. It goes to the thread it was meant for if that
    /// draft is free, and otherwise it stays in the history and the toast says so.
    #[test]
    fn a_refused_message_does_not_write_over_what_took_its_place() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.thread = Some(running_thread());
        app.current_thread_id = Some("t1".into());

        // Something written since: the composer is left alone.
        app.composer.set_text("the next one");
        app.on_send_refused("t1".into(), "the message".into(), "refused".into());
        assert_eq!(app.composer.text(), "the next one");
        assert!(app.toast.as_ref().unwrap().0.contains("history"));

        // Somewhere else entirely: it waits in the thread it was written in.
        app.current_thread_id = Some("t2".into());
        app.on_send_refused("t1".into(), "the message".into(), "refused".into());
        assert_eq!(
            app.drafts.get("t1").map(String::as_str),
            Some("the message")
        );
        assert_eq!(app.composer.text(), "the next one");
    }

    /// The app keeps a handful of letters the composer's Vim has no use for, but a key
    /// that is part of a command already begun is the composer's whatever the letter is:
    /// `3J` joins three lines rather than walking three threads down the list.
    #[test]
    fn a_half_typed_command_keeps_the_keys_the_app_would_take() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.composer.set_text("one\ntwo");
        let press = |app: &mut App, ch: char| {
            app.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
        };

        // Nothing half-typed: the letters are the app's.
        let sidebar = app.sidebar_visible;
        press(&mut app, 's');
        assert_ne!(app.sidebar_visible, sidebar, "s toggles the sidebar");
        press(&mut app, 's');

        // A count in front of them, and they are the composer's again.
        for key in ['s', 'S', 'J', 'K', 'n', 'm', '/', '?', 'g', 'z'] {
            press(&mut app, '2');
            assert!(app.composer.vim_busy());
            press(&mut app, key);
            assert_eq!(app.mode, Mode::Normal, "{key} left normal mode");
            assert!(app.picker.is_none(), "{key} opened a picker");
            assert!(
                app.waiting_prefix().is_none(),
                "{key} started an app prefix"
            );
            app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        }
        assert_eq!(app.sidebar_visible, sidebar, "and the sidebar stayed put");
    }

    /// A selection is a command half made, so the same rule holds while one is up — and
    /// leaving the composer lets it go rather than leaving it drawn behind you.
    #[test]
    fn a_selection_keeps_the_keys_too() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.thread = Some(awaiting_approval());
        app.composer.set_text("one two three");
        app.on_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert!(app.composer.vim_visual().is_some());

        // `s` substitutes the selection rather than toggling the sidebar.
        let sidebar = app.sidebar_visible;
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.sidebar_visible, sidebar);
        assert_eq!(app.mode, Mode::Insert, "and leaves it ready to type");
        assert_eq!(app.composer.text(), " two three");

        // A selection does not follow the focus out of the composer.
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        assert!(app.composer.vim_visual().is_none());
    }

    /// One of the answers to an approval grants a permission for the whole session, and
    /// an approval arrives whenever the agent reaches one — including in the middle of a
    /// message being written. A count typed at the composer must not answer it: `2w` is
    /// two words, not "allow this for the session".
    #[tokio::test]
    async fn a_count_typed_at_the_composer_does_not_answer_an_approval() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, mut sent) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.thread = Some(awaiting_approval());
        app.composer.set_text("half a message");

        app.on_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert!(
            sent.try_recv().is_err(),
            "a digit with something written answers nothing"
        );
        assert!(
            app.composer.vim_pending(),
            "it is the count it looks like instead"
        );

        // With nothing written there is no other reading of it, and it answers as before.
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.composer.clear();
        app.on_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|(m, _, _)| m == "Allow for session"),
            "{:?}",
            app.toast
        );

        // And `:approve` answers whatever the composer holds, which is what makes the
        // guard something other than a way of being unable to answer.
        app.composer.set_text("half a message");
        app.toast = None;
        app.run_command("approve 3");
        assert!(
            app.toast.as_ref().is_some_and(|(m, _, _)| m == "Deny"),
            "{:?}",
            app.toast
        );
    }

    /// The next thing the UI asked the server for. The calls go out from a task of
    /// their own, so there is a moment between the key and the request.
    async fn asked(
        requests: &mut mpsc::UnboundedReceiver<crate::session::Request>,
    ) -> Option<crate::session::Request> {
        tokio::time::timeout(std::time::Duration::from_millis(500), requests.recv())
            .await
            .ok()
            .flatten()
    }

    /// That nothing was asked for. Given the same moment to arrive as anything else,
    /// so that "nothing happened" is not just "nothing happened yet".
    async fn asked_nothing(
        requests: &mut mpsc::UnboundedReceiver<crate::session::Request>,
    ) -> bool {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        requests.try_recv().is_err()
    }

    /// The first command dispatched, past whatever subscribing and looking-up goes out
    /// beside it.
    async fn sent_command(
        requests: &mut mpsc::UnboundedReceiver<crate::session::Request>,
    ) -> Option<Value> {
        loop {
            match asked(requests).await? {
                crate::session::Request::Dispatch { command, .. } => return Some(command),
                _ => continue,
            }
        }
    }

    /// That no command was dispatched, whatever else was asked for.
    async fn sent_no_command(
        requests: &mut mpsc::UnboundedReceiver<crate::session::Request>,
    ) -> bool {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        while let Ok(request) = requests.try_recv() {
            if matches!(request, crate::session::Request::Dispatch { .. }) {
                return false;
            }
        }
        true
    }

    fn holding(app: &mut App, changes: Option<bool>) -> String {
        let worktree = ThreadWorktree {
            thread_id: "t1".into(),
            title: "a thread".into(),
            project: "p".into(),
            project_cwd: "/src/p".into(),
            path: "/worktrees/p/w-1".into(),
            branch: Some("tria/1".into()),
            settled: true,
            running: false,
            changes,
            files: Vec::new(),
        };
        let path = worktree.path.clone();
        app.worktrees = vec![worktree];
        app.worktree_selected = 0;
        app.mode = Mode::Worktrees;
        path
    }

    /// `X` is one shift away from `x` in a list moved through with `j` and `k`, and it
    /// is the only key here that destroys work: git refuses a worktree with anything
    /// uncommitted in it, untracked files are in no commit anywhere, and a reflexive
    /// `dd` used to take one of each. So it asks first.
    #[tokio::test]
    async fn forcing_a_dirty_worktree_out_asks_before_it_does_it() {
        let (handle, mut requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        holding(&mut app, Some(true));

        app.on_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE));
        assert!(app.worktree_confirm.is_some(), "it asks");
        // Nothing has been removed: the only call out is the re-read of the checkout,
        // so that what is listed is what is there now rather than what was cached.
        assert!(matches!(
            asked(&mut requests).await,
            Some(crate::session::Request::Call { tag, .. }) if tag == "vcs.refreshStatus"
        ));

        // And saying no leaves it exactly as it was.
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.worktree_confirm.is_none());
        assert_eq!(app.worktrees.len(), 1);
        assert_eq!(app.mode, Mode::Worktrees, "the list is still there");
        assert!(asked_nothing(&mut requests).await, "nothing was removed");

        // Saying yes is what removes it.
        app.on_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE));
        let _ = asked(&mut requests).await;
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.worktree_confirm.is_none());
        assert!(matches!(
            asked(&mut requests).await,
            Some(crate::session::Request::Call { tag, payload, .. })
                if tag == "vcs.removeWorktree" && payload["force"] == true
        ));
    }

    /// Nothing uncommitted means git would not have refused and there is nothing to
    /// lose, so there is no question worth asking.
    #[tokio::test]
    async fn forcing_a_clean_worktree_out_just_does_it() {
        let (handle, mut requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        holding(&mut app, Some(false));

        app.on_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE));
        assert!(app.worktree_confirm.is_none());
        assert!(matches!(
            asked(&mut requests).await,
            Some(crate::session::Request::Call { tag, .. }) if tag == "vcs.removeWorktree"
        ));
    }

    /// The reasons a worktree cannot go at all are given before the question. Agreeing
    /// to lose the files and only then being told no is a worse conversation.
    #[tokio::test]
    async fn a_worktree_that_cannot_go_says_so_instead_of_asking() {
        let (handle, mut requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        holding(&mut app, Some(true));
        app.worktrees[0].running = true;

        app.on_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE));
        assert!(app.worktree_confirm.is_none());
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|(m, _, _)| m.contains("still running")),
            "{:?}",
            app.toast
        );
        assert!(asked_nothing(&mut requests).await);
    }

    /// A thread whose updates have stopped looks exactly like one nobody is writing to,
    /// which is why the toast was not enough: six seconds later the screen said nothing
    /// was wrong and the conversation had quietly stopped being the conversation.
    #[test]
    fn a_thread_that_has_stopped_updating_says_so_until_it_starts_again() {
        let (handle, mut requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.current_thread_id = Some("t1".into());

        app.on_update(Update::ThreadStreamError {
            thread_id: "t1".into(),
            error: "the thread's stream ended".into(),
        });
        let said = app.trouble().expect("the line under the header");
        assert!(said.contains("stopped updating"), "{said}");
        assert!(said.contains("the thread's stream ended"), "{said}");

        // It is asked for again, on its own, for as long as the thread stays open.
        app.retry_lost_stream();
        assert!(requests.try_recv().is_err(), "not before the interval");
        app.lost_stream.as_mut().unwrap().retry_at = Instant::now();
        app.retry_lost_stream();
        assert!(
            matches!(requests.try_recv(), Ok(crate::session::Request::OpenThread(id)) if id == "t1"),
            "the stream is asked for again"
        );

        // And the thread speaking again is the whole of what "it is back" means.
        app.on_update(Update::Thread {
            thread_id: "t1".into(),
            item: crate::model::ThreadItem::Synchronized,
        });
        assert!(app.trouble().is_none());
    }

    /// A connection the server has settled into refusing used to be two words in the
    /// header — "auth failed" — with the reason thrown away and no way back. The reason
    /// is the only part of it anybody can act on.
    #[test]
    fn a_connection_that_will_not_come_back_says_why_and_how_to_try_again() {
        let (handle, mut requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.current_thread_id = Some("t1".into());
        // A thread that has stopped hearing anything, which is what a refused
        // connection looks like from the thread's end.
        app.on_update(Update::ThreadStreamError {
            thread_id: "t1".into(),
            error: "connection closed".into(),
        });

        app.on_update(Update::Status(crate::session::Status::Failed(
            "the server would not accept the stored token (401) · run `tria pair \
             <credential>` again, then `:reconnect`"
                .into(),
        )));
        let said = app.trouble().expect("the line under the header");
        assert!(said.contains("tria pair"), "{said}");
        assert!(
            !said.contains("stopped updating"),
            "the connection outranks the thread: {said}"
        );

        // And there is a way to ask again without quitting the whole client.
        app.run_command("reconnect");
        assert!(matches!(
            requests.try_recv(),
            Ok(crate::session::Request::Reconnect)
        ));
    }

    /// The trouble belongs to the thread it happened to. One left in the meantime is not
    /// the one on the screen, and the thread that is has nothing wrong with it.
    #[test]
    fn a_stream_that_stopped_on_a_thread_since_left_is_not_this_thread_s_trouble() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.current_thread_id = Some("t2".into());

        app.on_update(Update::ThreadStreamError {
            thread_id: "t1".into(),
            error: "gone".into(),
        });
        assert!(app.lost_stream.is_none());
        assert!(app.trouble().is_none());
    }

    /// The fold keys step a level at a time as well as going straight to the ends, and
    /// stepping past either end stays there rather than wrapping round to the other.
    #[test]
    fn the_fold_keys_step_a_level_at_a_time() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.focus = Focus::Chat;
        let press = |app: &mut App, key: char| {
            app.on_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
            app.on_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        };

        press(&mut app, 'r');
        assert_eq!(app.open_levels, 1, "the groups");
        press(&mut app, 'r');
        assert_eq!(app.open_levels, 2, "and what is inside the calls in them");
        press(&mut app, 'r');
        assert_eq!(app.open_levels, MOST_OPEN_LEVELS, "and no further");
        press(&mut app, 'm');
        assert_eq!(app.open_levels, 1);
        press(&mut app, 'm');
        assert_eq!(app.open_levels, 0);
        press(&mut app, 'm');
        assert_eq!(app.open_levels, 0, "and no further the other way");
        press(&mut app, 'R');
        assert_eq!(app.open_levels, MOST_OPEN_LEVELS);

        // Shutting everything also lets go of the folds opened one at a time, which is
        // what makes it the one key that leaves the conversation as it was read.
        app.expanded.insert("work-1".to_string());
        press(&mut app, 'M');
        assert_eq!(app.open_levels, 0);
        assert!(app.expanded.is_empty());
    }

    /// Leaving a thread drops the status of the checkout it was in, because the next
    /// thread's directory is not known until its snapshot arrives. The watch has to go
    /// with it: where the next thread — or a new one being drafted — sits in the same
    /// directory, a watch that is still on it sends nothing, a quiet checkout offers
    /// nothing unasked, and a draft that needs a branch to base a worktree on would be
    /// refused for as long as the repository stayed quiet.
    #[test]
    fn leaving_a_thread_asks_again_for_a_checkout_it_was_already_watching() {
        let (handle, mut requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        app.shell.projects.insert(
            "p".into(),
            Project {
                id: "p".into(),
                title: "p".into(),
                workspace_root: "/src/p".into(),
                project_icon: None,
                default_model_selection: None,
                default_thread_env_mode: Some("worktree".into()),
            },
        );
        app.new_thread_model = Some(selection());
        // Watching the project's own checkout, with its branch in hand.
        app.vcs_cwd = Some("/src/p".into());
        app.vcs = Some(VcsLocal {
            is_repo: true,
            ref_name: Some("main".into()),
            is_default_ref: true,
            has_working_tree_changes: false,
            working_tree: VcsWorkingTree::default(),
        });

        app.open_thread("t1");
        app.start_new_thread("p");

        let asked = std::iter::from_fn(|| requests.try_recv().ok()).any(
            |request| matches!(request, crate::session::Request::WatchVcs { cwd } if cwd.as_deref() == Some("/src/p")),
        );
        assert!(asked, "the draft's checkout was never asked for");
    }

    /// Renaming from the list of projects: the letters go into the name rather than the
    /// search that is usually under them, and what the server is asked is the new title.
    #[tokio::test]
    async fn a_project_is_renamed_by_typing_over_its_name() {
        let (handle, mut requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        project(&mut app);
        app.open_picker(PickerKind::Project);

        let press = |app: &mut App, code, ctrl| {
            app.on_picker_key(KeyEvent::new(
                code,
                if ctrl {
                    KeyModifiers::CONTROL
                } else {
                    KeyModifiers::NONE
                },
            ))
        };
        press(&mut app, KeyCode::Char('r'), true);
        // The name it already has is there to be edited, and the list stays put.
        let picker = app.picker.as_ref().expect("the list is still open");
        assert_eq!(picker.query.text(), "p");
        assert_eq!(picker.renaming.as_deref(), Some("p"));
        press(&mut app, KeyCode::Backspace, false);
        for letter in "shelf".chars() {
            press(&mut app, KeyCode::Char(letter), false);
        }
        assert_eq!(app.picker.as_ref().unwrap().filtered().len(), 1);
        press(&mut app, KeyCode::Enter, false);
        tokio::task::yield_now().await;

        let renamed = std::iter::from_fn(|| requests.try_recv().ok()).any(|request| {
            matches!(request, crate::session::Request::Dispatch { command, .. }
                if command["type"] == "project.meta.update"
                    && command["projectId"] == "p"
                    && command["title"] == "shelf")
        });
        assert!(renamed, "the server was never asked to rename it");
    }

    /// Escaping a rename leaves the project's name alone and the list where it was.
    #[test]
    fn a_rename_let_go_of_changes_nothing() {
        let (handle, _requests) = crate::session::Handle::detached();
        let (events, _events) = mpsc::unbounded_channel();
        let mut app = App::new(handle, events);
        project(&mut app);
        app.open_picker(PickerKind::Project);
        app.on_picker_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        app.on_picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let picker = app.picker.as_ref().expect("the list is still open");
        assert!(picker.renaming.is_none());
        assert!(picker.query.is_empty());
        assert_eq!(app.shell.projects["p"].title, "p");
    }

    use super::{split_args, tmux_session_name, window_args};

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

    /// The pane is a split of tria's own, named rather than left to whichever pane tmux
    /// last called the active one, and it starts where the thread works.
    #[test]
    fn the_pane_is_a_split_of_this_one_in_the_threads_directory() {
        assert_eq!(
            split_args("/home/me/work/app", Some("%3")),
            ["split-window", "-h", "-c", "/home/me/work/app", "-t", "%3"]
        );
        // Outside a pane tmux names, the split is of whichever one is active — which,
        // with tria on screen, is tria's.
        assert_eq!(
            split_args("/home/me/work/app", None),
            ["split-window", "-h", "-c", "/home/me/work/app"]
        );
    }

    /// The window is `prefix c` with a directory: no target, since the session is the
    /// one tria was started in, and no position, since that is the session's to decide.
    #[test]
    fn the_window_is_a_new_one_in_this_session() {
        assert_eq!(
            window_args("/home/me/work/app"),
            ["new-window", "-c", "/home/me/work/app"]
        );
    }
}

#[cfg(test)]
mod link_tests {
    use super::link_ranges;

    fn links(text: &str) -> Vec<&str> {
        link_ranges(text)
            .into_iter()
            .map(|(from, to)| &text[from..to])
            .collect()
    }

    #[test]
    fn a_bare_url_is_a_link() {
        assert_eq!(
            links("see https://example.com/a for more"),
            ["https://example.com/a"]
        );
        assert_eq!(links("no links here"), Vec::<&str>::new());
    }

    #[test]
    fn sentence_punctuation_is_not_part_of_it() {
        assert_eq!(links("at http://example.com/a."), ["http://example.com/a"]);
        assert_eq!(links("(https://example.com/a)"), ["https://example.com/a"]);
        // A bracket the URL opened itself stays.
        assert_eq!(
            links("https://example.com/a_(b)"),
            ["https://example.com/a_(b)"]
        );
    }

    #[test]
    fn several_on_one_line_are_found() {
        assert_eq!(
            links("https://a.example/x and https://b.example/y"),
            ["https://a.example/x", "https://b.example/y"]
        );
    }

    #[test]
    fn a_scheme_on_its_own_is_not_a_link() {
        assert_eq!(links("https://"), Vec::<&str>::new());
    }
}

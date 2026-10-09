//! The reader: something read in place of the thread's conversation, from being asked for
//! to being put away. It is a subagent's work, one of the thread's pull requests, or a
//! background task's own rows, and the conversation is kept underneath it and put
//! back on the way out. A subagent's
//! work is the thread the server keeps of it, followed live, or for one from before the
//! server kept those, its transcript file.
//!
//! It does no reading of its own. Asked to open something, or told that time has passed or
//! the thread has moved, it says what to fetch; told what came back, it says what changed
//! and where the reader is to be put. The app does the fetching and the moving. So which
//! read wins, which is dropped, and who is disturbed by one is all decided here, and can be
//! tried without a server.
//!
//! A read is loud or quiet. A loud read is one somebody asked for: its answer takes over
//! the chat, and its failure is said out loud. A quiet read is one nobody asked for, made
//! to bring the open reading up to date: it changes only what is still open, leaves the
//! reader's place alone, and fails silently.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use crate::{
    model::{PullRequestRef, ThreadDetail, ThreadDetailSnapshot, ThreadItem},
    pull_request::{self, Activity, Detail},
    state::ThreadState,
    subagent::Subagent,
    timeline::{self, Block},
};

/// How often a running agent's open transcript is read again when nothing it did said so.
pub const TRANSCRIPT_POLL: Duration = Duration::from_secs(5);

/// A transcript as the server read it off disk.
#[derive(Debug)]
pub struct TranscriptFile {
    pub contents: String,
    /// The server stops reading at a megabyte; the tail of a long run is then missing.
    pub truncated: bool,
}

/// What a reading is of, by what names it: a subagent by its id, a pull request by its
/// link. Never by where either sits in a list, which moves under whoever is reading.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Source {
    Subagent(String),
    PullRequest(String),
    /// A background task, by the id the server lists it under.
    Task(String),
}

/// Where a loud read was asked from, which decides where the keys go once it opens, and
/// where `q` goes back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The chat's own keys, a command, or a picker: the keys go to what is read.
    Chat,
    /// The sidebar's list, whose keys stay there so the next row is one keystroke away.
    Sidebar,
    /// The subagent roster, which `q` goes back to.
    Roster,
    /// The background task list, which `q` goes back to.
    Tasks,
}

/// The open thread, lent to the reader for one call. Everything the reader knows of the
/// thread it reads from here, so nothing it holds can go stale behind it.
pub struct Thread<'a> {
    pub state: &'a ThreadState,
    /// The branch checked out where the thread works, which says which of its pull
    /// requests the header speaks for.
    pub branch: Option<&'a str>,
    /// Where the thread works, which is where a transcript's path is read from and where
    /// `gh` is asked for a token.
    pub directory: Option<&'a str>,
}

/// Something for the app to fetch, and to hand back to the reader once it has come.
#[derive(Debug, Clone, PartialEq)]
pub enum Fetch {
    /// `projects.readFile` of a subagent's transcript, back through `on_transcript`. The
    /// path is the provider's own, outside any workspace, which is what the call takes an
    /// absolute path for.
    Transcript {
        agent_id: String,
        cwd: String,
        path: String,
    },
    /// Follow a subagent's thread, whose items come back through `on_child`. Following
    /// stops when the reader no longer wants it, which `following` says.
    Follow { thread_id: String },
    /// `orchestration.getTurnItem`, for the output of the call that started a task when
    /// the stream left it out, back through `on_turn_item`.
    TurnItem {
        task_id: String,
        thread_id: String,
        item_id: String,
        /// `live` for a call still going, else when it last changed.
        revision: String,
    },
    /// `pullRequests.detail` and `pullRequests.activity` together, back through
    /// `on_pull_request` and `on_activity`; `fresh` asks `pullRequests.invalidate` first,
    /// past the server's copy of what the host said.
    PullRequest {
        url: String,
        payload: Value,
        fresh: bool,
    },
    /// A picture a pull request shows, back through `on_image`. One on the pull request's
    /// own host is asked for with `gh`'s token for that host, asked in `directory`.
    Image {
        url: String,
        token_host: Option<String>,
        directory: Option<String>,
    },
}

/// Where the reader is put in what has just opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// At the top, as anything read for the first time is.
    Top,
    /// Following the end, where a run still going writes what is new.
    End,
    /// Wherever they were in it.
    Keep,
}

/// What an answer did to what is on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Outcome {
    #[default]
    Nothing,
    /// What is open was brought up to date under the reader, who stays where they were.
    Refreshed,
    /// A loud read opened, or opened again. Anywhere but `Place::Keep` it is read, not
    /// written to: the view goes back to normal mode with nothing selected or searched,
    /// and the keys go to it if `take_focus`.
    Opened { place: Place, take_focus: bool },
}

/// What to say about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Toast {
    /// Put away whatever was being said, which was about the read now answered.
    Clear,
    Say(String),
    Warn(String),
}

/// What the app is to do after a call: what to fetch, what to say, and what changed.
#[must_use]
#[derive(Debug, Default)]
pub struct Step {
    pub fetches: Vec<Fetch>,
    pub toast: Option<Toast>,
    pub outcome: Outcome,
}

impl Step {
    fn say(text: impl Into<String>) -> Self {
        Self {
            toast: Some(Toast::Say(text.into())),
            ..Self::default()
        }
    }

    fn warn(text: impl Into<String>) -> Self {
        Self {
            toast: Some(Toast::Warn(text.into())),
            ..Self::default()
        }
    }

    fn refreshed() -> Self {
        Self {
            outcome: Outcome::Refreshed,
            ..Self::default()
        }
    }

    /// This, and then that: the fetches of both, and whatever the later says and does.
    fn then(mut self, next: Step) -> Self {
        self.fetches.extend(next.fetches);
        self.toast = next.toast.or(self.toast);
        if next.outcome != Outcome::Nothing {
            self.outcome = next.outcome;
        }
        self
    }
}

/// The header over an open reading. What it says is the reader's; how it looks is the
/// screen's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub title: String,
    pub subtitle: String,
    pub badges: Vec<Badge>,
    /// The keys worth knowing here.
    pub hint: &'static str,
}

/// Something about how complete a reading is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    /// The server stopped reading the transcript at its limit.
    Truncated,
    /// The agent was still working when this was read, so what is shown is as far as it
    /// had got rather than the whole run.
    Working,
}

impl Badge {
    pub fn text(self) -> &'static str {
        match self {
            Badge::Truncated => "cut at 1 MB",
            Badge::Working => "still working",
        }
    }
}

/// A reading put away: where the conversation was, to put it back, and where the reading
/// was asked from, to go back there.
pub struct Closed<R> {
    pub restore: R,
    pub origin: Origin,
}

/// Whether a read was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    Quiet,
    Loud,
}

/// A read on its way.
#[derive(Debug, Clone, Copy)]
struct Asked {
    intent: Intent,
    origin: Origin,
    /// Asked for again while this was on its way, and whether past the server's copy. What
    /// comes back was read before that, so it is read once more.
    again: Option<bool>,
}

/// The reading on the screen.
struct Open<R> {
    kind: Kind,
    title: String,
    subtitle: String,
    /// What was read, as a thread, so the chat draws it like any other.
    state: ThreadState,
    /// Where the conversation underneath was, to put it back on the way out.
    restore: R,
    /// Where it was last asked for from.
    origin: Origin,
}

enum Kind {
    Transcript {
        agent_id: String,
        truncated: bool,
        live: bool,
        /// When it was last read, and the agent as it stood then, to tell when it is
        /// worth reading again.
        read_at: Instant,
        seen: String,
    },
    /// A subagent's thread, kept up to date by its stream.
    Child {
        agent_id: String,
        thread_id: String,
        live: bool,
    },
    /// A background task's rows, cut out of the open thread and cut again as it moves.
    Task {
        task_id: String,
        live: bool,
        /// The rows as last cut, to tell when they have moved.
        seen: u64,
        /// The output the stream left out, as last fetched, and when.
        output: Option<String>,
        read_at: Instant,
    },
    PullRequest {
        detail: Box<Detail>,
        /// Its link as the thread list last had it, to notice the server's sync bringing
        /// news of it.
        synced: Option<String>,
        /// The stack it is in, as last drawn.
        stack_seen: String,
    },
}

/// What is read in place of the conversation, the reads on their way to it, and what has
/// been fetched for the pull requests read. `R` is where the conversation was, which the
/// reader keeps without looking at and gives back on `close`.
pub struct Reader<R> {
    open: Option<Open<R>>,
    /// The reads on their way, by what they are of. At most one of them is loud: the last
    /// one asked for, which is the only one that may take over the view.
    asked: HashMap<Source, Asked>,
    /// How many times something has been read in place of the conversation, or redrawn
    /// there, which is the revision each read gets. The chat rebuilds what it draws only
    /// when the revision moves, and a re-read keeps the identity of the one before it.
    revision: u64,
    /// Each pull request's review history as last read, by its link, or why it could not be.
    activity: HashMap<String, Result<Activity, String>>,
    /// The pictures pull requests show, by their links: base64, or `None` where the fetch
    /// failed. Kept for the session, so a re-read does not fetch again.
    images: HashMap<String, Option<String>>,
    /// Pictures asked for, whether or not they have come back.
    images_asked: HashSet<String>,
    /// The server's refresh count as last heard, to tell a change from where it stands.
    server_refreshes: Option<u64>,
    /// The subagent thread asked to be followed and not yet heard from: its agent and
    /// its thread.
    child_asked: Option<(String, String)>,
}

impl<R> Default for Reader<R> {
    fn default() -> Self {
        Self {
            open: None,
            asked: HashMap::new(),
            revision: 0,
            activity: HashMap::new(),
            images: HashMap::new(),
            images_asked: HashSet::new(),
            server_refreshes: None,
            child_asked: None,
        }
    }
}

impl<R> Reader<R> {
    // ── What is open ───────────────────────────────────────────────────

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// What the open reading is of.
    pub fn source(&self) -> Option<Source> {
        Some(match &self.open.as_ref()?.kind {
            Kind::Transcript { agent_id, .. } | Kind::Child { agent_id, .. } => {
                Source::Subagent(agent_id.clone())
            }
            Kind::PullRequest { detail, .. } => Source::PullRequest(detail.url.clone()),
            Kind::Task { task_id, .. } => Source::Task(task_id.clone()),
        })
    }

    /// What was read, as a thread, which is the conversation on screen while it is open.
    pub fn conversation(&self) -> Option<&ThreadState> {
        self.open.as_ref().map(|open| &open.state)
    }

    /// The subagent being read, when it is one.
    pub fn agent_id(&self) -> Option<&str> {
        match &self.open.as_ref()?.kind {
            Kind::Transcript { agent_id, .. } | Kind::Child { agent_id, .. } => Some(agent_id),
            Kind::PullRequest { .. } | Kind::Task { .. } => None,
        }
    }

    /// The pull request being read, when it is one.
    pub fn pull_request(&self) -> Option<&Detail> {
        match &self.open.as_ref()?.kind {
            Kind::PullRequest { detail, .. } => Some(detail),
            Kind::Transcript { .. } | Kind::Child { .. } | Kind::Task { .. } => None,
        }
    }

    /// The background task being read, when it is one.
    pub fn task_id(&self) -> Option<&str> {
        match &self.open.as_ref()?.kind {
            Kind::Task { task_id, .. } => Some(task_id),
            _ => None,
        }
    }

    /// The subagent thread to follow: the one asked for, or else the one open.
    pub fn following(&self) -> Option<&str> {
        if let Some((_, thread_id)) = &self.child_asked {
            return Some(thread_id);
        }
        match &self.open.as_ref()?.kind {
            Kind::Child { thread_id, .. } => Some(thread_id),
            _ => None,
        }
    }

    /// Whether a read of this is on its way, so its row can say so.
    pub fn is_loading(&self, source: &Source) -> bool {
        self.asked.contains_key(source)
    }

    /// The revision of what is open, which moves each time it changes.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// What a message written over it says first, so the agent knows what "this" is.
    pub fn context(&self) -> Option<String> {
        Some(match &self.open.as_ref()?.kind {
            Kind::Transcript { agent_id, .. } | Kind::Child { agent_id, .. } => {
                format!("Sent looking at the transcript for subagent {agent_id}:")
            }
            Kind::PullRequest { detail, .. } => format!(
                "Sent looking at pull request {}#{} ({}):",
                detail.repository, detail.number, detail.url
            ),
            Kind::Task { task_id, .. } => format!("Sent looking at background task {task_id}:"),
        })
    }

    /// What it is, in a sentence about it.
    pub fn noun(&self) -> Option<&'static str> {
        Some(match self.open.as_ref()?.kind {
            Kind::Transcript { .. } | Kind::Child { .. } => "the subagent",
            Kind::PullRequest { .. } => "the pull request",
            Kind::Task { .. } => "the task",
        })
    }

    /// The header naming what is being read, since the chat below it is no longer the
    /// thread's own conversation.
    pub fn header(&self) -> Option<Header> {
        let open = self.open.as_ref()?;
        let mut badges = Vec::new();
        let hint = match &open.kind {
            Kind::Transcript {
                truncated, live, ..
            } => {
                if *truncated {
                    badges.push(Badge::Truncated);
                }
                // A run still going has written more since this was read, and nothing
                // tells us.
                if *live {
                    badges.push(Badge::Working);
                    "r re-reads · q back"
                } else {
                    "q back"
                }
            }
            // Cut again from the thread as it moves; only output the stream left out is
            // read again, which `r` asks for at once.
            Kind::Task { live, .. } => {
                if *live {
                    badges.push(Badge::Working);
                }
                "r re-reads · q back"
            }
            // Its stream keeps it up to date, so there is nothing to read again.
            Kind::Child { live, .. } => {
                if *live {
                    badges.push(Badge::Working);
                }
                "q back"
            }
            Kind::PullRequest { detail, .. } if detail.labels_editable() => {
                "r re-reads · gx opens · L labels · q back"
            }
            Kind::PullRequest { .. } => "r re-reads · gx opens · q back",
        };
        Some(Header {
            title: open.title.clone(),
            subtitle: open.subtitle.clone(),
            badges,
            hint,
        })
    }

    /// What the chat draws for the open reading. A pull request is drawn by its own module,
    /// already styled, with the stack it is in across the top, which is the thread's
    /// `under` it; a transcript is drawn as any conversation is.
    pub fn blocks(
        &self,
        under: Option<&ThreadState>,
        expanded: &HashSet<String>,
        open_levels: u8,
        size: (u16, u16),
        now: &str,
    ) -> Vec<Block> {
        let Some(open) = &self.open else {
            return Vec::new();
        };
        match &open.kind {
            Kind::PullRequest { detail, .. } => {
                let stack = under.and_then(|thread| stack_of(thread, &detail.url));
                pull_request::blocks(
                    detail,
                    &pull_request::Context {
                        activity: self.activity.get(&detail.url),
                        stack: stack.as_ref().map(|(layers, at)| (layers.as_slice(), *at)),
                        expanded,
                        open_levels,
                        size,
                        images: &self.images,
                        now,
                    },
                )
            }
            Kind::Transcript { .. } | Kind::Child { .. } => {
                timeline::build(&open.state, expanded, open_levels, size.0, size.1)
            }
            // A task is opened to read what it said, so its rows are drawn open.
            Kind::Task { .. } => timeline::build(&open.state, expanded, 2, size.0, size.1),
        }
    }

    /// The link a row of the open pull request stands for, kept off the row to leave
    /// room: a check's page, or the review a reviewer's row shows.
    pub fn link_at(&self, key: &str) -> Option<String> {
        let detail = self.pull_request()?;
        let activity = self.activity.get(&detail.url).and_then(|a| a.as_ref().ok());
        pull_request::link_at(key, detail, activity)
    }

    // ── Asking ─────────────────────────────────────────────────────────

    /// Read something in place of the conversation, because somebody asked.
    pub fn open(&mut self, thread: &Thread, source: Source, origin: Origin) -> Step {
        self.request(thread, source, Intent::Loud, origin, false)
    }

    /// Read it again, past anything kept of it: the file grows while the agent works, and
    /// a pull request moves on while it is read, and nothing pushes either to us.
    pub fn reread(&mut self, thread: &Thread, source: Source, origin: Origin) -> Step {
        self.request(thread, source, Intent::Loud, origin, true)
    }

    /// `[` and `]`: read the layer below or above in the stack.
    pub fn move_through_stack(&mut self, thread: &Thread, by: isize) -> Step {
        let Some((layers, at)) = self
            .pull_request()
            .and_then(|detail| stack_of(thread.state, &detail.url))
        else {
            return Step::say("this pull request is not in a stack");
        };
        match at.checked_add_signed(by).and_then(|next| layers.get(next)) {
            Some(layer) => self.open(thread, Source::PullRequest(layer.url.clone()), Origin::Chat),
            None if by < 0 => Step::say("already the bottom of the stack"),
            None => Step::say("already the top of the stack"),
        }
    }

    /// Keep a running agent's transcript up to date while it is open. The server has no
    /// way to say a file changed, but the thread says when the agent does something, and
    /// that is when its transcript grows; between those, a stretch of writing is caught by
    /// reading again every few seconds. Once the agent has finished, one last read shows
    /// how it ended, and that is the end of it.
    pub fn tick(&mut self, thread: &Thread, now: Instant) -> Step {
        if self.task_id().is_some() {
            return self.follow_task(thread, now);
        }
        let Some(Open {
            kind:
                Kind::Transcript {
                    agent_id,
                    live: true,
                    read_at,
                    seen,
                    ..
                },
            ..
        }) = &mut self.open
        else {
            return Step::default();
        };
        let source = Source::Subagent(agent_id.clone());
        if self.asked.contains_key(&source) {
            return Step::default();
        }
        let agents = thread.state.subagents();
        let Some(agent) = agents.iter().find(|agent| &agent.id == agent_id) else {
            return Step::default();
        };
        let now_seen = seen_as(agent);
        let moved = *seen != now_seen;
        let due = now.saturating_duration_since(*read_at) >= TRANSCRIPT_POLL;
        if !moved && !due {
            return Step::default();
        }
        *seen = now_seen;
        self.request(thread, source, Intent::Quiet, Origin::Chat, false)
    }

    /// The server syncs linked pull requests with the host on its own, and the thread list
    /// carries what it found. News of the one being read — checks finishing, a review, a
    /// merge — is a reason to read it again.
    pub fn notice_sync(&mut self, thread: &Thread) -> Step {
        let Some(Open {
            kind:
                Kind::PullRequest {
                    detail,
                    synced,
                    stack_seen,
                },
            state,
            ..
        }) = &mut self.open
        else {
            return Step::default();
        };
        let mut step = Step::default();
        // The strip across the top is the thread list's, so a layer landing or linked
        // redraws it, without reading anything.
        let stack = format!("{:?}", stack_of(thread.state, &detail.url));
        if *stack_seen != stack {
            *stack_seen = stack;
            self.revision += 1;
            state.revision = self.revision;
            step.outcome = Outcome::Refreshed;
        }
        let now_synced = synced_link(thread.state, &detail.url);
        if now_synced.is_some() && now_synced != *synced {
            let had = std::mem::replace(synced, now_synced);
            if had.is_some() {
                step = step.then(self.refresh(thread));
            }
        }
        step
    }

    /// The server says pull requests may have changed: a turn has ended, and the agent may
    /// have pushed, commented, or merged. Whether that is news, which is a reason to
    /// `refresh`: the first count after connecting is only where it stands, unless it moved
    /// while the connection was down. It is counted with or without a thread open.
    pub fn server_refreshed(&mut self, revision: u64) -> bool {
        let changed = self.server_refreshes.is_some_and(|seen| seen != revision);
        self.server_refreshes = Some(revision);
        changed
    }

    /// A label went on the pull request with this link or came off it. The chips across
    /// the top show it at once, and the pull request is read again for how the host
    /// shows it.
    pub fn label_set(&mut self, thread: &Thread, url: &str, name: &str, applied: bool) -> Step {
        let Some(Open {
            kind: Kind::PullRequest { detail, .. },
            state,
            ..
        }) = &mut self.open
        else {
            return Step::default();
        };
        if detail.url != url {
            return Step::default();
        }
        detail.labels.retain(|label| label.name != name);
        if applied {
            detail.labels.push(pull_request::Label {
                name: name.to_string(),
                color: None,
            });
        }
        self.revision += 1;
        state.revision = self.revision;
        Step::refreshed().then(self.refresh(thread))
    }

    /// Bring the open pull request up to date, past the server's copy of it, without a word.
    pub fn refresh(&mut self, thread: &Thread) -> Step {
        match self.pull_request().map(|detail| detail.url.clone()) {
            Some(url) => self.request(
                thread,
                Source::PullRequest(url),
                Intent::Quiet,
                Origin::Chat,
                true,
            ),
            None => Step::default(),
        }
    }

    /// Put a read of `source` on its way, unless one already is. Only a loud read says why
    /// it could not be made.
    fn request(
        &mut self,
        thread: &Thread,
        source: Source,
        intent: Intent,
        origin: Origin,
        fresh: bool,
    ) -> Step {
        let loud = intent == Intent::Loud;
        let failed = |step: Step| if loud { step } else { Step::default() };
        match source {
            Source::Subagent(agent_id) => {
                let agents = thread.state.subagents();
                let Some(agent) = agents.iter().find(|agent| agent.id == agent_id) else {
                    return failed(Step::warn("that subagent is no longer in this thread"));
                };
                if let Some(thread_id) = &agent.child_thread_id {
                    return self.follow(agent_id, thread_id.clone(), intent, origin);
                }
                let Some(path) = transcript_path(thread.state, agent) else {
                    return failed(Step::say("no transcript for this subagent yet"));
                };
                let source = Source::Subagent(agent_id.clone());
                let Some(cwd) = thread.directory else {
                    return failed(Step::warn("thread has no directory"));
                };
                // A pull request asked for before this no longer opens, so its "reading
                // #N…" is about nothing.
                let toast = (loud
                    && self.asked.iter().any(|(other, asked)| {
                        asked.intent == Intent::Loud && matches!(other, Source::PullRequest(_))
                    }))
                .then_some(Toast::Clear);
                if self.in_flight(&source, intent, origin, false) {
                    return Step {
                        toast,
                        ..Step::default()
                    };
                }
                self.ask(
                    source,
                    Asked {
                        intent,
                        origin,
                        again: None,
                    },
                );
                Step {
                    fetches: vec![Fetch::Transcript {
                        agent_id,
                        cwd: cwd.to_string(),
                        path,
                    }],
                    toast,
                    ..Step::default()
                }
            }
            // Opened by `open_task`, which has where the conversation is; asked for here,
            // it is the open one read again.
            Source::Task(task_id) => {
                if self.task_id() != Some(task_id.as_str()) {
                    return failed(Step::warn("that task is not open"));
                }
                self.show_task(thread, task_id, origin, None, intent, fresh)
            }
            Source::PullRequest(url) => {
                let (payload, number) = match pull_request_payload(thread, &url) {
                    Ok(found) => found,
                    Err(why) => return failed(Step::warn(why)),
                };
                let source = Source::PullRequest(url.clone());
                let toast = loud.then(|| Toast::Say(format!("reading #{number}…")));
                if self.in_flight(&source, intent, origin, fresh) {
                    return Step {
                        toast,
                        ..Step::default()
                    };
                }
                self.ask(
                    source,
                    Asked {
                        intent,
                        origin,
                        again: None,
                    },
                );
                Step {
                    fetches: vec![Fetch::PullRequest {
                        url,
                        payload,
                        fresh,
                    }],
                    toast,
                    ..Step::default()
                }
            }
        }
    }

    /// Follow a subagent's thread. One already open is live, so it is only put in view
    /// again; asking for another lets go of whatever was being followed.
    fn follow(
        &mut self,
        agent_id: String,
        thread_id: String,
        intent: Intent,
        origin: Origin,
    ) -> Step {
        let open_on_it = matches!(
            &self.open,
            Some(Open { kind: Kind::Child { thread_id: open, .. }, .. }) if *open == thread_id
        );
        if open_on_it {
            self.child_asked = None;
            if let Some(open) = self.open.as_mut() {
                open.origin = origin;
            }
            return match intent {
                Intent::Quiet => Step::default(),
                Intent::Loud => Step {
                    toast: Some(Toast::Clear),
                    outcome: Outcome::Opened {
                        place: Place::Keep,
                        take_focus: origin != Origin::Sidebar,
                    },
                    ..Step::default()
                },
            };
        }
        let source = Source::Subagent(agent_id.clone());
        if self.in_flight(&source, intent, origin, false) {
            return Step::default();
        }
        self.ask(
            source,
            Asked {
                intent,
                origin,
                again: None,
            },
        );
        self.child_asked = Some((agent_id, thread_id.clone()));
        Step {
            fetches: vec![Fetch::Follow { thread_id }],
            ..Step::default()
        }
    }

    // ── Reads on their way ─────────────────────────────────────────────

    /// Whether a read of this is already on its way, so it is not asked for twice. One
    /// asked for out loud while a quiet one is on its way makes that one loud, so what it
    /// brings back opens as though it were asked for, which it now was. Either way what
    /// comes back was read before this was asked, so it is marked to be read once more.
    fn in_flight(&mut self, source: &Source, intent: Intent, origin: Origin, fresh: bool) -> bool {
        let Some(asked) = self.asked.get_mut(source) else {
            return false;
        };
        let again = Some(asked.again.unwrap_or(false) || fresh);
        if intent == Intent::Loud {
            self.ask(
                source.clone(),
                Asked {
                    intent,
                    origin,
                    again,
                },
            );
        } else {
            asked.again = again;
        }
        true
    }

    /// Put a read on its way. A loud one is the last asked for, so any other loud read on
    /// its way is now only bringing up to date what is open, should it still be open.
    fn ask(&mut self, source: Source, asked: Asked) {
        if asked.intent == Intent::Loud {
            self.quiet_the_rest(&source);
        }
        self.asked.insert(source, asked);
    }

    /// `source` is the last thing asked for, so nothing else on its way is to take over
    /// the view when it comes.
    fn quiet_the_rest(&mut self, source: &Source) {
        for other in self.asked.values_mut() {
            other.intent = Intent::Quiet;
        }
        // A subagent thread is only followed to be read, and is not going to be.
        if let Some((agent_id, _)) = &self.child_asked
            && Source::Subagent(agent_id.clone()) != *source
        {
            self.asked.remove(&Source::Subagent(agent_id.clone()));
            self.child_asked = None;
        }
    }

    /// The read this answers, if it is still wanted.
    fn answered(&mut self, source: &Source) -> Option<Asked> {
        self.asked.remove(source)
    }

    // ── Answers ────────────────────────────────────────────────────────

    /// A subagent's transcript came back from the machine that ran it. `here` is where the
    /// conversation is, kept for the way back should this open over it.
    pub fn on_transcript(
        &mut self,
        thread: &Thread,
        agent_id: String,
        result: Result<TranscriptFile, String>,
        here: R,
        now: Instant,
    ) -> Step {
        let source = Source::Subagent(agent_id.clone());
        let Some(asked) = self.answered(&source) else {
            return Step::default();
        };
        let quiet = asked.intent == Intent::Quiet;
        // A read nobody asked for only brings up to date what is still open.
        let reread = self.agent_id() == Some(agent_id.as_str());
        if quiet && !reread {
            return Step::default();
        }
        if let Some(Open {
            kind: Kind::Transcript { read_at, .. },
            ..
        }) = self.open.as_mut().filter(|_| reread)
        {
            *read_at = now;
        }
        let file = match result {
            Ok(file) => file,
            Err(error) if quiet => {
                tracing::info!(%error, "bringing the transcript up to date");
                return Step::default();
            }
            Err(error) => return Step::warn(format!("reading the transcript: {error}")),
        };
        let agents = thread.state.subagents();
        let Some(agent) = agents.iter().find(|agent| agent.id == agent_id) else {
            return Step::default();
        };
        let (mut state, summary) =
            match crate::transcript::parse(&file.contents, &agent.id, &agent.title) {
                Ok(parsed) => parsed,
                Err(error) if quiet => {
                    tracing::info!(%error, "bringing the transcript up to date");
                    return Step::default();
                }
                Err(error) => return Step::warn(format!("{error}")),
            };
        self.revision += 1;
        state.revision = self.revision;
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
        let live = !agent.status.is_terminal();
        // Re-reading a running agent replaces what is open, so the way back is the one
        // taken on the way in, not wherever the reading had got to; and whatever else is
        // open already holds the conversation's own place.
        let (restore, before, seen) = match self.open.take() {
            Some(open) => {
                let seen = match open.kind {
                    Kind::Transcript { seen, .. } if reread => seen,
                    _ => seen_as(agent),
                };
                (open.restore, Some(open.origin), seen)
            }
            None => (here, None, seen_as(agent)),
        };
        // Read again from the view itself, it is still from wherever it was first opened.
        let origin = match before {
            Some(before) if reread && asked.origin == Origin::Chat => before,
            _ => asked.origin,
        };
        self.open = Some(Open {
            kind: Kind::Transcript {
                agent_id,
                truncated: file.truncated,
                live,
                read_at: now,
                seen,
            },
            title: agent.title.clone(),
            subtitle,
            state,
            restore,
            origin,
        });
        // Brought up to date under somebody, it leaves them where they were: reading
        // further up stays put, and reading at the end follows what is new.
        if quiet {
            return Step::refreshed();
        }
        // The transcript is read, not written to, so the cursor goes where the reading is
        // done and the conversation's own place is kept for the way back. A re-read of a
        // run still going lands at the end, which is the part that is new. One asked for
        // from the sidebar leaves the keys there, for the next.
        Step {
            outcome: Outcome::Opened {
                place: if reread && live {
                    Place::End
                } else {
                    Place::Top
                },
                take_focus: asked.origin != Origin::Sidebar,
            },
            ..Step::default()
        }
    }

    /// An item of a followed subagent thread. The first, its snapshot, opens it if it was
    /// asked for; the rest bring it up to date under the reader. `here` is where the
    /// conversation is, kept for the way back should this open over it.
    pub fn on_child(
        &mut self,
        thread: &Thread,
        thread_id: &str,
        item: ThreadItem,
        here: R,
    ) -> Step {
        let asked_for = self
            .child_asked
            .as_ref()
            .filter(|(_, asked)| asked == thread_id)
            .map(|(agent_id, _)| agent_id.clone());
        let agents = thread.state.subagents();
        if let Some(agent_id) = asked_for
            && let ThreadItem::Snapshot { snapshot } = item
        {
            self.child_asked = None;
            let Some(asked) = self.answered(&Source::Subagent(agent_id.clone())) else {
                return Step::default();
            };
            let Some(agent) = agents.iter().find(|agent| agent.id == agent_id) else {
                return Step::default();
            };
            let mut state = ThreadState::from_snapshot(snapshot);
            self.revision += 1;
            state.revision = self.revision;
            let (restore, before) = match self.open.take() {
                Some(open) => (open.restore, Some(open.origin)),
                None => (here, None),
            };
            let quiet = asked.intent == Intent::Quiet;
            self.open = Some(Open {
                kind: Kind::Child {
                    agent_id,
                    thread_id: thread_id.to_string(),
                    live: state.is_running() || !agent.status.is_terminal(),
                },
                title: agent.title.clone(),
                subtitle: child_subtitle(agent),
                state,
                restore,
                origin: before.filter(|_| quiet).unwrap_or(asked.origin),
            });
            if quiet {
                return Step::refreshed();
            }
            return Step {
                toast: Some(Toast::Clear),
                outcome: Outcome::Opened {
                    place: Place::Top,
                    take_focus: asked.origin != Origin::Sidebar,
                },
                ..Step::default()
            };
        }
        let Some(Open {
            kind:
                Kind::Child {
                    agent_id,
                    thread_id: open,
                    live,
                },
            state,
            ..
        }) = &mut self.open
        else {
            return Step::default();
        };
        if open != thread_id {
            return Step::default();
        }
        state.apply(item);
        *live = state.is_running()
            || agents
                .iter()
                .find(|agent| &agent.id == agent_id)
                .is_some_and(|agent| !agent.status.is_terminal());
        self.revision += 1;
        state.revision = self.revision;
        Step::refreshed()
    }

    /// Read a background task in place of the conversation: the call that started it and
    /// what it has said since, cut out of the thread. `here` is where the conversation is,
    /// kept for the way back.
    ///
    /// A watch the server keeps of a pull request has nothing in the thread to read: what
    /// it waits on is the pull request, which is read instead.
    pub fn open_task(&mut self, thread: &Thread, task_id: &str, origin: Origin, here: R) -> Step {
        let watched = thread
            .state
            .detail
            .shell
            .pull_requests
            .iter()
            .find(|pr| pr.watch_task_id().as_deref() == Some(task_id));
        if let Some(pr) = watched {
            return self.open(thread, Source::PullRequest(pr.url.clone()), origin);
        }
        self.show_task(
            thread,
            task_id.to_string(),
            origin,
            Some(here),
            Intent::Loud,
            false,
        )
    }

    fn show_task(
        &mut self,
        thread: &Thread,
        task_id: String,
        origin: Origin,
        here: Option<R>,
        intent: Intent,
        fresh: bool,
    ) -> Step {
        let loud = intent == Intent::Loud;
        let again = self.task_id() == Some(task_id.as_str());
        let output = match &self.open {
            Some(Open {
                kind: Kind::Task { output, .. },
                ..
            }) if again => output.clone(),
            _ => None,
        };
        let Some(cut) = cut_task(thread.state, &task_id, output.as_deref()) else {
            return if loud {
                Step::warn("that task is no longer in this thread")
            } else {
                Step::default()
            };
        };
        if loud {
            self.quiet_the_rest(&Source::Task(task_id.clone()));
        }
        let (restore, origin) = match (self.open.take(), here) {
            // Read again from the view itself, it is still from wherever it was opened.
            (Some(open), _) if again && origin == Origin::Chat => (open.restore, open.origin),
            (Some(open), _) => (open.restore, origin),
            (None, Some(here)) => (here, origin),
            (None, None) => return Step::default(),
        };
        self.revision += 1;
        let mut state = cut.state;
        state.revision = self.revision;
        self.open = Some(Open {
            kind: Kind::Task {
                task_id: task_id.clone(),
                live: cut.live,
                seen: cut.seen,
                output,
                read_at: Instant::now(),
            },
            title: cut.title,
            subtitle: cut.subtitle,
            state,
            restore,
            origin,
        });
        let fetches = match cut.omitted {
            // What was fetched before is shown until `r` asks past it.
            Some(omitted) if fresh || !again => self.fetch_output(thread, &task_id, omitted),
            _ => Vec::new(),
        };
        if !loud {
            return Step {
                fetches,
                ..Step::refreshed()
            };
        }
        Step {
            fetches,
            toast: Some(Toast::Clear),
            outcome: Outcome::Opened {
                place: if again { Place::Keep } else { Place::Top },
                take_focus: origin != Origin::Sidebar,
            },
        }
    }

    /// Keep an open task up to date: cut its rows again whenever the thread has moved
    /// them, and while it runs, read again every few seconds what the stream leaves out.
    fn follow_task(&mut self, thread: &Thread, now: Instant) -> Step {
        let Some(Open {
            kind:
                Kind::Task {
                    task_id,
                    live,
                    seen,
                    output,
                    read_at,
                },
            state,
            subtitle,
            ..
        }) = &mut self.open
        else {
            return Step::default();
        };
        let Some(cut) = cut_task(thread.state, task_id, output.as_deref()) else {
            return Step::default();
        };
        let task_id = task_id.clone();
        let mut step = Step::default();
        if cut.seen != *seen {
            *seen = cut.seen;
            *live = cut.live;
            *subtitle = cut.subtitle;
            self.revision += 1;
            *state = cut.state;
            state.revision = self.revision;
            step.outcome = Outcome::Refreshed;
        }
        let due = now.saturating_duration_since(*read_at) >= TRANSCRIPT_POLL;
        if let Some(omitted) = cut.omitted.filter(|_| cut.live && due) {
            *read_at = now;
            step.fetches = self.fetch_output(thread, &task_id, omitted);
        }
        step
    }

    /// Ask for the output the stream left out of a task's call, unless it is on its way.
    fn fetch_output(&mut self, thread: &Thread, task_id: &str, omitted: Omitted) -> Vec<Fetch> {
        let source = Source::Task(task_id.to_string());
        if self.asked.contains_key(&source) {
            return Vec::new();
        }
        self.asked.insert(
            source,
            Asked {
                intent: Intent::Quiet,
                origin: Origin::Chat,
                again: None,
            },
        );
        vec![Fetch::TurnItem {
            task_id: task_id.to_string(),
            thread_id: thread.state.id().to_string(),
            item_id: omitted.item_id,
            revision: omitted.revision,
        }]
    }

    /// The call that started a task came back whole, with the output the stream left out.
    pub fn on_turn_item(
        &mut self,
        thread: &Thread,
        task_id: String,
        result: Result<Value, String>,
    ) -> Step {
        if self.answered(&Source::Task(task_id.clone())).is_none() {
            return Step::default();
        }
        let item = match result {
            Ok(value) => value,
            Err(error) => {
                tracing::info!(%error, "reading a task's output");
                return Step::default();
            }
        };
        let item = item.get("item").unwrap_or(&item);
        let text = match item.get("output") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Null) | None => return Step::default(),
            Some(other) => other.to_string(),
        };
        let Some(Open {
            kind:
                Kind::Task {
                    task_id: open,
                    live,
                    seen,
                    output,
                    ..
                },
            state,
            subtitle,
            ..
        }) = &mut self.open
        else {
            return Step::default();
        };
        if *open != task_id {
            return Step::default();
        }
        let Some(cut) = cut_task(thread.state, &task_id, Some(&text)) else {
            return Step::default();
        };
        *output = Some(text);
        *seen = cut.seen;
        *live = cut.live;
        *subtitle = cut.subtitle;
        self.revision += 1;
        *state = cut.state;
        state.revision = self.revision;
        Step::refreshed()
    }

    /// A pull request's detail came back from the host, through the server. `here` is
    /// where the conversation is, kept for the way back should this open over it.
    pub fn on_pull_request(
        &mut self,
        thread: &Thread,
        url: String,
        result: Result<Value, String>,
        here: R,
    ) -> Step {
        let source = Source::PullRequest(url.clone());
        let again = self
            .asked
            .get(&source)
            .and_then(|asked| Some((asked.again?, asked.origin)));
        let mut step = self.pull_request_answered(thread, url, result, here);
        // Asked for again while it was on its way: a label just set, or news from the sync,
        // may not be in what came back, so what is open is brought up to date once more.
        if let Some((fresh, origin)) = again
            && self.source().as_ref() == Some(&source)
        {
            let more = self.request(thread, source, Intent::Quiet, origin, fresh);
            step.fetches.extend(more.fetches);
        }
        step
    }

    fn pull_request_answered(
        &mut self,
        thread: &Thread,
        url: String,
        result: Result<Value, String>,
        here: R,
    ) -> Step {
        let source = Source::PullRequest(url.clone());
        let Some(asked) = self.answered(&source) else {
            return Step::default();
        };
        let quiet = asked.intent == Intent::Quiet;
        // A read nobody asked for only brings up to date what is still open.
        let reread = self.pull_request().is_some_and(|open| open.url == url);
        if quiet && !reread {
            return Step::default();
        }
        let detail = match result
            .and_then(|value| serde_json::from_value::<Detail>(value).map_err(|e| e.to_string()))
        {
            // Known by the link it was asked for by, which is what its review history is
            // kept under too.
            Ok(detail) => Detail {
                url: url.clone(),
                ..detail
            },
            Err(error) if quiet => {
                tracing::info!(%error, "bringing the pull request up to date");
                return Step::default();
            }
            Err(error) => return Step::warn(format!("reading the pull request: {error}")),
        };
        self.revision += 1;
        let state = match pull_request::state(&detail, self.revision) {
            Ok(state) => state,
            Err(error) if quiet => {
                tracing::info!(%error, "bringing the pull request up to date");
                return Step::default();
            }
            Err(error) => return Step::warn(format!("{error}")),
        };
        let fetches = self.images_for(thread, &detail);
        let (restore, before, synced, stack_seen) = match self.open.take() {
            Some(Open {
                kind:
                    Kind::PullRequest {
                        synced, stack_seen, ..
                    },
                restore,
                origin,
                ..
            }) if reread => (restore, Some(origin), synced, stack_seen),
            Some(open) => (
                open.restore,
                Some(open.origin),
                synced_link(thread.state, &url),
                format!("{:?}", stack_of(thread.state, &url)),
            ),
            None => (
                here,
                None,
                synced_link(thread.state, &url),
                format!("{:?}", stack_of(thread.state, &url)),
            ),
        };
        let origin = match before {
            Some(before) if reread && asked.origin == Origin::Chat => before,
            _ => asked.origin,
        };
        self.open = Some(Open {
            // The title is the first thing the view itself says.
            title: format!("{}#{}", detail.repository, detail.number),
            subtitle: "pull request".to_string(),
            kind: Kind::PullRequest {
                detail: Box::new(detail),
                synced,
                stack_seen,
            },
            state,
            restore,
            origin,
        });
        // Brought up to date under somebody, it leaves them where they were: writing, with
        // a selection, or in the middle of a search.
        if quiet {
            return Step {
                fetches,
                ..Step::refreshed()
            };
        }
        // A re-read keeps the place in it; anything else starts at the top.
        Step {
            fetches,
            toast: Some(Toast::Clear),
            outcome: Outcome::Opened {
                place: if reread { Place::Keep } else { Place::Top },
                take_focus: !reread && asked.origin != Origin::Sidebar,
            },
        }
    }

    /// A pull request's review history came back, through the server.
    pub fn on_activity(
        &mut self,
        thread: &Thread,
        url: String,
        result: Result<Value, String>,
    ) -> Step {
        let activity = result
            .and_then(|value| serde_json::from_value::<Activity>(value).map_err(|e| e.to_string()));
        // What was read before stays in view over a re-read that failed.
        if activity.is_err() && matches!(self.activity.get(&url), Some(Ok(_))) {
            return Step::default();
        }
        self.activity.insert(url.clone(), activity);
        let Some(detail) = self.pull_request().filter(|d| d.url == url).cloned() else {
            return Step::default();
        };
        // The reviews shown can have pictures of their own.
        let fetches = self.images_for(thread, &detail);
        self.redraw();
        Step {
            fetches,
            ..Step::refreshed()
        }
    }

    /// A picture a pull request shows came back, base64, or failed to.
    pub fn on_image(&mut self, url: String, data: Option<String>) -> Step {
        if data.is_none() {
            tracing::info!(%url, "a pull request's picture could not be fetched");
            // Asked for again on the next read, which is what `r` is for.
            self.images_asked.remove(&url);
        }
        self.images.insert(url, data);
        if self.pull_request().is_none() {
            return Step::default();
        }
        self.redraw();
        Step::refreshed()
    }

    /// The chat redraws what it has built only when the revision moves.
    fn redraw(&mut self) {
        if let Some(open) = &mut self.open {
            self.revision += 1;
            open.state.revision = self.revision;
        }
    }

    /// Fetch the pictures a pull request shows that have not been asked for yet. They are
    /// fetched from here rather than through the server, which has no way to hand them
    /// over; a picture on the pull request's own host is asked for with `gh`'s token for
    /// that host, since a private repository's uploads are private too. `gh` is asked in
    /// the thread's directory, because which account it answers for can depend on where
    /// it is run.
    fn images_for(&mut self, thread: &Thread, detail: &Detail) -> Vec<Fetch> {
        let host = url_host(&detail.url);
        let activity = self.activity.get(&detail.url).and_then(|a| a.as_ref().ok());
        let mut fetches = Vec::new();
        for url in pull_request::image_urls(detail, activity) {
            if !self.images_asked.insert(url.clone()) {
                continue;
            }
            // The token goes only where the pull request is, and never in the clear.
            let own =
                host.filter(|host| url.starts_with("https://") && url_host(&url) == Some(*host));
            fetches.push(Fetch::Image {
                token_host: own.map(str::to_string),
                directory: thread.directory.map(str::to_string),
                url,
            });
        }
        fetches
    }

    // ── Putting away ───────────────────────────────────────────────────

    /// Put the reading away, and give back where the conversation was. Where to go next is
    /// the caller's: `q` returns to the list the reading was opened from, while sending a
    /// message simply carries on.
    pub fn close(&mut self) -> Option<Closed<R>> {
        let open = self.open.take()?;
        if let Some((agent_id, _)) = self.child_asked.take() {
            self.asked.remove(&Source::Subagent(agent_id));
        }
        Some(Closed {
            restore: open.restore,
            origin: open.origin,
        })
    }

    /// Forget what is open and on its way, for a thread being left: a transcript belongs to
    /// the thread that ran the subagent, and a pull request to the thread it is linked to,
    /// and neither follows. What was fetched for pull requests is kept for the session.
    pub fn forget(&mut self) {
        self.open = None;
        self.asked.clear();
        self.child_asked = None;
    }
}

/// Output the stream left out of the call that started a task, and how to ask for it.
#[derive(Debug, Clone, PartialEq)]
struct Omitted {
    item_id: String,
    revision: String,
}

/// A background task's rows, cut out of the thread they are in.
struct Cut {
    state: ThreadState,
    title: String,
    subtitle: String,
    live: bool,
    /// The rows as they stand, to tell when they have moved.
    seen: u64,
    omitted: Option<Omitted>,
}

/// Cut a background task's rows out of the thread: the call that started it, which the
/// task is named after, and the news it has sent back about it since. `output` is what
/// was fetched of the call's output when the stream left it out.
fn cut_task(thread: &ThreadState, task_id: &str, output: Option<&str>) -> Option<Cut> {
    let activities = &thread.detail.activities;
    let mut call = activities
        .iter()
        .rfind(|activity| {
            activity.str("toolCallId").is_some()
                && (activity.str("nativeId") == Some(task_id)
                    || activity.str("toolCallId") == Some(task_id))
        })?
        .clone();
    let listed = thread
        .detail
        .shell
        .background_tasks
        .iter()
        .find(|task| task.task_id == task_id);
    let live = listed.is_some();
    let omitted = (call.payload["outputOmitted"] == Value::Bool(true)).then(|| Omitted {
        item_id: call.id.clone(),
        revision: if call.kind == "tool.completed" {
            call.str("updatedAt").unwrap_or_default().to_string()
        } else {
            "live".to_string()
        },
    });
    if let Some(text) = output.filter(|_| omitted.is_some()) {
        let place = if call.str("itemType") == Some("command_execution") {
            "rawOutput"
        } else {
            "result"
        };
        call.payload["data"][place] = json!({ "content": text });
    }
    let news: Vec<crate::model::Activity> = activities
        .iter()
        .filter(|activity| activity.str("parentItemId") == Some(call.id.as_str()))
        .cloned()
        .collect();
    let title = listed
        .and_then(|task| task.description.clone())
        .or_else(|| call.str("detail").map(str::to_string))
        .or_else(|| call.str("title").map(str::to_string))
        .unwrap_or_else(|| "background task".to_string());
    let kind = match listed.map(|task| task.kind.as_str()) {
        Some("monitor") => "monitor",
        Some("command") => "command",
        Some(_) => "background task",
        None if call.str("itemType") == Some("command_execution") => "command",
        None => "background task",
    };
    let subtitle = if live {
        format!("{kind} · running")
    } else {
        format!("{kind} · {}", call.str("status").unwrap_or("ended"))
    };
    let mut rows = vec![call];
    rows.extend(news);
    let seen = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        live.hash(&mut hasher);
        for row in &rows {
            row.id.hash(&mut hasher);
            row.kind.hash(&mut hasher);
            row.payload.to_string().hash(&mut hasher);
        }
        hasher.finish()
    };
    let state = ThreadState::from_snapshot(ThreadDetailSnapshot {
        snapshot_sequence: 0,
        thread: ThreadDetail {
            shell: thread.detail.shell.clone(),
            messages: Vec::new(),
            activities: rows,
            proposed_plans: Vec::new(),
            checkpoints: Vec::new(),
        },
        page: None,
        projection: None,
    });
    Some(Cut {
        state,
        title,
        subtitle,
        live,
        seen,
        omitted,
    })
}

/// What a subagent thread's header says under its title: what it runs as.
fn child_subtitle(agent: &Subagent) -> String {
    let parts: Vec<String> = agent
        .role
        .iter()
        .cloned()
        .chain(agent.model_label())
        .collect();
    if parts.is_empty() {
        "subagent".to_string()
    } else {
        parts.join(" · ")
    }
}

/// The agent as it stands, to tell when it has moved on.
fn seen_as(agent: &Subagent) -> String {
    format!("{} {:?}", agent.updated_at, agent.status)
}

/// Where the provider is writing a subagent's transcript. It tells the server the path
/// only when the task finishes, so for one still working it is taken from a task in the
/// same thread that has: they are written side by side in one directory and named after
/// the task, and the file is there from the moment the agent starts.
pub fn transcript_path(thread: &ThreadState, agent: &Subagent) -> Option<String> {
    if let Some(path) = &agent.output_file {
        return Some(path.clone());
    }
    let known = thread
        .detail
        .activities
        .iter()
        .rev()
        .find_map(|activity| activity.str("outputFile"))?;
    crate::transcript::sibling_path(known, &agent.id)
}

/// How the server knows a pull request linked to the open thread: its project, host,
/// repository, and number. And the number, to speak of it by.
pub fn pull_request_payload(thread: &Thread, url: &str) -> Result<(Value, u64), &'static str> {
    let shell = &thread.state.detail.shell;
    let pr = shell
        .all_pull_requests(thread.branch)
        .into_iter()
        .find(|pr| pr.url == url)
        .ok_or("that pull request is not linked to this thread")?;
    let mut payload = json!({
        "projectId": shell.project_id,
        "repository": pr.repository,
        "number": pr.number,
    });
    // One the server found on the branch but never linked has no host of its own; the
    // server then takes the project's.
    if !pr.host.is_empty() {
        payload["host"] = json!(pr.host);
    }
    Ok((payload, pr.number))
}

/// The stack the pull request with this link is a layer of, bottom to top, and which layer
/// it is. Only a stack of more than one: a pull request on its own is not in one.
fn stack_of(thread: &ThreadState, url: &str) -> Option<(Vec<PullRequestRef>, usize)> {
    thread
        .detail
        .shell
        .pull_request_chains()
        .into_iter()
        .filter(|chain| chain.layers.len() > 1)
        .find_map(|chain| {
            let at = chain.layers.iter().position(|layer| layer.url == url)?;
            Some((chain.layers, at))
        })
}

/// What the thread list says of a linked pull request, as something to compare.
fn synced_link(thread: &ThreadState, url: &str) -> Option<String> {
    let link = thread
        .detail
        .shell
        .pull_requests
        .iter()
        .find(|pr| pr.url == url)?;
    Some(format!("{:?}", link.snapshot))
}

/// The host a link is on.
pub fn url_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Where the conversation was, as the reader is handed it and gives it back.
    const CONVERSATION: &str = "the conversation";
    const FIRST: &str = "https://github.com/o/r/pull/1";
    const SECOND: &str = "https://github.com/o/r/pull/2";

    fn lent(state: &ThreadState) -> Thread<'_> {
        Thread {
            state,
            branch: None,
            directory: state.detail.shell.worktree_path.as_deref(),
        }
    }

    fn thread(activities: Vec<Value>, session: Value, pull_requests: Value) -> ThreadState {
        serde_json::from_value::<crate::model::ThreadDetailSnapshot>(json!({
            "snapshotSequence": 0,
            "thread": {"id": "t1", "projectId": "p", "title": "A thread",
                "modelSelection": {"instanceId": "c", "model": "m"},
                "session": session, "pullRequests": pull_requests,
                "messages": [], "activities": activities, "proposedPlans": []},
        }))
        .map(ThreadState::from_snapshot)
        .unwrap()
    }

    /// A thread whose one subagent is still working.
    pub(crate) fn thread_with_a_running_agent() -> ThreadState {
        let started = json!({
            "id": "task.started-1", "kind": "task.started", "tone": "info", "summary": "",
            "createdAt": "2026-01-01T00:00:00Z",
            "payload": {"taskId": "a1", "agentKind": "agent", "taskType": "local_agent",
                "title": "Look into it"},
        });
        thread(vec![started], Value::Null, json!([]))
    }

    /// A thread whose subagent `a1` is still working, beside a finished one whose output
    /// file says where `a1`'s is being written.
    pub(crate) fn thread_with_a_followable_agent() -> ThreadState {
        let activity = |id: &str, kind: &str, at: &str, payload: Value| {
            json!({"id": id, "kind": kind, "tone": "info", "summary": "", "createdAt": at,
                "payload": payload})
        };
        let started = |task: &str, at: &str| {
            json!({"taskId": task, "agentKind": "agent", "taskType": "local_agent",
                "title": format!("Look into {task}"), "startedAt": at})
        };
        let mut thread = thread(
            vec![
                activity(
                    "s0",
                    "task.started",
                    "2026-01-01T00:00:00Z",
                    started("a0", "2026-01-01T00:00:00Z"),
                ),
                activity(
                    "c0",
                    "task.completed",
                    "2026-01-01T00:00:01Z",
                    json!({"taskId": "a0", "status": "completed", "outputFile": "/tmp/tasks/a0.output"}),
                ),
                activity(
                    "s1",
                    "task.started",
                    "2026-01-01T00:00:02Z",
                    started("a1", "2026-01-01T00:00:02Z"),
                ),
            ],
            json!({"status": "running"}),
            json!([]),
        );
        thread.detail.shell.worktree_path = Some("/tmp".into());
        thread
    }

    /// The agent does something: a progress activity arrives on the thread.
    pub(crate) fn agent_progresses(thread: &mut ThreadState, at: &str) {
        let progress: crate::model::Activity = serde_json::from_value(json!({
            "id": format!("p-{at}"), "kind": "task.progress", "tone": "info", "summary": "",
            "createdAt": at, "payload": {"taskId": "a1", "detail": "Reading ws.ts", "lastToolName": "Read"},
        }))
        .unwrap();
        thread.detail.activities.push(progress);
    }

    /// The agent finishes.
    fn agent_finishes(thread: &mut ThreadState) {
        let done: crate::model::Activity = serde_json::from_value(json!({
            "id": "done", "kind": "task.completed", "tone": "info", "summary": "",
            "createdAt": "2026-01-01T00:00:09Z",
            "payload": {"taskId": "a1", "status": "completed", "summary": "Found it in ws.ts"},
        }))
        .unwrap();
        thread.detail.activities.push(done);
    }

    pub(crate) fn transcript_saying(replies: &[&str]) -> TranscriptFile {
        let mut contents = String::from(
            r#"{"type":"user","uuid":"u1","timestamp":"1","message":{"role":"user","content":"Look into it"}}"#,
        );
        for (n, reply) in replies.iter().enumerate() {
            contents.push('\n');
            contents.push_str(
                &json!({"type": "assistant", "uuid": format!("a{n}"), "timestamp": format!("{}", n + 2),
                    "message": {"role": "assistant", "content": [{"type": "text", "text": reply}]}})
                .to_string(),
            );
        }
        TranscriptFile {
            contents,
            truncated: false,
        }
    }

    /// A thread with a stack of two pull requests linked to it, #1 at the bottom.
    pub(crate) fn stacked_thread() -> ThreadState {
        let pr = |number: u64, head: &str, base: &str| {
            json!({"host":"github.com","repository":"o/r","number":number,
                "url":format!("https://github.com/o/r/pull/{number}"),"source":"agent",
                "linkedAt":"2026-01-01T00:00:00.000Z",
                "snapshot":{"state":"open","title":format!("layer {number}"),
                    "headBranch":head,"baseBranch":base}})
        };
        thread(
            Vec::new(),
            json!({"status": "running"}),
            json!([pr(2, "b", "a"), pr(1, "a", "main")]),
        )
    }

    pub(crate) fn open_detail(title: &str) -> Value {
        json!({
            "repository": "o/r", "number": 1, "title": title,
            "url": FIRST, "state": "open",
            "headBranch": "a", "baseBranch": "main",
        })
    }

    fn second_detail() -> Value {
        let mut second = open_detail("layer 2");
        second["number"] = json!(2);
        second["url"] = json!(SECOND);
        second
    }

    /// What the conversation on screen says, message by message.
    fn said(reader: &Reader<&'static str>) -> Vec<String> {
        reader
            .conversation()
            .map(|state| {
                state
                    .detail
                    .messages
                    .iter()
                    .map(|m| m.text.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn opened(place: Place, take_focus: bool) -> Outcome {
        Outcome::Opened { place, take_focus }
    }

    /// A transcript of `a1`, open, read at `at`.
    fn reading_a1(thread: &ThreadState, at: Instant) -> Reader<&'static str> {
        let mut reader = Reader::default();
        let _ = reader.open(&lent(thread), Source::Subagent("a1".into()), Origin::Chat);
        let answer = Ok(transcript_saying(&["Looking now."]));
        let _ = reader.on_transcript(&lent(thread), "a1".into(), answer, CONVERSATION, at);
        reader
    }

    /// Pull request #1, open.
    fn reading_the_first(thread: &ThreadState) -> Reader<&'static str> {
        let mut reader = Reader::default();
        let _ = reader.open(
            &lent(thread),
            Source::PullRequest(FIRST.into()),
            Origin::Chat,
        );
        let answer = Ok(open_detail("layer 1"));
        let _ = reader.on_pull_request(&lent(thread), FIRST.into(), answer, CONVERSATION);
        reader
    }

    fn pull_request_fetches(step: &Step) -> Vec<(String, bool)> {
        step.fetches
            .iter()
            .filter_map(|fetch| match fetch {
                Fetch::PullRequest { url, fresh, .. } => Some((url.clone(), *fresh)),
                _ => None,
            })
            .collect()
    }

    /// A thread whose subagent `a1` is still working, in a thread the server keeps of it.
    fn thread_with_a_child_agent() -> ThreadState {
        let started = json!({
            "id": "task.started-1", "kind": "task.started", "tone": "info", "summary": "",
            "createdAt": "2026-01-01T00:00:00Z",
            "payload": {"taskId": "a1", "agentKind": "agent", "title": "Look into it",
                "childThreadId": "child"},
        });
        thread(vec![started], json!({"status": "running"}), json!([]))
    }

    /// The subagent's thread as its stream sends it, saying `replies` after its brief.
    fn child_saying(replies: &[&str]) -> ThreadItem {
        let mut messages = vec![json!({"id": "u", "role": "user", "text": "Look into it"})];
        for (n, reply) in replies.iter().enumerate() {
            messages.push(json!({"id": format!("a{n}"), "role": "assistant", "text": reply}));
        }
        ThreadItem::Snapshot {
            snapshot: serde_json::from_value(json!({
                "snapshotSequence": 1,
                "thread": {"id": "child", "projectId": "p", "title": "Look into it",
                    "modelSelection": {"instanceId": "c", "model": "m"},
                    "messages": messages, "activities": []},
            }))
            .unwrap(),
        }
    }

    // ── Subagent threads ───────────────────────────────────────────────

    /// A subagent the server keeps a thread of is followed rather than read from a file:
    /// its thread opens at the top once it arrives, and what it says next comes in under
    /// the reader with nothing asked for again. Put away, it is let go.
    #[test]
    fn a_subagent_thread_is_followed_live() {
        let thread = thread_with_a_child_agent();
        let mut reader: Reader<&str> = Reader::default();
        let a1 = Source::Subagent("a1".into());
        let step = reader.open(&lent(&thread), a1.clone(), Origin::Chat);
        assert_eq!(
            step.fetches,
            vec![Fetch::Follow {
                thread_id: "child".into()
            }]
        );
        assert_eq!(reader.following(), Some("child"));
        assert!(reader.is_loading(&a1));

        let step = reader.on_child(
            &lent(&thread),
            "child",
            child_saying(&["Looking now."]),
            CONVERSATION,
        );
        assert_eq!(step.outcome, opened(Place::Top, true));
        assert!(!reader.is_loading(&a1));
        assert_eq!(said(&reader), vec!["Look into it", "Looking now."]);
        let header = reader.header().expect("a header");
        assert_eq!(header.title, "Look into it");
        assert_eq!(header.badges, vec![Badge::Working]);
        assert_eq!(header.hint, "q back");

        let step = reader.on_child(
            &lent(&thread),
            "child",
            child_saying(&["Looking now.", "Found it."]),
            CONVERSATION,
        );
        assert_eq!(step.outcome, Outcome::Refreshed);
        assert!(step.fetches.is_empty());
        assert_eq!(
            said(&reader),
            vec!["Look into it", "Looking now.", "Found it."]
        );

        let other = reader.on_child(&lent(&thread), "elsewhere", child_saying(&[]), CONVERSATION);
        assert_eq!(other.outcome, Outcome::Nothing);

        let again = reader.open(&lent(&thread), a1, Origin::Chat);
        assert!(again.fetches.is_empty(), "followed twice");
        assert_eq!(again.outcome, opened(Place::Keep, true));

        let closed = reader.close().expect("it was open");
        assert_eq!(closed.restore, CONVERSATION);
        assert_eq!(reader.following(), None);
    }

    /// Something else asked for before the subagent's thread arrives takes its place, and
    /// the thread is no longer followed nor opened when it does arrive.
    #[test]
    fn a_subagent_thread_asked_over_is_let_go() {
        let mut thread = thread_with_a_child_agent();
        thread.detail.shell = stacked_thread().detail.shell;
        let mut reader: Reader<&str> = Reader::default();
        let _ = reader.open(&lent(&thread), Source::Subagent("a1".into()), Origin::Chat);
        let _ = reader.open(
            &lent(&thread),
            Source::PullRequest(FIRST.into()),
            Origin::Chat,
        );
        assert_eq!(reader.following(), None);
        let step = reader.on_child(&lent(&thread), "child", child_saying(&[]), CONVERSATION);
        assert_eq!(step.outcome, Outcome::Nothing);
        assert!(!reader.is_open());
    }

    // ── Background tasks ───────────────────────────────────────────────

    /// A thread running background command `bg1`, started by call `item-1`, which has
    /// printed `output`, beside a call that has nothing to do with it and a note about it.
    fn thread_with_a_task(output: &str, omitted: bool) -> ThreadState {
        let at = "2026-01-01T00:00:00Z";
        let mut thread = thread(
            vec![
                json!({"id": "item-1", "kind": "tool.started", "tone": "tool", "summary": "Ran command",
                    "createdAt": at, "payload": {"itemType": "command_execution", "toolCallId": "item-1",
                    "nativeId": "bg1", "outputOmitted": omitted, "status": "inProgress",
                    "title": "Ran command", "detail": "npm run watch",
                    "data": {"command": "npm run watch", "rawOutput": {"content": output}}}}),
                json!({"id": "item-2", "kind": "tool.completed", "tone": "tool", "summary": "Ran command",
                    "createdAt": at, "payload": {"itemType": "command_execution", "toolCallId": "item-2",
                    "status": "completed", "title": "Ran command", "detail": "ls",
                    "data": {"command": "ls", "rawOutput": {"content": "a"}}}}),
                json!({"id": "note-1", "kind": "runtime.note", "tone": "info", "summary": "Build failed",
                    "createdAt": at, "payload": {"message": "3 errors", "parentItemId": "item-1"}}),
            ],
            json!({"status": "ready"}),
            json!([]),
        );
        thread.detail.shell.background_tasks = serde_json::from_value(json!([
            {"taskId": "bg1", "description": "watch the build", "kind": "command"},
        ]))
        .unwrap();
        thread
    }

    fn rows_read(reader: &Reader<&'static str>) -> Vec<String> {
        reader
            .conversation()
            .map(|state| {
                state
                    .detail
                    .activities
                    .iter()
                    .map(|a| a.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn output_read(reader: &Reader<&'static str>) -> Option<String> {
        let state = reader.conversation()?;
        state.detail.activities[0].payload["data"]["rawOutput"]["content"]
            .as_str()
            .map(str::to_string)
    }

    /// A task opens on the call that started it and the news about it, and nothing else in
    /// the thread; it keeps up as the thread moves, and says how it ended once it has.
    #[test]
    fn a_task_is_read_out_of_the_thread_and_keeps_up() {
        let mut thread = thread_with_a_task("compiling", false);
        let mut reader: Reader<&str> = Reader::default();
        let step = reader.open_task(&lent(&thread), "bg1", Origin::Tasks, CONVERSATION);
        assert_eq!(step.outcome, opened(Place::Top, true));
        assert!(step.fetches.is_empty(), "the stream had all of it");
        assert_eq!(rows_read(&reader), ["item-1", "note-1"]);
        assert_eq!(reader.source(), Some(Source::Task("bg1".into())));
        let header = reader.header().expect("a header");
        assert_eq!(header.title, "watch the build");
        assert_eq!(header.subtitle, "command · running");
        assert_eq!(header.badges, vec![Badge::Working]);

        let unmoved = reader.tick(&lent(&thread), Instant::now());
        assert_eq!(unmoved.outcome, Outcome::Nothing);

        thread.detail.activities[0].payload["data"]["rawOutput"]["content"] =
            json!("compiling\nwatching");
        let step = reader.tick(&lent(&thread), Instant::now());
        assert_eq!(step.outcome, Outcome::Refreshed);
        assert_eq!(output_read(&reader).as_deref(), Some("compiling\nwatching"));

        thread.detail.shell.background_tasks.clear();
        thread.detail.activities[0].kind = "tool.completed".into();
        thread.detail.activities[0].payload["status"] = json!("completed");
        let step = reader.tick(&lent(&thread), Instant::now());
        assert_eq!(step.outcome, Outcome::Refreshed);
        let header = reader.header().expect("still open once it has ended");
        assert_eq!(header.subtitle, "command · completed");
        assert!(header.badges.is_empty());

        let closed = reader.close().expect("it was open");
        assert_eq!(
            (closed.restore, closed.origin),
            (CONVERSATION, Origin::Tasks)
        );
    }

    /// A pull request the agent has watched lists its watch as a task, which reads the pull
    /// request: the watch has nothing of its own in the thread, and the pull request is
    /// what it waits on. `q` still goes back to the task list.
    #[test]
    fn a_watched_pull_request_is_read_from_its_task() {
        let mut thread = stacked_thread();
        thread.detail.shell.pull_requests[1].host = "GitHub.com".into();
        thread.detail.shell.pull_requests[1].watch = Some(json!({"wakes": 0}));
        let mut reader: Reader<&str> = Reader::default();
        let step = reader.open_task(
            &lent(&thread),
            "pull-request-watch:github.com/o/r#1",
            Origin::Tasks,
            CONVERSATION,
        );
        assert_eq!(
            pull_request_fetches(&step),
            vec![(FIRST.to_string(), false)]
        );
        let _ = reader.on_pull_request(
            &lent(&thread),
            FIRST.into(),
            Ok(open_detail("layer 1")),
            CONVERSATION,
        );
        assert_eq!(reader.source(), Some(Source::PullRequest(FIRST.into())));
        assert_eq!(
            reader.close().map(|closed| closed.origin),
            Some(Origin::Tasks)
        );

        let unwatched = reader.open_task(
            &lent(&thread),
            "pull-request-watch:github.com/o/r#2",
            Origin::Tasks,
            CONVERSATION,
        );
        assert!(unwatched.fetches.is_empty(), "#2 is not watched");
    }

    /// Output the stream left out is read whole, and read again while the task runs: every
    /// few seconds on its own, and at once on `r`.
    #[test]
    fn a_tasks_omitted_output_is_read_whole() {
        let thread = thread_with_a_task("", true);
        let mut reader: Reader<&str> = Reader::default();
        let at = Instant::now();
        let step = reader.open_task(&lent(&thread), "bg1", Origin::Tasks, CONVERSATION);
        let asked = Fetch::TurnItem {
            task_id: "bg1".into(),
            thread_id: "t1".into(),
            item_id: "item-1".into(),
            revision: "live".into(),
        };
        assert_eq!(step.fetches, vec![asked.clone()]);

        let step = reader.on_turn_item(
            &lent(&thread),
            "bg1".into(),
            Ok(json!({"item": {"id": "item-1", "output": "all of it"}})),
        );
        assert_eq!(step.outcome, Outcome::Refreshed);
        assert_eq!(output_read(&reader).as_deref(), Some("all of it"));

        let soon = reader.tick(&lent(&thread), at);
        assert!(soon.fetches.is_empty(), "read a moment ago");
        assert_eq!(
            output_read(&reader).as_deref(),
            Some("all of it"),
            "kept between reads"
        );
        let later = reader.tick(&lent(&thread), Instant::now() + TRANSCRIPT_POLL);
        assert_eq!(later.fetches, vec![asked.clone()]);
        let _ = reader.on_turn_item(&lent(&thread), "bg1".into(), Err("gone".into()));

        let step = reader.reread(&lent(&thread), Source::Task("bg1".into()), Origin::Chat);
        assert_eq!(step.fetches, vec![asked]);
        assert_eq!(step.outcome, opened(Place::Keep, true));
        assert_eq!(output_read(&reader).as_deref(), Some("all of it"));
    }

    // ── Transcripts ────────────────────────────────────────────────────

    /// A transcript is asked for by where the provider is writing it, once however often it
    /// is asked for, and opens at its top with the keys on it.
    #[test]
    fn a_transcript_is_read_from_where_the_provider_writes_it() {
        let thread = thread_with_a_followable_agent();
        let mut reader: Reader<&str> = Reader::default();
        let a1 = Source::Subagent("a1".into());
        let step = reader.open(&lent(&thread), a1.clone(), Origin::Chat);
        assert_eq!(
            step.fetches,
            vec![Fetch::Transcript {
                agent_id: "a1".into(),
                cwd: "/tmp".into(),
                path: "/tmp/tasks/a1.output".into(),
            }]
        );
        assert!(reader.is_loading(&a1));
        let again = reader.open(&lent(&thread), a1.clone(), Origin::Chat);
        assert!(again.fetches.is_empty(), "asked for twice");

        let answer = Ok(transcript_saying(&["Looking now."]));
        let step = reader.on_transcript(
            &lent(&thread),
            "a1".into(),
            answer,
            CONVERSATION,
            Instant::now(),
        );
        assert_eq!(step.outcome, opened(Place::Top, true));
        assert!(!reader.is_loading(&a1));
        assert_eq!(said(&reader), vec!["Look into it", "Looking now."]);
        let header = reader.header().expect("a header");
        assert_eq!(header.title, "Look into a1");
        assert_eq!(header.badges, vec![Badge::Working]);
        assert_eq!(header.hint, "r re-reads · q back");
        assert_eq!(
            reader.context().as_deref(),
            Some("Sent looking at the transcript for subagent a1:")
        );
    }

    /// A subagent with nowhere its transcript is written says so, and one no longer in the
    /// thread says that; neither asks for anything.
    #[test]
    fn a_transcript_that_cannot_be_read_says_why() {
        let thread = thread_with_a_running_agent();
        let mut reader: Reader<&str> = Reader::default();
        let step = reader.open(&lent(&thread), Source::Subagent("a1".into()), Origin::Chat);
        assert!(step.fetches.is_empty());
        assert_eq!(
            step.toast,
            Some(Toast::Say("no transcript for this subagent yet".into()))
        );
        let step = reader.open(
            &lent(&thread),
            Source::Subagent("gone".into()),
            Origin::Chat,
        );
        assert_eq!(
            step.toast,
            Some(Toast::Warn(
                "that subagent is no longer in this thread".into()
            ))
        );
    }

    /// A read asked for that fails says so; one nobody asked for fails without a word and
    /// leaves what is open as it was.
    #[test]
    fn only_a_loud_read_fails_out_loud() {
        let mut thread = thread_with_a_followable_agent();
        let start = Instant::now();
        let mut reader: Reader<&str> = Reader::default();
        let _ = reader.open(&lent(&thread), Source::Subagent("a1".into()), Origin::Chat);
        let step = reader.on_transcript(
            &lent(&thread),
            "a1".into(),
            Err("gone".into()),
            CONVERSATION,
            start,
        );
        assert_eq!(
            step.toast,
            Some(Toast::Warn("reading the transcript: gone".into()))
        );
        assert!(!reader.is_open());

        let mut reader = reading_a1(&thread, start);
        agent_progresses(&mut thread, "2026-01-01T00:00:05Z");
        let step = reader.tick(&lent(&thread), start);
        assert_eq!(step.fetches.len(), 1);
        let step = reader.on_transcript(
            &lent(&thread),
            "a1".into(),
            Err("gone".into()),
            CONVERSATION,
            start,
        );
        assert_eq!(step.toast, None);
        assert_eq!(said(&reader), vec!["Look into it", "Looking now."]);
    }

    /// A running agent's transcript reads itself again when the agent does something, and
    /// shows what was written without moving whoever is reading it.
    #[test]
    fn a_running_agents_transcript_follows_what_it_does() {
        let mut thread = thread_with_a_followable_agent();
        let start = Instant::now();
        let mut reader = reading_a1(&thread, start);
        let answer = |reader: &mut Reader<&'static str>,
                      thread: &ThreadState,
                      at: Instant,
                      replies: &[&str]| {
            reader.on_transcript(
                &lent(thread),
                "a1".into(),
                Ok(transcript_saying(replies)),
                "somewhere else",
                at,
            )
        };

        let step = reader.tick(&lent(&thread), start + Duration::from_secs(1));
        assert!(
            step.fetches.is_empty(),
            "nothing has happened, and it is not due"
        );

        agent_progresses(&mut thread, "2026-01-01T00:00:05Z");
        let step = reader.tick(&lent(&thread), start + Duration::from_secs(1));
        assert_eq!(step.fetches.len(), 1, "the agent did something");
        assert_eq!(step.toast, None);
        let step = answer(&mut reader, &thread, start, &["Looking now.", "Found it."]);
        assert_eq!(step.outcome, Outcome::Refreshed, "nobody is moved");
        assert_eq!(step.toast, None);
        assert!(said(&reader).contains(&"Found it.".to_string()));

        // A stretch of writing says nothing on the thread; the clock catches it.
        let later = start + TRANSCRIPT_POLL * 2;
        let step = reader.tick(&lent(&thread), later);
        assert_eq!(step.fetches.len(), 1, "it is due");
        let more = ["Looking now.", "Found it.", "Still writing."];
        let _ = answer(&mut reader, &thread, later, &more);
        assert!(said(&reader).contains(&"Still writing.".to_string()));

        // Finished, it is read once more for how it ended, and then left alone.
        agent_finishes(&mut thread);
        let step = reader.tick(&lent(&thread), later);
        assert_eq!(step.fetches.len(), 1, "how it ended");
        let _ = answer(&mut reader, &thread, later, &["Looking now.", "Done."]);
        assert!(!reader.header().unwrap().badges.contains(&Badge::Working));
        let step = reader.tick(&lent(&thread), later + TRANSCRIPT_POLL * 2);
        assert!(step.fetches.is_empty(), "a finished run is not read again");

        let closed = reader.close().expect("it was open");
        assert_eq!(closed.restore, CONVERSATION, "the way back is the way in");
    }

    /// Put away, a transcript is not brought back by a read that was already on its way.
    #[test]
    fn a_put_away_transcript_stays_away() {
        let mut thread = thread_with_a_followable_agent();
        let start = Instant::now();
        let mut reader = reading_a1(&thread, start);
        agent_progresses(&mut thread, "2026-01-01T00:00:05Z");
        let step = reader.tick(&lent(&thread), start);
        assert_eq!(step.fetches.len(), 1);
        let closed = reader.close().expect("it was open");
        assert_eq!(closed.restore, CONVERSATION);
        assert_eq!(closed.origin, Origin::Chat);

        let answer = Ok(transcript_saying(&["Looking now.", "Found it."]));
        let step = reader.on_transcript(&lent(&thread), "a1".into(), answer, CONVERSATION, start);
        assert_eq!(step.outcome, Outcome::Nothing);
        assert!(!reader.is_open());
    }

    /// `r` on an agent still working reads what it has written since, lands at the end,
    /// which is the part that is new, and moves the revision so it is what is drawn.
    #[test]
    fn reading_a_running_agent_again_follows_what_it_has_written_since() {
        let thread = thread_with_a_followable_agent();
        let start = Instant::now();
        let mut reader = reading_a1(&thread, start);
        let revision = reader.revision();

        let step = reader.reread(&lent(&thread), Source::Subagent("a1".into()), Origin::Chat);
        assert_eq!(step.fetches.len(), 1);
        let answer = Ok(transcript_saying(&["Looking now.", "Found it in ws.ts."]));
        let step =
            reader.on_transcript(&lent(&thread), "a1".into(), answer, "somewhere else", start);
        assert_eq!(step.outcome, opened(Place::End, true));
        assert!(reader.revision() > revision);
        assert_eq!(reader.conversation().unwrap().revision, reader.revision());
        assert!(said(&reader).contains(&"Found it in ws.ts.".to_string()));
    }

    /// Asking for a transcript already being brought up to date asks for nothing more, and
    /// its answer is then the one asked for: it takes the keys, and remembers the roster it
    /// was asked from.
    #[test]
    fn asking_for_a_transcript_being_followed_opens_it_loudly() {
        let mut thread = thread_with_a_followable_agent();
        let start = Instant::now();
        let mut reader = reading_a1(&thread, start);
        agent_progresses(&mut thread, "2026-01-01T00:00:05Z");
        let quiet = reader.tick(&lent(&thread), start);
        assert_eq!(quiet.fetches.len(), 1, "a follow read on its way");

        let loud = reader.open(
            &lent(&thread),
            Source::Subagent("a1".into()),
            Origin::Roster,
        );
        assert!(loud.fetches.is_empty(), "the read on its way will do");
        let answer = Ok(transcript_saying(&["Looking now.", "Found it."]));
        let step = reader.on_transcript(&lent(&thread), "a1".into(), answer, CONVERSATION, start);
        assert_eq!(step.outcome, opened(Place::End, true));
        assert_eq!(
            reader.close().map(|closed| closed.origin),
            Some(Origin::Roster)
        );
    }

    /// A transcript opened while another is being followed is asked for, rather than
    /// waiting on a read of something else.
    #[test]
    fn another_transcript_is_not_held_up_by_a_follow_read() {
        let mut thread = thread_with_a_followable_agent();
        let start = Instant::now();
        let mut reader = reading_a1(&thread, start);
        agent_progresses(&mut thread, "2026-01-01T00:00:05Z");
        let _ = reader.tick(&lent(&thread), start);
        let step = reader.open(&lent(&thread), Source::Subagent("a0".into()), Origin::Chat);
        assert_eq!(step.fetches.len(), 1);
        let answer = Ok(transcript_saying(&["Done."]));
        let step = reader.on_transcript(&lent(&thread), "a0".into(), answer, CONVERSATION, start);
        assert_eq!(step.outcome, opened(Place::Top, true));
        assert_eq!(reader.agent_id(), Some("a0"));
    }

    /// A transcript cut short says so.
    #[test]
    fn a_truncated_transcript_says_so() {
        let thread = thread_with_a_followable_agent();
        let mut reader: Reader<&str> = Reader::default();
        let _ = reader.open(&lent(&thread), Source::Subagent("a0".into()), Origin::Chat);
        let mut file = transcript_saying(&["Done."]);
        file.truncated = true;
        let _ = reader.on_transcript(
            &lent(&thread),
            "a0".into(),
            Ok(file),
            CONVERSATION,
            Instant::now(),
        );
        let header = reader.header().unwrap();
        assert_eq!(header.badges, vec![Badge::Truncated]);
        assert_eq!(header.hint, "q back");
    }

    // ── Pull requests ──────────────────────────────────────────────────

    /// A pull request is asked for by the link's own identity, says it is on its way, and
    /// opens at the top with the keys on it once it comes.
    #[test]
    fn a_pull_request_is_read_by_its_identity() {
        let thread = stacked_thread();
        let mut reader: Reader<&str> = Reader::default();
        let step = reader.open(
            &lent(&thread),
            Source::PullRequest(FIRST.into()),
            Origin::Chat,
        );
        assert_eq!(
            step.fetches,
            vec![Fetch::PullRequest {
                url: FIRST.into(),
                payload: json!({"projectId": "p", "host": "github.com", "repository": "o/r", "number": 1}),
                fresh: false,
            }]
        );
        assert_eq!(step.toast, Some(Toast::Say("reading #1…".into())));

        let answer = Ok(open_detail("layer 1"));
        let step = reader.on_pull_request(&lent(&thread), FIRST.into(), answer, CONVERSATION);
        assert_eq!(step.outcome, opened(Place::Top, true));
        assert_eq!(step.toast, Some(Toast::Clear));
        let header = reader.header().unwrap();
        assert_eq!(header.title, "o/r#1");
        assert_eq!(header.hint, "r re-reads · gx opens · q back");
        assert_eq!(
            reader.context().as_deref(),
            Some("Sent looking at pull request o/r#1 (https://github.com/o/r/pull/1):")
        );
        assert_eq!(reader.noun(), Some("the pull request"));
    }

    /// One the thread does not have is not asked for.
    #[test]
    fn a_pull_request_not_on_the_thread_is_not_read() {
        let thread = stacked_thread();
        let mut reader: Reader<&str> = Reader::default();
        let url = "https://github.com/o/r/pull/9";
        let step = reader.open(
            &lent(&thread),
            Source::PullRequest(url.into()),
            Origin::Chat,
        );
        assert!(step.fetches.is_empty());
        assert_eq!(
            step.toast,
            Some(Toast::Warn(
                "that pull request is not linked to this thread".into()
            ))
        );
    }

    /// Brought up to date, it moves nobody and says nothing; one that comes back after the
    /// view was put away does not bring it back.
    #[test]
    fn a_quiet_read_disturbs_nobody() {
        let thread = stacked_thread();
        let mut reader = reading_the_first(&thread);
        let step = reader.refresh(&lent(&thread));
        assert_eq!(pull_request_fetches(&step), vec![(FIRST.to_string(), true)]);
        assert_eq!(step.toast, None);
        let answer = Ok(open_detail("renamed"));
        let step = reader.on_pull_request(&lent(&thread), FIRST.into(), answer, "elsewhere");
        assert_eq!(step.outcome, Outcome::Refreshed);
        assert_eq!(step.toast, None);
        assert_eq!(
            reader.pull_request().map(|d| d.title.as_str()),
            Some("renamed")
        );

        let _ = reader.refresh(&lent(&thread));
        let closed = reader.close().unwrap();
        assert_eq!(closed.restore, CONVERSATION);
        let answer = Ok(open_detail("late"));
        let step = reader.on_pull_request(&lent(&thread), FIRST.into(), answer, CONVERSATION);
        assert_eq!(step.outcome, Outcome::Nothing);
        assert!(!reader.is_open(), "put away, it stays away");
    }

    /// `r` reads it again past the server's copy, out loud, and keeps the place in it.
    #[test]
    fn reading_a_pull_request_again_keeps_the_place() {
        let thread = stacked_thread();
        let mut reader = reading_the_first(&thread);
        let step = reader.reread(
            &lent(&thread),
            Source::PullRequest(FIRST.into()),
            Origin::Chat,
        );
        assert_eq!(pull_request_fetches(&step), vec![(FIRST.to_string(), true)]);
        assert_eq!(step.toast, Some(Toast::Say("reading #1…".into())));
        let answer = Ok(open_detail("layer 1"));
        let step = reader.on_pull_request(&lent(&thread), FIRST.into(), answer, "elsewhere");
        assert_eq!(step.outcome, opened(Place::Keep, false));
        assert_eq!(reader.close().unwrap().restore, CONVERSATION);
    }

    /// The first count after connecting is where the server stands; a change is a turn
    /// that ended, and the pull request open is read again, past the server's copy of it.
    #[test]
    fn a_pull_request_is_read_again_when_the_server_says_so() {
        let thread = stacked_thread();
        let mut reader = reading_the_first(&thread);
        assert!(
            !reader.server_refreshed(4),
            "where it stands is not a change"
        );
        assert!(!reader.server_refreshed(4));
        assert!(reader.server_refreshed(5));
    }

    /// The server's own sync bringing news of the pull request open is a reason to read it
    /// again; news of anything else is not.
    #[test]
    fn news_from_the_sync_reads_it_again() {
        let mut thread = stacked_thread();
        let mut reader = reading_the_first(&thread);
        let step = reader.notice_sync(&lent(&thread));
        assert!(step.fetches.is_empty(), "nothing new yet");

        let link = |thread: &mut ThreadState, number| {
            let shell = &mut thread.detail.shell;
            let pr = shell
                .pull_requests
                .iter_mut()
                .find(|pr| pr.number == number);
            pr.unwrap().snapshot.as_mut().unwrap().checks_state = Some("failing".into());
        };
        link(&mut thread, 2);
        let step = reader.notice_sync(&lent(&thread));
        assert!(step.fetches.is_empty(), "news of another layer");

        link(&mut thread, 1);
        let step = reader.notice_sync(&lent(&thread));
        assert_eq!(pull_request_fetches(&step), vec![(FIRST.to_string(), true)]);
        assert_eq!(step.toast, None);
    }

    /// `]` reads the layer above, and the ends of a stack say so rather than moving.
    #[test]
    fn brackets_move_through_a_stack() {
        let thread = stacked_thread();
        let mut reader = reading_the_first(&thread);
        let step = reader.move_through_stack(&lent(&thread), -1);
        assert!(step.fetches.is_empty());
        assert_eq!(
            step.toast,
            Some(Toast::Say("already the bottom of the stack".into()))
        );
        let step = reader.move_through_stack(&lent(&thread), 1);
        assert_eq!(
            pull_request_fetches(&step),
            vec![(SECOND.to_string(), false)]
        );
        let answer = Ok(second_detail());
        let _ = reader.on_pull_request(&lent(&thread), SECOND.into(), answer, CONVERSATION);
        let step = reader.move_through_stack(&lent(&thread), 1);
        assert_eq!(
            step.toast,
            Some(Toast::Say("already the top of the stack".into()))
        );
        assert_eq!(
            reader.close().unwrap().restore,
            CONVERSATION,
            "still the conversation's"
        );
    }

    /// Asked for from the sidebar, it leaves the keys there, so the next one is `j` and
    /// `Enter` away; asked for from anywhere else, it takes them.
    #[test]
    fn a_read_from_the_sidebar_leaves_the_keys_there() {
        let thread = stacked_thread();
        let mut reader: Reader<&str> = Reader::default();
        let _ = reader.open(
            &lent(&thread),
            Source::PullRequest(SECOND.into()),
            Origin::Sidebar,
        );
        let answer = Ok(second_detail());
        let step = reader.on_pull_request(&lent(&thread), SECOND.into(), answer, CONVERSATION);
        assert_eq!(step.outcome, opened(Place::Top, false));

        let _ = reader.open(
            &lent(&thread),
            Source::PullRequest(FIRST.into()),
            Origin::Chat,
        );
        let answer = Ok(open_detail("layer 1"));
        let step = reader.on_pull_request(&lent(&thread), FIRST.into(), answer, "elsewhere");
        assert_eq!(step.outcome, opened(Place::Top, true));
        assert_eq!(reader.close().unwrap().restore, CONVERSATION);
    }

    /// Bringing the open pull request up to date does not lose one asked for on its way:
    /// that one still opens when it comes, and the quiet read of the one it replaced then
    /// changes nothing.
    #[test]
    fn a_quiet_read_does_not_cancel_a_loud_one() {
        let thread = stacked_thread();
        let mut reader = reading_the_first(&thread);
        let loud = reader.open(
            &lent(&thread),
            Source::PullRequest(SECOND.into()),
            Origin::Chat,
        );
        assert_eq!(
            pull_request_fetches(&loud),
            vec![(SECOND.to_string(), false)]
        );
        let quiet = reader.refresh(&lent(&thread));
        assert_eq!(
            pull_request_fetches(&quiet),
            vec![(FIRST.to_string(), true)]
        );

        let answer = Ok(second_detail());
        let step = reader.on_pull_request(&lent(&thread), SECOND.into(), answer, CONVERSATION);
        assert_eq!(step.outcome, opened(Place::Top, true));
        assert_eq!(reader.pull_request().map(|d| d.number), Some(2));

        let answer = Ok(open_detail("layer 1"));
        let step = reader.on_pull_request(&lent(&thread), FIRST.into(), answer, CONVERSATION);
        assert_eq!(step.outcome, Outcome::Nothing);
        assert_eq!(reader.pull_request().map(|d| d.number), Some(2));
    }

    /// A pull request's "reading #N…" is put away when a transcript is asked for over it,
    /// since what that read brings back no longer opens.
    #[test]
    fn a_transcript_asked_for_over_a_pull_request_read_puts_its_toast_away() {
        let mut thread = thread_with_a_followable_agent();
        thread.detail.shell.pull_requests = stacked_thread().detail.shell.pull_requests;
        let mut reader: Reader<&str> = Reader::default();
        let step = reader.open(
            &lent(&thread),
            Source::PullRequest(FIRST.into()),
            Origin::Chat,
        );
        assert_eq!(step.toast, Some(Toast::Say("reading #1…".into())));
        let step = reader.open(
            &lent(&thread),
            Source::Subagent("a1".into()),
            Origin::Roster,
        );
        assert_eq!(step.toast, Some(Toast::Clear));
    }

    /// Of two asked for one after the other, only the later opens.
    #[test]
    fn only_the_last_read_asked_for_opens() {
        let thread = stacked_thread();
        let mut reader: Reader<&str> = Reader::default();
        let _ = reader.open(
            &lent(&thread),
            Source::PullRequest(FIRST.into()),
            Origin::Chat,
        );
        let _ = reader.open(
            &lent(&thread),
            Source::PullRequest(SECOND.into()),
            Origin::Chat,
        );
        let answer = Ok(open_detail("layer 1"));
        let step = reader.on_pull_request(&lent(&thread), FIRST.into(), answer, CONVERSATION);
        assert_eq!(step.outcome, Outcome::Nothing);
        assert!(!reader.is_open());
        let answer = Ok(second_detail());
        let step = reader.on_pull_request(&lent(&thread), SECOND.into(), answer, CONVERSATION);
        assert_eq!(step.outcome, opened(Place::Top, true));
    }

    /// The review history fills in what is open when it comes, and a re-read that could
    /// not get it leaves what was read before in view.
    #[test]
    fn the_review_history_fills_in_what_is_open() {
        let thread = stacked_thread();
        let mut reader = reading_the_first(&thread);
        let revision = reader.revision();
        let history = json!({"comments": [], "reviewThreads": []});
        let step = reader.on_activity(&lent(&thread), FIRST.into(), Ok(history));
        assert_eq!(step.outcome, Outcome::Refreshed);
        assert!(reader.revision() > revision);
        let step = reader.on_activity(&lent(&thread), FIRST.into(), Err("slow".into()));
        assert_eq!(step.outcome, Outcome::Nothing);
        assert!(matches!(reader.activity.get(FIRST), Some(Ok(_))));
    }

    /// The pictures a description shows are fetched once each, those on the pull request's
    /// own host with `gh`'s token for it, and only over HTTPS; one that failed is asked for
    /// on the next read.
    #[test]
    fn pictures_are_fetched_once_and_again_after_failing() {
        let thread = stacked_thread();
        let own = "https://github.com/user-attachments/assets/1.png";
        let elsewhere = "https://example.com/2.png";
        let plain = "http://github.com/user-attachments/assets/3.png";
        let mut detail = open_detail("layer 1");
        detail["body"] = json!(format!(
            "![one]({own})\n\n![two]({elsewhere})\n\n![three]({plain})"
        ));
        let images = |step: &Step| -> Vec<(String, Option<String>)> {
            step.fetches
                .iter()
                .filter_map(|fetch| match fetch {
                    Fetch::Image {
                        url, token_host, ..
                    } => Some((url.clone(), token_host.clone())),
                    _ => None,
                })
                .collect()
        };
        let mut reader: Reader<&str> = Reader::default();
        let read = |reader: &mut Reader<&'static str>| {
            let _ = reader.reread(
                &lent(&thread),
                Source::PullRequest(FIRST.into()),
                Origin::Chat,
            );
            reader.on_pull_request(
                &lent(&thread),
                FIRST.into(),
                Ok(detail.clone()),
                CONVERSATION,
            )
        };
        let step = read(&mut reader);
        assert_eq!(
            images(&step),
            vec![
                (own.to_string(), Some("github.com".to_string())),
                (elsewhere.to_string(), None),
                (plain.to_string(), None),
            ]
        );
        assert!(images(&read(&mut reader)).is_empty(), "asked for once");

        let revision = reader.revision();
        let step = reader.on_image(own.into(), Some("aGk=".into()));
        assert_eq!(step.outcome, Outcome::Refreshed);
        assert!(reader.revision() > revision, "drawn with it");
        let _ = reader.on_image(elsewhere.into(), None);
        assert_eq!(
            images(&read(&mut reader)),
            vec![(elsewhere.to_string(), None)]
        );
    }

    /// A label put on shows at once, and the pull request is read again, quietly, for how
    /// the host shows it.
    #[test]
    fn a_label_set_shows_at_once() {
        let thread = stacked_thread();
        let mut editable = open_detail("layer 1");
        editable["capabilities"] = json!({"labels": true});
        editable["labels"] = json!([{"name": "docs"}]);
        let mut reader: Reader<&str> = Reader::default();
        let _ = reader.open(
            &lent(&thread),
            Source::PullRequest(FIRST.into()),
            Origin::Chat,
        );
        let _ = reader.on_pull_request(&lent(&thread), FIRST.into(), Ok(editable), CONVERSATION);
        assert_eq!(
            reader.header().unwrap().hint,
            "r re-reads · gx opens · L labels · q back"
        );
        let labels = |reader: &Reader<&str>| -> Vec<String> {
            reader
                .pull_request()
                .unwrap()
                .labels
                .iter()
                .map(|label| label.name.clone())
                .collect()
        };

        let revision = reader.revision();
        let step = reader.label_set(&lent(&thread), FIRST, "bug", true);
        assert_eq!(step.outcome, Outcome::Refreshed);
        assert_eq!(pull_request_fetches(&step), vec![(FIRST.to_string(), true)]);
        assert_eq!(labels(&reader), vec!["docs", "bug"]);
        assert!(reader.revision() > revision);

        let _ = reader.label_set(&lent(&thread), FIRST, "docs", false);
        assert_eq!(labels(&reader), vec!["bug"]);
        let step = reader.label_set(&lent(&thread), SECOND, "docs", true);
        assert_eq!(step.outcome, Outcome::Nothing, "not the one open");
    }

    /// A label set while a read is on its way is read for once more when that one comes
    /// back, since what it brings was read before the label went on.
    #[test]
    fn a_label_set_while_a_read_is_on_its_way_is_read_for_again() {
        let thread = stacked_thread();
        let mut editable = open_detail("layer 1");
        editable["capabilities"] = json!({"labels": true});
        let mut reader = reading_the_first(&thread);
        let _ = reader.on_pull_request(
            &lent(&thread),
            FIRST.into(),
            Ok(editable.clone()),
            CONVERSATION,
        );
        let sync = reader.refresh(&lent(&thread));
        assert_eq!(pull_request_fetches(&sync), vec![(FIRST.to_string(), true)]);
        let step = reader.label_set(&lent(&thread), FIRST, "bug", true);
        assert!(pull_request_fetches(&step).is_empty(), "one is on its way");

        // What the sync's read brings was read before the label went on.
        let step = reader.on_pull_request(&lent(&thread), FIRST.into(), Ok(editable), CONVERSATION);
        assert_eq!(pull_request_fetches(&step), vec![(FIRST.to_string(), true)]);
        let mut labelled = open_detail("layer 1");
        labelled["labels"] = json!([{"name": "bug"}]);
        let step = reader.on_pull_request(&lent(&thread), FIRST.into(), Ok(labelled), CONVERSATION);
        assert!(pull_request_fetches(&step).is_empty(), "and only once");
        assert_eq!(reader.pull_request().unwrap().labels[0].name, "bug");
    }

    /// Forgotten with the thread, nothing read for it opens afterwards.
    #[test]
    fn a_thread_left_takes_its_reads_with_it() {
        let thread = stacked_thread();
        let mut reader = reading_the_first(&thread);
        let _ = reader.open(
            &lent(&thread),
            Source::PullRequest(SECOND.into()),
            Origin::Chat,
        );
        reader.forget();
        assert!(!reader.is_open());
        assert!(!reader.is_loading(&Source::PullRequest(SECOND.into())));
        let answer = Ok(second_detail());
        let step = reader.on_pull_request(&lent(&thread), SECOND.into(), answer, CONVERSATION);
        assert_eq!(step.outcome, Outcome::Nothing);
    }

    #[test]
    fn a_link_names_its_host() {
        assert_eq!(
            url_host("https://github.com/o/r/pull/1"),
            Some("github.com")
        );
        assert_eq!(
            url_host("https://ghe.example.com?x"),
            Some("ghe.example.com")
        );
        assert_eq!(url_host("not a link"), None);
    }
}

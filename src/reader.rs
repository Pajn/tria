//! The reader: something read in place of the thread's conversation, from being asked for
//! to being put away. It is either a subagent's transcript or one of the thread's pull
//! requests, and the conversation is kept underneath it and put back on the way out.
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
    model::PullRequestRef,
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
            Kind::Transcript { agent_id, .. } => Source::Subagent(agent_id.clone()),
            Kind::PullRequest { detail, .. } => Source::PullRequest(detail.url.clone()),
        })
    }

    /// What was read, as a thread, which is the conversation on screen while it is open.
    pub fn conversation(&self) -> Option<&ThreadState> {
        self.open.as_ref().map(|open| &open.state)
    }

    /// The subagent being read, when it is one.
    pub fn agent_id(&self) -> Option<&str> {
        match &self.open.as_ref()?.kind {
            Kind::Transcript { agent_id, .. } => Some(agent_id),
            Kind::PullRequest { .. } => None,
        }
    }

    /// The pull request being read, when it is one.
    pub fn pull_request(&self) -> Option<&Detail> {
        match &self.open.as_ref()?.kind {
            Kind::PullRequest { detail, .. } => Some(detail),
            Kind::Transcript { .. } => None,
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
            Kind::Transcript { agent_id, .. } => {
                format!("Sent looking at the transcript for subagent {agent_id}:")
            }
            Kind::PullRequest { detail, .. } => format!(
                "Sent looking at pull request {}#{} ({}):",
                detail.repository, detail.number, detail.url
            ),
        })
    }

    /// What it is, in a sentence about it.
    pub fn noun(&self) -> Option<&'static str> {
        Some(match self.open.as_ref()?.kind {
            Kind::Transcript { .. } => "the subagent",
            Kind::PullRequest { .. } => "the pull request",
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
            Kind::Transcript { .. } => {
                timeline::build(&open.state, expanded, open_levels, size.0, size.1)
            }
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
            for other in self.asked.values_mut() {
                other.intent = Intent::Quiet;
            }
        }
        self.asked.insert(source, asked);
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

//! The outbox: what is written, from being written to being taken, or given back. A
//! message goes to a thread that exists, or makes one; it can be held back until the turn
//! running is over; and until the server has taken it, it is still ours, so a refusal can
//! put it back where it was written. A rewind takes messages back out of a thread, and
//! they come here to be written again. What is written and not yet sent is parked here
//! too, one draft per thread, while another thread is open.
//!
//! It sends nothing of its own. Asked to send, queue, rewind, or build a plan, or told what
//! the thread list now says, what the open thread now holds, or that time has passed, it
//! says what commands to send; told what the server answered, it says what to give back,
//! where to, and where the view is to go. The app does the sending, the writing into the
//! composer, and the moving. So which message goes when, which refusal is whose, and what
//! is written over is all decided here, and can be tried without a server.
//!
//! What was written is given back as it was written, never over anything written since:
//! into the composer it belongs to if that is empty, and otherwise into the composer's
//! history, where `Ctrl-p` finds it, with a toast saying so. A rewind is the one
//! exception: what it gives back joins whatever is being written, since it is being
//! handed back to be written again rather than put back after a failure.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use crate::{
    commands::{self, PullRequestLink},
    model::{Id, LatestTurn, ModelSelection, ThreadShell},
    state::{Rewind, ThreadState},
};

/// The draft key for a thread that does not exist yet.
pub const NEW_THREAD_DRAFT_KEY: &str = "\0new-thread";

/// The activity the server writes into a thread when a rewind did not happen.
const REWIND_FAILED: &str = "checkpoint.revert.failed";

/// How long a rewind is waited for before its messages are given back anyway. A
/// Claude rewind reads and forks the session's history, which is not instant.
pub const REWIND_TIMEOUT: Duration = Duration::from_secs(120);

/// A thread being composed that does not exist on the server yet.
#[derive(Debug, Clone)]
pub struct NewThreadDraft {
    /// Reserved while drafting, so terminals can belong to the thread before it is sent.
    pub thread_id: Id,
    pub project_id: Id,
    pub model_selection: ModelSelection,
    pub runtime_mode: String,
    pub interaction_mode: String,
    /// Start the thread in a fresh worktree rather than the project's own checkout.
    pub worktree: bool,
    /// A pull request checked out for the thread to start in, where it was made for one.
    pub checkout: Option<PreparedCheckout>,
}

impl NewThreadDraft {
    pub fn id(&self) -> &str {
        self.checkout
            .as_ref()
            .map_or(self.thread_id.as_str(), |checkout| {
                checkout.thread_id.as_str()
            })
    }
}

/// A pull request checked out by the server for a thread to be written in.
#[derive(Debug, Clone)]
pub struct PreparedCheckout {
    /// The thread's id, chosen before it exists so the server could set the worktree up
    /// for it.
    pub thread_id: Id,
    pub number: u64,
    pub branch: String,
    pub worktree_path: String,
    /// False where a worktree that was already there kept changes or commits of its own,
    /// so what the thread starts in is not the pull request's latest.
    pub on_head: bool,
    /// The link the thread gets once it exists, where one could be made.
    pub link: Option<PullRequestLink>,
}

/// Which command an answer is for. Handed out with each command, and handed back with
/// what the server said to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ticket(u64);

/// Something for the app to send the server, and to hand the answer to back here.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// A turn on a thread that exists, with `text` as the agent gets it.
    Start {
        ticket: Ticket,
        thread: Id,
        text: String,
        model_selection: ModelSelection,
        runtime_mode: String,
        interaction_mode: String,
        /// The plan the turn builds, where it builds one.
        implementing: Option<Implementing>,
    },
    /// A thread made for a message, and the message as its first turn.
    Create {
        ticket: Ticket,
        thread: Id,
        text: String,
        new: NewThread,
        /// The plan the thread is made to build, where it is made for one.
        implementing: Option<Implementing>,
    },
    /// A pull request put on a thread made for it.
    Link {
        ticket: Ticket,
        thread: Id,
        link: PullRequestLink,
    },
    /// Drop the thread's turns after the first `turn_count`, and with `restore_files` put
    /// the files back as that turn left them.
    Revert {
        ticket: Ticket,
        thread: Id,
        turn_count: u32,
        restore_files: bool,
    },
}

impl Command {
    pub fn ticket(&self) -> Ticket {
        match self {
            Command::Start { ticket, .. }
            | Command::Create { ticket, .. }
            | Command::Link { ticket, .. }
            | Command::Revert { ticket, .. } => *ticket,
        }
    }
}

/// A proposed plan a turn builds: the thread it was proposed in, and which it is. The
/// server marks it built once it has the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Implementing {
    pub thread: Id,
    pub plan: Id,
}

/// What a thread being made starts as.
#[derive(Debug, Clone, PartialEq)]
pub struct NewThread {
    pub project_id: Id,
    pub title: String,
    pub model_selection: ModelSelection,
    pub runtime_mode: String,
    pub interaction_mode: String,
    /// A fresh worktree for it, where it asked for one.
    pub worktree: Option<Worktree>,
    /// The branch and checkout it works in, where that is not the project's own: a pull
    /// request checked out for it, or the thread whose plan it builds.
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
}

/// Where a new thread's worktree branches from: the project's checkout, and the branch
/// that checkout has now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub project_cwd: String,
    pub base_branch: String,
    /// Fetch and start from `origin/<base branch>` rather than the local one.
    pub start_from_origin: bool,
}

/// What the server said to a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub ticket: Ticket,
    pub result: Result<(), String>,
}

/// Text for the composer on screen, or for its history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GiveBack {
    /// The thread it was written in.
    pub thread: Id,
    pub text: String,
    pub slot: Slot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// The composer on screen, which is the thread's and is empty.
    Composer,
    /// The composer on screen, which is the thread's, to write again: `text` is what a
    /// rewind took back, after whatever was already being written there.
    Rewound,
    /// The composer's history, since wherever it would have gone has something else in it.
    /// A message sent as it was written is there already, from when it was sent.
    History,
}

/// Where the view is to go.
#[derive(Debug, Clone)]
pub enum Go {
    /// To a thread being made, which reads as loading until it exists.
    Opening(Id),
    /// Subscribe to a thread the server has just made, which is still the one on screen.
    Made(Id),
    /// Open a thread the server has just made, from the one it was asked from.
    Open(Id),
    /// Back to the draft a thread the server would not make was written in.
    Draft(Box<NewThreadDraft>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Toast {
    Say(String),
    Warn(String),
}

/// What the app is to do after a call: what to send, what to give back, where to go,
/// and what to say.
#[must_use]
#[derive(Debug, Default)]
pub struct Step {
    pub commands: Vec<Command>,
    pub give_backs: Vec<GiveBack>,
    pub go: Option<Go>,
    pub toast: Option<Toast>,
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

    /// This, and then that: the commands and give-backs of both, and whatever the later
    /// says and does.
    fn then(mut self, next: Step) -> Self {
        self.commands.extend(next.commands);
        self.give_backs.extend(next.give_backs);
        self.go = next.go.or(self.go);
        self.toast = next.toast.or(self.toast);
        self
    }
}

/// Where things stand on screen, lent for one call: what a give-back would write over.
pub struct Here<'a> {
    /// The thread the view is on, which is the one the composer belongs to. A thread
    /// being made is one; a draft of one not yet sent is not.
    pub thread: Option<&'a str>,
    /// What the composer on screen holds.
    pub composer: &'a str,
}

impl Here<'_> {
    fn composer_empty(&self) -> bool {
        self.composer.trim().is_empty()
    }
}

/// What it says when a give-back went to the history.
const IN_HISTORY: &str = "it is in the composer's history (Ctrl-p)";

/// A message written during a turn and held back until that turn is over, where
/// `Enter` would have steered the turn with it. It is tria's, not the server's: the
/// protocol has no queue, and a message the server has been given cannot be taken
/// back, so this is the only kind that can be.
struct Queued {
    /// What was written, which is what comes back.
    text: String,
    /// The same as the agent gets it, with what each part was written over.
    sent: String,
    /// The turn that was running when it was queued.
    after: Option<Waited>,
}

/// The turn a queued message waits for.
struct Waited {
    turn: Id,
    /// When it was asked for, to tell a list entry about an older turn from one about
    /// a newer. Empty where the server did not say.
    requested_at: String,
    /// Whether the thread list has shown this turn, after which any other turn it shows
    /// is one that came after it.
    seen: bool,
}

/// A thread asked for and not yet made. Kept whole so a refusal can put back what was
/// typed: the message is otherwise gone, and the view is left on a thread that will
/// never exist.
struct Creating {
    draft: NewThreadDraft,
    text: String,
}

/// A rewind the server has been asked for. The command is taken before the work is
/// done, so what says it happened is the messages going, and what says it did not is a
/// failure the server writes into the thread.
struct Rewinding {
    rewind: Rewind,
    /// Failures already in the thread when it was asked for, which are not this one's.
    failures_seen: HashSet<Id>,
    deadline: Instant,
}

/// A command on its way, by what its answer is about.
enum Asked {
    /// A message to a thread that exists, as it was written, to give back if refused.
    Send {
        thread: Id,
        text: String,
    },
    Create {
        thread: Id,
    },
    Link,
    Revert {
        thread: Id,
    },
    /// A plan built where it was proposed. Nobody wrote it, so there is nothing to give
    /// back.
    Plan,
    /// A thread made to build a plan, and the thread it was asked from.
    PlanThread {
        thread: Id,
        from: Id,
    },
}

/// The messages on their way, the ones held back, the threads being made for them, the
/// rewinds that will give messages back, and what is parked for each thread not open.
#[derive(Default)]
pub struct Outbox {
    next: u64,
    /// Messages waiting for their thread's turn to end, one per thread: a second one
    /// queued behind the first joins it, as one message is what the turn gets next.
    queued: HashMap<Id, Queued>,
    /// Threads being made, by the id each was given when its message was sent, so that
    /// any number can be on their way and each answer finds its own.
    creating: HashMap<Id, Creating>,
    /// Rewinds on their way, one per thread.
    rewinding: HashMap<Id, Rewinding>,
    /// Unsent composer text per thread, keyed by thread id, so switching threads keeps a
    /// half-written message where it belongs. New-thread drafts use `NEW_THREAD_DRAFT_KEY`.
    drafts: HashMap<String, String>,
    asked: HashMap<Ticket, Asked>,
}

impl Outbox {
    // ── Sending ────────────────────────────────────────────────────────

    /// Send a message to a thread that exists, keeping hold of what was written until
    /// the server has taken it. The composer is emptied when a message goes, because
    /// that is what sending looks like; a message the server refuses has to come back,
    /// or the only copy of it is one keypress of history away and nothing says so.
    pub fn send(&mut self, thread: &ThreadShell, text: &str, context: Option<&str>) -> Step {
        let sent = over_reading(context, text);
        self.start(thread, text.to_string(), sent)
    }

    fn start(&mut self, thread: &ThreadShell, text: String, sent: String) -> Step {
        let ticket = self.ticket(Asked::Send {
            thread: thread.id.clone(),
            text,
        });
        Step {
            commands: vec![Command::Start {
                ticket,
                thread: thread.id.clone(),
                text: sent,
                model_selection: thread.model_selection.clone(),
                runtime_mode: thread.runtime_mode.clone(),
                interaction_mode: thread.interaction_mode.clone(),
                implementing: None,
            }],
            ..Step::default()
        }
    }

    /// Make a thread for a message, in the draft it was written in. The view moves to
    /// the new thread now, and reads as loading until the server has made it and the
    /// subscription has something to say.
    pub fn create(
        &mut self,
        draft: NewThreadDraft,
        text: &str,
        worktree: Option<Worktree>,
    ) -> Step {
        let checkout = draft.checkout.as_ref();
        // Keep the identity used by terminals and any prepared pull-request checkout.
        let thread = draft.id().to_string();
        // Named for its first line with anything in it, trimmed before it is cut short.
        let title: String = text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("New thread")
            .chars()
            .take(60)
            .collect();
        let new = NewThread {
            project_id: draft.project_id.clone(),
            title: title.trim_end().to_string(),
            model_selection: draft.model_selection.clone(),
            runtime_mode: draft.runtime_mode.clone(),
            interaction_mode: draft.interaction_mode.clone(),
            worktree,
            branch: checkout.map(|c| c.branch.clone()),
            worktree_path: checkout.map(|c| c.worktree_path.clone()),
        };
        let ticket = self.ticket(Asked::Create {
            thread: thread.clone(),
        });
        self.creating.insert(
            thread.clone(),
            Creating {
                draft,
                text: text.to_string(),
            },
        );
        Step {
            commands: vec![Command::Create {
                ticket,
                thread: thread.clone(),
                text: text.to_string(),
                new,
                implementing: None,
            }],
            go: Some(Go::Opening(thread)),
            ..Step::default()
        }
    }

    /// `:implement`: build the thread's plan here, leaving plan mode for it; with
    /// `new`, in a thread of its own that works where this one does. Either way the
    /// server is told which plan it is, and marks it built.
    pub fn implement(&mut self, thread: &ThreadState, new: bool) -> Step {
        if thread.is_running() {
            return Step::warn("the thread is still working; implement once it is done");
        }
        let Some(plan) = thread.actionable_plan() else {
            return Step::warn("no plan waiting to be built");
        };
        let source = thread.id().to_string();
        // It would land in the turns the rewind is about to drop.
        if !new && self.is_rewinding(&source) {
            return Step::warn("wait for the rewind to finish");
        }
        let text = format!(
            "{}{}",
            commands::PLAN_PROMPT_PREFIX,
            plan.plan_markdown.trim()
        );
        let shell = &thread.detail.shell;
        let implementing = Some(Implementing {
            thread: source.clone(),
            plan: plan.id.clone(),
        });
        if !new {
            let ticket = self.ticket(Asked::Plan);
            return Step {
                commands: vec![Command::Start {
                    ticket,
                    thread: source,
                    text,
                    model_selection: shell.model_selection.clone(),
                    runtime_mode: shell.runtime_mode.clone(),
                    interaction_mode: "default".into(),
                    implementing,
                }],
                ..Step::default()
            };
        }
        let title: String = plan_title(&plan.plan_markdown)
            .map_or_else(
                || "Implement plan".into(),
                |title| format!("Implement {title}"),
            )
            .chars()
            .take(60)
            .collect();
        let made = commands::new_id();
        let new = NewThread {
            project_id: shell.project_id.clone(),
            title: title.trim().to_string(),
            model_selection: shell.model_selection.clone(),
            runtime_mode: shell.runtime_mode.clone(),
            interaction_mode: "default".into(),
            worktree: None,
            branch: shell.branch.clone(),
            worktree_path: shell.worktree_path.clone(),
        };
        // The view stays on the plan until the thread building it exists: moving first
        // would be looking at a thread that may never be made.
        let ticket = self.ticket(Asked::PlanThread {
            thread: made.clone(),
            from: source,
        });
        Step {
            commands: vec![Command::Create {
                ticket,
                thread: made,
                text,
                new,
                implementing,
            }],
            ..Step::say("starting a thread for the plan…")
        }
    }

    // ── Holding back ───────────────────────────────────────────────────

    /// Hold the message until the thread's running turn is over, instead of steering
    /// the turn with it as `Enter` does. What `context` says goes out with it, and what
    /// was written is what comes back.
    pub fn queue(&mut self, thread: &ThreadShell, text: &str, context: Option<&str>) {
        let waited = thread
            .latest_turn
            .as_ref()
            .filter(|turn| turn.state == "running")
            .map(|turn| Waited {
                turn: turn.turn_id.clone(),
                requested_at: turn.requested_at.clone(),
                seen: false,
            });
        let queued = self.queued.entry(thread.id.clone()).or_insert(Queued {
            text: String::new(),
            sent: String::new(),
            after: None,
        });
        if !queued.text.is_empty() {
            queued.text.push_str("\n\n");
            queued.sent.push_str("\n\n");
        }
        queued.text.push_str(text);
        queued.sent.push_str(&over_reading(context, text));
        // It now waits for the turn running now; what the list has shown of that turn
        // still counts if it is the one it was already waiting for.
        let same = queued
            .after
            .as_ref()
            .zip(waited.as_ref())
            .is_some_and(|(was, now)| was.turn == now.turn);
        if !same {
            queued.after = waited;
        }
    }

    /// What a thread has queued, as it was written, for the composer to show.
    pub fn queued(&self, thread: &str) -> Option<&str> {
        self.queued.get(thread).map(|queued| queued.text.as_str())
    }

    /// Take a thread's queued message back, as it was written, to be edited or thrown
    /// away. Nothing is sent for it until it is sent again.
    pub fn recall(&mut self, thread: &str) -> Option<String> {
        self.queued.remove(thread).map(|queued| queued.text)
    }

    // ── Drafts ─────────────────────────────────────────────────────────

    /// Park what the composer holds for the slot being left, `leaving`, and hand back
    /// what is parked for `target`, the slot being opened, to be written on. A composer
    /// holding nothing but blanks parks nothing, and takes away whatever was parked.
    pub fn swap(&mut self, leaving: Option<&str>, composer: &str, target: &str) -> Option<&str> {
        if let Some(key) = leaving {
            if composer.trim().is_empty() {
                self.drafts.remove(key);
            } else {
                self.drafts.insert(key.to_string(), composer.to_string());
            }
        }
        self.drafts.get(target).map(String::as_str)
    }

    /// What is parked for a slot.
    pub fn parked(&self, key: &str) -> Option<&str> {
        self.drafts.get(key).map(String::as_str)
    }

    /// Nothing is parked for a slot any more: what was written there has been sent.
    pub fn unpark(&mut self, key: &str) {
        self.drafts.remove(key);
    }

    fn draft_free(&self, thread: &str) -> bool {
        self.drafts
            .get(thread)
            .is_none_or(|text| text.trim().is_empty())
    }

    // ── Rewinding ──────────────────────────────────────────────────────

    /// Whether a thread has a rewind on its way.
    pub fn is_rewinding(&self, thread: &str) -> bool {
        self.rewinding.contains_key(thread)
    }

    /// Rewind the open thread as worked out and agreed to, with `restore_files` putting
    /// the files back as well. What the rewind drops comes back once it has been through.
    pub fn rewind(
        &mut self,
        thread: &ThreadState,
        rewind: Rewind,
        restore_files: bool,
        now: Instant,
    ) -> Step {
        let id = thread.id().to_string();
        if self.is_rewinding(&id) {
            return Step::warn("a rewind is already on its way");
        }
        let failures_seen = thread
            .detail
            .activities
            .iter()
            .filter(|a| a.kind == REWIND_FAILED)
            .map(|a| a.id.clone())
            .collect();
        let ticket = self.ticket(Asked::Revert { thread: id.clone() });
        let command = Command::Revert {
            ticket,
            thread: id.clone(),
            turn_count: rewind.turn_count,
            restore_files,
        };
        self.rewinding.insert(
            id,
            Rewinding {
                rewind,
                failures_seen,
                deadline: now + REWIND_TIMEOUT,
            },
        );
        Step {
            commands: vec![command],
            ..Step::say("rewinding…")
        }
    }

    /// See whether the rewind on its way in the open thread has been through. The
    /// messages it drops going is what it looks like when it has, and they come back to
    /// be written again; a failure the server wrote into the thread since is what it
    /// looks like when it has not. Only a thread's own detail says either, so a thread
    /// left while its rewind is on its way is seen through once it is open again, or
    /// at the deadline.
    pub fn on_thread(&mut self, thread: &ThreadState, here: &Here) -> Step {
        let Some(pending) = self.rewinding.get(thread.id()) else {
            return Step::default();
        };
        let failure = thread
            .detail
            .activities
            .iter()
            .rev()
            .find(|a| a.kind == REWIND_FAILED && !pending.failures_seen.contains(&a.id))
            .map(|a| a.str("detail").unwrap_or(&a.summary).to_string());
        if let Some(detail) = failure {
            self.rewinding.remove(thread.id());
            return Step::warn(format!("not rewound: {detail}"));
        }
        let gone = pending
            .rewind
            .message_ids
            .iter()
            .all(|id| !thread.detail.messages.iter().any(|m| &m.id == id));
        if !gone {
            return Step::default();
        }
        self.rewound(thread.id(), here)
    }

    /// Time has passed. A rewind nobody has heard of by its deadline gives its messages
    /// back anyway: a copy too many is better than none.
    pub fn tick(&mut self, now: Instant, here: &Here) -> Step {
        let mut due: Vec<Id> = self
            .rewinding
            .iter()
            .filter(|(_, pending)| now >= pending.deadline)
            .map(|(thread, _)| thread.clone())
            .collect();
        due.sort();
        let mut step = Step::default();
        let mut toasts = Vec::new();
        for thread in due {
            let Some(pending) = self.rewinding.remove(&thread) else {
                continue;
            };
            let open = self.give_rewound(&thread, pending.rewind.text, here, &mut step);
            let text = if open {
                "no word of the rewind from the server · what you sent is back in the composer"
            } else {
                "no word of the rewind from the server · what you sent is back in that thread"
            };
            toasts.push((open, text));
        }
        // One toast is shown, so it is the one about the thread on screen, where there is
        // one; otherwise the last by thread id, so the same rewinds always say the same.
        step.toast = toasts
            .iter()
            .find(|(open, _)| *open)
            .or(toasts.last())
            .map(|(_, text)| Toast::Warn((*text).to_string()));
        step
    }

    /// The rewind on its way in a thread has been through: what it dropped comes back to
    /// be written again.
    fn rewound(&mut self, thread: &str, here: &Here) -> Step {
        let Some(pending) = self.rewinding.remove(thread) else {
            return Step::default();
        };
        let mut step = Step::default();
        let open = self.give_rewound(thread, pending.rewind.text, here, &mut step);
        step.toast = Some(Toast::Say(if open {
            "rewound · what you sent that turn is back to edit".into()
        } else {
            "rewound · what you sent that turn is back in that thread".into()
        }));
        step
    }

    /// Put text a rewind took out of the thread back where it can be written again,
    /// after whatever is already there: the composer when the thread is open, its
    /// parked draft when it is not. Says whether it was the composer.
    fn give_rewound(&mut self, thread: &str, text: String, here: &Here, step: &mut Step) -> bool {
        let join = |existing: &str| {
            if existing.trim().is_empty() {
                text.clone()
            } else {
                format!("{}\n\n{text}", existing.trim_end())
            }
        };
        if here.thread == Some(thread) {
            step.give_backs.push(GiveBack {
                thread: thread.to_string(),
                text: join(here.composer),
                slot: Slot::Rewound,
            });
            return true;
        }
        let joined = join(self.parked(thread).unwrap_or_default());
        self.drafts.insert(thread.to_string(), joined);
        false
    }

    // ── The thread list ────────────────────────────────────────────────

    /// The thread list has news of a thread: send what it has queued once the turn it
    /// waits for is over. A turn that completed is what it was waiting for. One that was interrupted or failed is not:
    /// the message was written for a turn that did not get where it was going, so it
    /// comes back to be read again rather than going out on its own.
    ///
    /// The turn waited for is over once the thread is idle and the list's latest turn is
    /// that turn or a later one — one another client started after it ended, or one that
    /// came and went while tria was not connected — and how that latest turn ended
    /// decides it either way. A later turn is one asked for after the one waited for, or,
    /// where the server did not say when either was asked for, any other turn once the
    /// list has shown the one waited for. Any other entry is from before the turn waited
    /// for began, and says nothing about it.
    pub fn on_shell(&mut self, thread: &ThreadShell, here: &Here) -> Step {
        let Some(queued) = self.queued.get_mut(&thread.id) else {
            return Step::default();
        };
        let turn = thread.latest_turn.as_ref();
        if let (Some(waited), Some(turn)) = (queued.after.as_mut(), turn)
            && turn.turn_id == waited.turn
        {
            waited.seen = true;
        }
        if thread.is_running() || !waited_is_over(queued.after.as_ref(), turn) {
            return Step::default();
        }
        let Some(queued) = self.queued.remove(&thread.id) else {
            return Step::default();
        };
        match turn.map(|t| t.state.as_str()) {
            Some("completed") => self.start(thread, queued.text, queued.sent),
            state => {
                let why = match state {
                    Some("interrupted") => "the turn was interrupted",
                    Some("error") => "the turn failed",
                    _ => "the turn did not finish",
                };
                self.not_sent(&thread.id, queued.text, why, here)
            }
        }
    }

    /// A thread is gone from the server, and what was being written for it has nowhere
    /// to go. What it had queued goes to the composer's history, and the toast says so,
    /// rather than vanishing; what was parked for it, and a rewind on its way there, go
    /// with it.
    pub fn on_removed(&mut self, thread: &str) -> Step {
        self.drafts.remove(thread);
        self.rewinding.remove(thread);
        let Some(queued) = self.queued.remove(thread) else {
            return Step::default();
        };
        Step {
            give_backs: vec![GiveBack {
                thread: thread.to_string(),
                text: queued.text,
                slot: Slot::History,
            }],
            ..Step::warn(format!(
                "a removed thread's queued message was dropped · {IN_HISTORY}"
            ))
        }
    }

    // ── Answers ────────────────────────────────────────────────────────

    /// The server answered a command.
    pub fn on_answer(&mut self, answer: Answer, here: &Here) -> Step {
        let Some(asked) = self.asked.remove(&answer.ticket) else {
            return Step::default();
        };
        match (asked, answer.result) {
            (Asked::Send { .. }, Ok(())) => Step::default(),
            (Asked::Send { thread, text }, Err(error)) => {
                self.not_sent(&thread, text, &error, here)
            }
            (Asked::Create { thread }, Ok(())) => self.on_created(thread, here),
            (Asked::Create { thread }, Err(error)) => self.on_create_refused(thread, error, here),
            // The server would not take the rewind at all.
            (Asked::Revert { thread }, Err(error)) => match self.rewinding.remove(&thread) {
                Some(_) => Step::warn(format!("not rewound: {error}")),
                None => Step::default(),
            },
            // A thread made to build a plan is where to look, from where it was asked.
            (Asked::PlanThread { thread, from }, Ok(())) => Step {
                go: (here.thread == Some(from.as_str())).then_some(Go::Open(thread)),
                ..Step::default()
            },
            (Asked::Link | Asked::Revert { .. } | Asked::Plan, Ok(())) => Step::default(),
            (Asked::Link | Asked::Plan | Asked::PlanThread { .. }, Err(error)) => {
                Step::warn(format!("command failed: {error}"))
            }
        }
    }

    /// Link what the thread was made for, now there is a thread to put it on, and
    /// subscribe to it unless the view has moved on in the meantime: a thread opened
    /// since the message was sent is the one wanted.
    fn on_created(&mut self, thread: Id, here: &Here) -> Step {
        let link = self
            .creating
            .remove(&thread)
            .and_then(|creating| creating.draft.checkout)
            .and_then(|checkout| checkout.link);
        let mut step = Step::default();
        if let Some(link) = link {
            let ticket = self.ticket(Asked::Link);
            step.commands.push(Command::Link {
                ticket,
                thread: thread.clone(),
                link,
            });
        }
        if here.thread == Some(thread.as_str()) {
            step.go = Some(Go::Made(thread));
        }
        step
    }

    /// The server would not make the thread. The view is on one that does not exist and
    /// the message has left the composer, so both go back: what was typed is the work,
    /// and it is the only copy. Staying on the draft also keeps the view still, rather
    /// than falling through to whichever thread happens to be first in the list. Unless
    /// the view has moved on by itself, in which case it is where it is meant to be and
    /// the message is in the composer's history.
    fn on_create_refused(&mut self, thread: Id, error: String, here: &Here) -> Step {
        let Some(creating) = self.creating.remove(&thread) else {
            return Step::warn(error);
        };
        if here.thread != Some(thread.as_str()) {
            return history(&thread, creating.text, &error);
        }
        let back = if here.composer_empty() {
            Step {
                give_backs: vec![GiveBack {
                    thread,
                    text: creating.text,
                    slot: Slot::Composer,
                }],
                ..Step::warn(error)
            }
        } else {
            history(&thread, creating.text, &error)
        };
        Step {
            go: Some(Go::Draft(Box::new(creating.draft))),
            ..Step::default()
        }
        .then(back)
    }

    /// A message that was not sent goes back where it was written: into the composer if
    /// that is still where you are and nothing has been written since, and into the
    /// thread's parked draft if you have gone elsewhere and it has none. Where neither is
    /// free, what was typed is not lost — it goes to the composer's history — and the
    /// toast says so rather than writing over whatever took its place.
    fn not_sent(&mut self, thread: &str, text: String, why: &str, here: &Here) -> Step {
        let lead = format!("not sent: {why}");
        let here_now = here.thread == Some(thread);
        if here_now && here.composer_empty() {
            return Step {
                give_backs: vec![GiveBack {
                    thread: thread.to_string(),
                    text,
                    slot: Slot::Composer,
                }],
                ..Step::warn(lead)
            };
        }
        if !here_now && self.draft_free(thread) {
            self.drafts.insert(thread.to_string(), text);
            return Step::warn(format!("{lead} · the message is back in that thread"));
        }
        history(thread, text, &lead)
    }

    fn ticket(&mut self, asked: Asked) -> Ticket {
        self.next += 1;
        let ticket = Ticket(self.next);
        self.asked.insert(ticket, asked);
        ticket
    }
}

/// Whether the turn a queued message waits for is over, by what the list shows as the
/// thread's latest turn. See `Outbox::on_shell`.
fn waited_is_over(waited: Option<&Waited>, latest: Option<&LatestTurn>) -> bool {
    let Some(waited) = waited else {
        return true;
    };
    match latest {
        Some(turn) if turn.turn_id == waited.turn => true,
        // The server writes both as the same kind of timestamp, which sorts as text.
        Some(turn) => {
            waited.seen
                || (!waited.requested_at.is_empty()
                    && !turn.requested_at.is_empty()
                    && turn.requested_at > waited.requested_at)
        }
        // A list entry with no turn at all says nothing of the one waited for.
        None => false,
    }
}

/// Into the composer's history, and said so after `lead`.
fn history(thread: &str, text: String, lead: &str) -> Step {
    Step {
        give_backs: vec![GiveBack {
            thread: thread.to_string(),
            text,
            slot: Slot::History,
        }],
        ..Step::warn(format!("{lead} · {IN_HISTORY}"))
    }
}

/// The message as the agent gets it: with what it was written over, when it was written
/// over a reading, so a "this" or a "why did it" has something to point at.
fn over_reading(context: Option<&str>, text: &str) -> String {
    match context {
        Some(context) => format!("{context}\n\n{text}"),
        None => text.to_string(),
    }
}

/// What a plan calls itself: its first heading.
fn plan_title(markdown: &str) -> Option<&str> {
    markdown
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('#'))
        .map(|line| line.trim_start_matches('#').trim())
        .filter(|title| !title.is_empty())
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;

    use super::*;

    const EARLIER: &str = "2026-01-01T00:00:00Z";
    const LATER: &str = "2026-01-01T00:05:00Z";

    /// What is on screen, as the outbox is lent it.
    struct Screen {
        thread: Option<String>,
        composer: String,
    }

    impl Screen {
        /// On `thread`, with nothing in the composer and nothing parked.
        fn on(thread: &str) -> Self {
            Self {
                thread: Some(thread.into()),
                composer: String::new(),
            }
        }

        fn here(&self) -> Here<'_> {
            Here {
                thread: self.thread.as_deref(),
                composer: &self.composer,
            }
        }
    }

    fn selection() -> ModelSelection {
        ModelSelection {
            instance_id: "instance".into(),
            model: "a-model".into(),
            options: vec![],
        }
    }

    /// A thread as the list has it: on `turn` in `state`, asked for at `at`, with its
    /// session running or not.
    fn listed(turn: Option<(&str, &str, &str)>, session: Option<&str>) -> ThreadShell {
        serde_json::from_value(json!({
            "id": "t1", "projectId": "p", "title": "t1",
            "modelSelection": {"instanceId": "instance", "model": "a-model"},
            "runtimeMode": "approval-required", "interactionMode": "plan",
            "session": session.map(|status| json!({"status": status})),
            "latestTurn": turn.map(|(id, state, at)| {
                let mut turn = json!({"turnId": id, "state": state});
                if !at.is_empty() {
                    turn["requestedAt"] = json!(at);
                }
                turn
            }),
        }))
        .expect("a thread the server could send")
    }

    /// `t1` working on `turn`, asked for at `at`.
    fn working(at: &str) -> ThreadShell {
        listed(Some(("turn", "running", at)), Some("running"))
    }

    fn on(turn: &str, state: &str, at: &str) -> ThreadShell {
        listed(Some((turn, state, at)), None)
    }

    fn draft() -> NewThreadDraft {
        NewThreadDraft {
            thread_id: commands::new_id(),
            project_id: "p".into(),
            model_selection: selection(),
            runtime_mode: "full-access".into(),
            interaction_mode: "default".into(),
            worktree: false,
            checkout: None,
        }
    }

    fn checked_out() -> NewThreadDraft {
        NewThreadDraft {
            checkout: Some(PreparedCheckout {
                thread_id: "made-for-it".into(),
                number: 1,
                branch: "a".into(),
                worktree_path: "/src/p-worktrees/a".into(),
                on_head: true,
                link: PullRequestLink::from_url("https://github.com/o/r/pull/1"),
            }),
            ..draft()
        }
    }

    fn only_command(step: &Step) -> &Command {
        match step.commands.as_slice() {
            [command] => command,
            commands => panic!("one command, not {commands:?}"),
        }
    }

    /// The turn the step starts: its thread, and its text as the agent gets it.
    fn started(step: &Step) -> (Id, String) {
        match only_command(step) {
            Command::Start { thread, text, .. } => (thread.clone(), text.clone()),
            other => panic!("not a turn on a thread: {other:?}"),
        }
    }

    /// The thread the step makes.
    fn creating(step: &Step) -> Id {
        match only_command(step) {
            Command::Create { thread, .. } => thread.clone(),
            other => panic!("not a thread made: {other:?}"),
        }
    }

    fn refused(step: &Step, error: &str) -> Answer {
        Answer {
            ticket: only_command(step).ticket(),
            result: Err(error.into()),
        }
    }

    fn taken(step: &Step) -> Answer {
        Answer {
            ticket: only_command(step).ticket(),
            result: Ok(()),
        }
    }

    fn back(thread: &str, text: &str, slot: Slot) -> Vec<GiveBack> {
        vec![GiveBack {
            thread: thread.into(),
            text: text.into(),
            slot,
        }]
    }

    fn warned(step: &Step) -> &str {
        match &step.toast {
            Some(Toast::Warn(text)) => text,
            other => panic!("no warning: {other:?}"),
        }
    }

    fn said(step: &Step) -> &str {
        match &step.toast {
            Some(Toast::Say(text)) => text,
            other => panic!("nothing said: {other:?}"),
        }
    }

    // ── Sending ────────────────────────────────────────────────────────

    /// A message goes as a turn on the thread, the way the thread is set up, saying what
    /// it was written over; refused, what comes back is what was written.
    #[test]
    fn a_message_goes_saying_what_it_was_written_over_and_comes_back_as_written() {
        let mut outbox = Outbox::default();
        let context = "Sent looking at the transcript for subagent a1:";
        let step = outbox.send(&on("turn", "completed", ""), "why?", Some(context));
        match only_command(&step) {
            Command::Start {
                thread,
                text,
                model_selection,
                runtime_mode,
                interaction_mode,
                ..
            } => {
                assert_eq!(thread, "t1");
                assert_eq!(text, &format!("{context}\n\nwhy?"));
                assert_eq!(model_selection, &selection());
                assert_eq!(runtime_mode, "approval-required");
                assert_eq!(interaction_mode, "plan");
            }
            other => panic!("not a turn on a thread: {other:?}"),
        }

        let screen = Screen::on("t1");
        let answer = outbox.on_answer(refused(&step, "refused"), &screen.here());
        assert_eq!(answer.give_backs, back("t1", "why?", Slot::Composer));
        assert_eq!(warned(&answer), "not sent: refused");
    }

    /// Where the composer is not free, putting the message back would write over
    /// something else somebody typed. It goes to the thread it was meant for if that
    /// draft is free, and otherwise it stays in the history and the toast says so.
    #[test]
    fn a_refused_message_does_not_write_over_what_took_its_place() {
        let refuse = |outbox: &mut Outbox, screen: &Screen| {
            let step = outbox.send(&on("turn", "completed", ""), "the message", None);
            outbox.on_answer(refused(&step, "refused"), &screen.here())
        };

        let mut screen = Screen::on("t1");
        screen.composer = "the next one".into();
        let step = refuse(&mut Outbox::default(), &screen);
        assert_eq!(step.give_backs, back("t1", "the message", Slot::History));
        assert_eq!(
            warned(&step),
            "not sent: refused · it is in the composer's history (Ctrl-p)"
        );

        let mut screen = Screen::on("t2");
        screen.composer = "the next one".into();
        let mut outbox = Outbox::default();
        let step = refuse(&mut outbox, &screen);
        assert!(step.give_backs.is_empty(), "the composer is t2's");
        assert_eq!(outbox.parked("t1"), Some("the message"));
        assert_eq!(
            warned(&step),
            "not sent: refused · the message is back in that thread"
        );

        let mut outbox = Outbox::default();
        let _ = outbox.swap(Some("t1"), "parked", "t2");
        let step = refuse(&mut outbox, &screen);
        assert_eq!(step.give_backs, back("t1", "the message", Slot::History));
        assert_eq!(outbox.parked("t1"), Some("parked"));
    }

    /// A message the server has taken is the server's, and nothing more is heard of it.
    #[test]
    fn a_message_taken_is_let_go() {
        let mut outbox = Outbox::default();
        let step = outbox.send(&on("turn", "completed", ""), "the message", None);
        let screen = Screen::on("t1");
        let answer = outbox.on_answer(taken(&step), &screen.here());
        assert!(answer.give_backs.is_empty() && answer.toast.is_none());
        let late = outbox.on_answer(refused(&step, "refused"), &screen.here());
        assert!(late.give_backs.is_empty() && late.toast.is_none());
    }

    // ── Making threads ─────────────────────────────────────────────────

    /// A message written in a draft makes its thread, named for its first line, in the
    /// worktree it asked for, and the view goes to it.
    #[test]
    fn a_draft_makes_its_thread_and_the_view_goes_to_it() {
        let mut outbox = Outbox::default();
        let worktree = Worktree {
            project_cwd: "/src/p".into(),
            base_branch: "main".into(),
            start_from_origin: true,
        };
        let step = outbox.create(draft(), "Fix it\nplease", Some(worktree.clone()));
        let Command::Create {
            thread, text, new, ..
        } = only_command(&step)
        else {
            panic!("not a thread made");
        };
        assert_eq!(text, "Fix it\nplease");
        assert_eq!(new.title, "Fix it");
        assert_eq!(new.project_id, "p");
        assert_eq!(new.worktree, Some(worktree));
        assert!(new.branch.is_none() && new.worktree_path.is_none());
        assert!(matches!(&step.go, Some(Go::Opening(id)) if id == thread));

        // Named for its first line with anything in it, whatever it starts with.
        let named = |outbox: &mut Outbox, text: &str| {
            let step = outbox.create(draft(), text, None);
            match only_command(&step) {
                Command::Create { new, .. } => new.title.clone(),
                _ => panic!("not a thread made"),
            }
        };
        assert_eq!(named(&mut outbox, "\n  Fix it  \nplease"), "Fix it");
        let long = format!("{}{}", " ".repeat(70), "a".repeat(80));
        assert_eq!(named(&mut outbox, &long), "a".repeat(60));
        assert_eq!(named(&mut outbox, "   \n"), "New thread");

        // One made for a pull request checked out is the thread it was checked out for,
        // where it was checked out.
        let step = outbox.create(checked_out(), "review this", None);
        let Command::Create { thread, new, .. } = only_command(&step) else {
            panic!("not a thread made");
        };
        assert_eq!(thread, "made-for-it");
        assert_eq!(new.branch.as_deref(), Some("a"));
        assert_eq!(new.worktree_path.as_deref(), Some("/src/p-worktrees/a"));
    }

    /// A thread made is subscribed to only if it is still the one on screen: one opened
    /// since the message was sent is the one wanted.
    #[test]
    fn a_made_thread_is_opened_only_while_it_is_on_screen() {
        let mut outbox = Outbox::default();
        let step = outbox.create(draft(), "one", None);
        let thread = creating(&step);
        let answer = outbox.on_answer(taken(&step), &Screen::on(&thread).here());
        assert!(matches!(&answer.go, Some(Go::Made(id)) if id == &thread));

        let step = outbox.create(draft(), "two", None);
        let answer = outbox.on_answer(taken(&step), &Screen::on("elsewhere").here());
        assert!(answer.go.is_none());
    }

    /// Refused, a thread takes the view back to the draft it was written in, with the
    /// message back in the composer.
    #[test]
    fn a_thread_the_server_refuses_goes_back_to_its_draft() {
        let mut outbox = Outbox::default();
        let step = outbox.create(draft(), "the message", None);
        let thread = creating(&step);
        let screen = Screen::on(&thread);
        let answer = outbox.on_answer(refused(&step, "git worktree add failed"), &screen.here());
        assert!(matches!(&answer.go, Some(Go::Draft(d)) if d.project_id == "p"));
        assert_eq!(
            answer.give_backs,
            back(&thread, "the message", Slot::Composer)
        );
        assert_eq!(warned(&answer), "git worktree add failed");
    }

    /// Anything typed while the thread was on its way stays where it is, and the message
    /// is in the history; a view that has moved on stays where it went.
    #[test]
    fn a_refused_thread_does_not_write_over_what_was_typed_since() {
        let mut outbox = Outbox::default();
        let step = outbox.create(draft(), "the message", None);
        let thread = creating(&step);
        let mut screen = Screen::on(&thread);
        screen.composer = "typed since".into();
        let answer = outbox.on_answer(refused(&step, "no"), &screen.here());
        assert!(matches!(&answer.go, Some(Go::Draft(_))));
        assert_eq!(
            answer.give_backs,
            back(&thread, "the message", Slot::History)
        );
        assert_eq!(
            warned(&answer),
            "no · it is in the composer's history (Ctrl-p)"
        );

        let step = outbox.create(draft(), "another", None);
        let thread = creating(&step);
        let answer = outbox.on_answer(refused(&step, "no"), &Screen::on("t1").here());
        assert!(answer.go.is_none(), "the view went somewhere of its own");
        assert_eq!(answer.give_backs, back(&thread, "another", Slot::History));
    }

    /// Two threads on their way at once each keep what their own refusal puts back, and
    /// one being made says nothing about the other.
    #[test]
    fn two_threads_on_their_way_each_keep_their_own() {
        let mut outbox = Outbox::default();
        let first = outbox.create(draft(), "first", None);
        let second = outbox.create(draft(), "second", None);
        let (a, b) = (creating(&first), creating(&second));
        assert_ne!(a, b);

        let answer = outbox.on_answer(taken(&first), &Screen::on(&b).here());
        assert!(answer.go.is_none(), "the view is on the other one");
        let answer = outbox.on_answer(refused(&second, "no"), &Screen::on(&b).here());
        assert!(matches!(&answer.go, Some(Go::Draft(_))));
        assert_eq!(answer.give_backs, back(&b, "second", Slot::Composer));

        // And the other way round: the later refused first.
        let first = outbox.create(draft(), "first", None);
        let second = outbox.create(draft(), "second", None);
        let (a, b) = (creating(&first), creating(&second));
        let _ = outbox.on_answer(refused(&second, "no"), &Screen::on("t1").here());
        let answer = outbox.on_answer(refused(&first, "no"), &Screen::on(&a).here());
        assert!(matches!(&answer.go, Some(Go::Draft(_))));
        assert_eq!(answer.give_backs, back(&a, "first", Slot::Composer));
        assert!(!outbox.creating.contains_key(&b));
    }

    /// The pull request a thread was checked out for goes on it once it is made, and not
    /// at all when it is not; a link refused says so.
    #[test]
    fn a_pull_request_goes_on_the_thread_made_for_it() {
        let mut outbox = Outbox::default();
        let step = outbox.create(checked_out(), "review this", None);
        let screen = Screen::on("made-for-it");
        let made = outbox.on_answer(taken(&step), &screen.here());
        let Command::Link { thread, link, .. } = only_command(&made) else {
            panic!("not linked");
        };
        assert_eq!(thread, "made-for-it");
        assert_eq!(link.number, 1);
        let answer = outbox.on_answer(refused(&made, "gone"), &screen.here());
        assert_eq!(warned(&answer), "command failed: gone");

        let step = outbox.create(checked_out(), "review this", None);
        let _ = outbox.on_answer(refused(&step, "no"), &screen.here());
        assert!(outbox.creating.is_empty(), "a link kept for nothing");
        assert!(outbox.asked.is_empty());
    }

    // ── Holding back ───────────────────────────────────────────────────

    /// `Ctrl-s` holds the message until the turn has finished, and only then is it sent
    /// — as the next turn, not into this one — with what each part was written over.
    #[test]
    fn a_queued_message_waits_for_the_turn_to_finish() {
        let mut outbox = Outbox::default();
        let screen = Screen::on("t1");
        outbox.queue(&working(EARLIER), "first", Some("Over it:"));
        outbox.queue(&working(EARLIER), "second", None);
        assert_eq!(outbox.queued("t1"), Some("first\n\nsecond"));

        // A list entry from before the turn began says nothing about this one.
        let step = outbox.on_shell(&on("an-earlier-turn", "completed", EARLIER), &screen.here());
        assert!(step.commands.is_empty(), "sent behind a turn long over");
        let step = outbox.on_shell(&working(EARLIER), &screen.here());
        assert!(step.commands.is_empty(), "sent into the running turn");

        let step = outbox.on_shell(&on("turn", "completed", EARLIER), &screen.here());
        assert_eq!(
            started(&step),
            ("t1".into(), "Over it:\n\nfirst\n\nsecond".into())
        );
        assert_eq!(outbox.queued("t1"), None);
    }

    /// A turn stopped part way is not the one the message was written to follow, so the
    /// message comes back rather than going out behind it — as it was written, without
    /// what it was written over, which is only for the agent.
    #[test]
    fn a_queued_message_comes_back_as_it_was_written() {
        let mut outbox = Outbox::default();
        let screen = Screen::on("t1");
        outbox.queue(&working(""), "after that", Some("Over it:"));
        let step = outbox.on_shell(&on("turn", "interrupted", ""), &screen.here());
        assert!(step.commands.is_empty());
        assert_eq!(step.give_backs, back("t1", "after that", Slot::Composer));
        assert_eq!(warned(&step), "not sent: the turn was interrupted");

        outbox.queue(&working(""), "after that", Some("Over it:"));
        let step = outbox.on_shell(&on("turn", "error", ""), &screen.here());
        assert_eq!(step.give_backs, back("t1", "after that", Slot::Composer));
        assert_eq!(warned(&step), "not sent: the turn failed");

        // Taken back with `Up`, it is the same.
        outbox.queue(&working(""), "not yet", Some("Over it:"));
        assert_eq!(outbox.queued("t1"), Some("not yet"));
        assert_eq!(outbox.recall("t1").as_deref(), Some("not yet"));
        let step = outbox.on_shell(&on("turn", "completed", ""), &screen.here());
        assert!(step.commands.is_empty(), "a recalled message went out");
    }

    /// Given back where the composer is taken, the whole of what was queued is what the
    /// history gets, so `Ctrl-p` brings back the message the toast is about.
    #[test]
    fn a_queued_message_in_the_history_is_the_whole_of_it() {
        let mut outbox = Outbox::default();
        let mut screen = Screen::on("t1");
        screen.composer = "busy".into();
        outbox.queue(&working(""), "first", None);
        outbox.queue(&working(""), "second", Some("Over it:"));
        let step = outbox.on_shell(&on("turn", "interrupted", ""), &screen.here());
        assert_eq!(
            step.give_backs,
            back("t1", "first\n\nsecond", Slot::History)
        );
        assert_eq!(
            warned(&step),
            "not sent: the turn was interrupted · it is in the composer's history (Ctrl-p)"
        );
    }

    /// The turn waited for can end unheard, and another turn start and end after it: from
    /// another client, or while tria was not connected. That later turn is past the one
    /// waited for, so the message is not stuck behind it for good.
    #[test]
    fn a_queued_message_is_not_stuck_behind_a_turn_that_came_after() {
        let screen = Screen::on("t1");

        // The list showed the turn waited for, so any other it shows came after.
        let mut outbox = Outbox::default();
        outbox.queue(&working(""), "next", None);
        let _ = outbox.on_shell(&working(""), &screen.here());
        let step = outbox.on_shell(&on("someone-elses", "completed", ""), &screen.here());
        assert_eq!(started(&step), ("t1".into(), "next".into()));

        // Never shown it, a turn asked for after it came after it.
        let mut outbox = Outbox::default();
        outbox.queue(&working(EARLIER), "next", None);
        let step = outbox.on_shell(&on("after-a-reconnect", "running", LATER), &screen.here());
        assert!(step.commands.is_empty(), "that one is still running");
        let step = outbox.on_shell(&on("after-a-reconnect", "completed", LATER), &screen.here());
        assert_eq!(started(&step), ("t1".into(), "next".into()));

        // A list entry with no turn says nothing, even once the turn has been shown.
        let mut outbox = Outbox::default();
        outbox.queue(&working(""), "next", None);
        let _ = outbox.on_shell(&working(""), &screen.here());
        let step = outbox.on_shell(&listed(None, None), &screen.here());
        assert!(step.commands.is_empty() && step.give_backs.is_empty());
        assert_eq!(outbox.queued("t1"), Some("next"));

        // How that turn ended decides it, as it would have for the one waited for.
        let mut outbox = Outbox::default();
        outbox.queue(&working(EARLIER), "next", None);
        let step = outbox.on_shell(
            &on("after-a-reconnect", "interrupted", LATER),
            &screen.here(),
        );
        assert_eq!(step.give_backs, back("t1", "next", Slot::Composer));
    }

    /// Released and then refused, a queued message comes back as it was written, like
    /// any refused message.
    #[test]
    fn a_queued_message_refused_on_its_way_comes_back() {
        let mut outbox = Outbox::default();
        let screen = Screen::on("elsewhere");
        outbox.queue(&working(""), "after that", Some("Over it:"));
        let step = outbox.on_shell(&on("turn", "completed", ""), &screen.here());
        assert_eq!(started(&step).1, "Over it:\n\nafter that");
        let answer = outbox.on_answer(refused(&step, "refused"), &screen.here());
        assert!(answer.give_backs.is_empty());
        assert_eq!(outbox.parked("t1"), Some("after that"));
    }

    /// A thread removed has nowhere for its queued message to go: it is dropped out loud,
    /// and left in the history.
    #[test]
    fn a_removed_thread_drops_its_queued_message_out_loud() {
        let mut outbox = Outbox::default();
        outbox.queue(&working(""), "after that", Some("Over it:"));
        let step = outbox.on_removed("t1");
        assert_eq!(step.give_backs, back("t1", "after that", Slot::History));
        assert_eq!(
            warned(&step),
            "a removed thread's queued message was dropped · it is in the composer's history (Ctrl-p)"
        );
        assert_eq!(outbox.queued("t1"), None);
        let again = outbox.on_removed("t1");
        assert!(again.give_backs.is_empty() && again.toast.is_none());
    }
    /// A thread in plan mode on a worktree, with a plan waiting and an older one built.
    pub(crate) fn planned_thread() -> ThreadState {
        let snapshot: crate::model::ThreadDetailSnapshot = serde_json::from_value(json!({
            "snapshotSequence": 1,
            "thread": {
                "id": "t1", "projectId": "p", "title": "Test",
                "modelSelection": {"instanceId": "instance", "model": "a-model"},
                "runtimeMode": "full-access", "interactionMode": "plan",
                "branch": "tria/abc", "worktreePath": "/worktrees/p-1",
                "latestTurn": {"turnId": "turn", "state": "completed"},
                "hasActionableProposedPlan": true,
                "messages": [], "activities": [],
                "proposedPlans": [
                    {"id": "old", "planMarkdown": "# Old", "implementedAt": "2026-01-01T09:00:00Z",
                        "createdAt": "2026-01-01T08:00:00Z"},
                    {"id": "plan", "planMarkdown": "\n# Add a queue\n\n1. do it\n",
                        "createdAt": "2026-01-01T10:00:00Z"},
                ]
            }
        }))
        .expect("a plan the server could have sent");
        ThreadState::from_snapshot(snapshot)
    }

    /// Three finished turns, the second of them steered: `u3` arrived while it ran.
    pub(crate) fn rewindable_thread() -> ThreadState {
        let message = |id: &str, role: &str, turn: Option<&str>, at: &str| {
            json!({"id": id, "role": role, "text": id, "turnId": turn,
                "createdAt": format!("2026-01-01T10:{at}:00Z")})
        };
        let checkpoint = |turn: &str, count: u32, at: &str| {
            json!({"turnId": turn, "checkpointTurnCount": count,
                "completedAt": format!("2026-01-01T10:{at}:00Z")})
        };
        let snapshot: crate::model::ThreadDetailSnapshot = serde_json::from_value(json!({
            "snapshotSequence": 1,
            "thread": {
                "id": "t1", "projectId": "p", "title": "Test",
                "modelSelection": {"instanceId": "instance", "model": "a-model"},
                "latestTurn": {"turnId": "three", "state": "completed"},
                "messages": [
                    message("u1", "user", None, "00"),
                    message("a1", "assistant", Some("one"), "01"),
                    message("u2", "user", None, "03"),
                    message("a2", "assistant", Some("two"), "04"),
                    message("u3", "user", None, "05"),
                    message("a3", "assistant", Some("two"), "06"),
                    message("u4", "user", None, "08"),
                    message("a4", "assistant", Some("three"), "09"),
                ],
                "checkpoints": [
                    checkpoint("one", 1, "02"),
                    checkpoint("two", 2, "07"),
                    checkpoint("three", 3, "10"),
                ],
                "activities": []
            }
        }))
        .expect("a thread the server could have sent");
        ThreadState::from_snapshot(snapshot)
    }

    /// `t1` as the list has it, saying when its latest message of yours was sent.
    fn latest_message_at(at: Option<&str>) -> ThreadShell {
        let mut thread = on("three", "completed", "");
        thread.latest_user_message_at = at.map(str::to_string);
        thread
    }

    /// The thread the server writes a failed rewind into.
    fn failed(thread: &mut ThreadState, id: &str) {
        let activity: crate::model::Activity = serde_json::from_value(json!({
            "id": id, "kind": "checkpoint.revert.failed", "summary": "Checkpoint revert failed",
            "payload": {"detail": "no checkpoint"}, "createdAt": "",
        }))
        .unwrap();
        thread.detail.activities.push(activity);
    }

    /// `thread` rewound to before `u3`, which drops `u2` and `u3`.
    fn rewinding(outbox: &mut Outbox, thread: &ThreadState, now: Instant) -> Step {
        let rewind = thread.rewind_before("u3").expect("a rewind");
        outbox.rewind(thread, rewind, false, now)
    }

    fn with_id(mut thread: ThreadState, id: &str) -> ThreadState {
        thread.detail.shell.id = id.into();
        thread
    }

    // ── Rewinding ──────────────────────────────────────────────────────

    /// Sent, and once the messages are gone they are back in the composer, after what
    /// was already being written there.
    #[test]
    fn a_rewind_gives_back_what_it_took_once_it_is_through() {
        let mut outbox = Outbox::default();
        let mut thread = rewindable_thread();
        let step = rewinding(&mut outbox, &thread, Instant::now());
        match only_command(&step) {
            Command::Revert {
                thread,
                turn_count,
                restore_files,
                ..
            } => assert_eq!(
                (thread.as_str(), *turn_count, *restore_files),
                ("t1", 1, false)
            ),
            other => panic!("not a rewind: {other:?}"),
        }
        assert_eq!(said(&step), "rewinding…");
        assert!(outbox.is_rewinding("t1"));

        let mut screen = Screen::on("t1");
        screen.composer = "half written".into();
        let step = outbox.on_thread(&thread, &screen.here());
        assert!(step.give_backs.is_empty(), "back before it has gone");
        thread
            .detail
            .messages
            .retain(|m| m.id == "u1" || m.id == "a1");
        let step = outbox.on_thread(&thread, &screen.here());
        assert_eq!(
            step.give_backs,
            back("t1", "half written\n\nu2\n\nu3", Slot::Rewound)
        );
        assert_eq!(
            said(&step),
            "rewound · what you sent that turn is back to edit"
        );
        assert!(!outbox.is_rewinding("t1"));
    }

    /// The server takes the command before it does the work, and says it could not
    /// in the thread. That is where the reason is read from, and a failure already
    /// there before is not this one's.
    #[test]
    fn a_rewind_the_server_could_not_do_says_why() {
        let mut outbox = Outbox::default();
        let mut thread = rewindable_thread();
        failed(&mut thread, "before");
        let _ = rewinding(&mut outbox, &thread, Instant::now());
        let screen = Screen::on("t1");
        let step = outbox.on_thread(&thread, &screen.here());
        assert!(step.toast.is_none(), "an old failure");

        failed(&mut thread, "this one");
        let step = outbox.on_thread(&thread, &screen.here());
        assert_eq!(warned(&step), "not rewound: no checkpoint");
        assert!(
            step.give_backs.is_empty(),
            "nothing went, so nothing comes back"
        );
        assert!(!outbox.is_rewinding("t1"));
    }

    /// Refused outright, it says so and is over.
    #[test]
    fn a_rewind_the_server_would_not_take_says_why() {
        let mut outbox = Outbox::default();
        let step = rewinding(&mut outbox, &rewindable_thread(), Instant::now());
        let answer = outbox.on_answer(refused(&step, "no"), &Screen::on("t1").here());
        assert_eq!(warned(&answer), "not rewound: no");
        assert!(!outbox.is_rewinding("t1"));
    }

    /// What the thread list says of a thread is no word of its rewind, open or not, and
    /// whatever it says of when your latest message was sent: that is the thread's own
    /// detail to say, and a rewind let go of early would lift the guard on turns that
    /// are still there.
    #[test]
    fn the_thread_list_never_sees_a_rewind_through() {
        for open in ["t1", "t2"] {
            let mut outbox = Outbox::default();
            let _ = rewinding(&mut outbox, &rewindable_thread(), Instant::now());
            let screen = Screen::on(open);
            for listed in [
                latest_message_at(Some("2026-01-01T10:00:00Z")),
                latest_message_at(None),
            ] {
                let step = outbox.on_shell(&listed, &screen.here());
                assert!(step.give_backs.is_empty() && step.toast.is_none());
                assert!(outbox.is_rewinding("t1"), "let go of with {open} open");
            }
            assert_eq!(outbox.parked("t1"), None);
        }
    }

    /// A thread left while its rewind is on its way is seen through once it is open
    /// again and its detail shows the messages gone, back in the composer after what was
    /// parked there and is written there again.
    #[test]
    fn a_rewind_is_seen_through_when_its_thread_is_open_again() {
        let mut outbox = Outbox::default();
        let mut thread = rewindable_thread();
        let _ = rewinding(&mut outbox, &thread, Instant::now());
        let _ = outbox.swap(Some("t1"), "half written", "t2");
        let other = with_id(rewindable_thread(), "t2");
        let step = outbox.on_thread(&other, &Screen::on("t2").here());
        assert!(step.toast.is_none(), "another thread's detail");

        let parked = outbox.swap(Some("t2"), "", "t1").map(str::to_string);
        let mut screen = Screen::on("t1");
        screen.composer = parked.expect("its draft");
        let step = outbox.on_thread(&thread, &screen.here());
        assert!(step.toast.is_none(), "not gone yet");
        thread
            .detail
            .messages
            .retain(|m| m.id == "u1" || m.id == "a1");
        let step = outbox.on_thread(&thread, &screen.here());
        assert_eq!(
            step.give_backs,
            back("t1", "half written\n\nu2\n\nu3", Slot::Rewound)
        );
        assert_eq!(
            said(&step),
            "rewound · what you sent that turn is back to edit"
        );
        assert!(!outbox.is_rewinding("t1"));
    }

    /// One thread's rewind is that thread's: another can rewind meanwhile, and the same
    /// one cannot twice.
    #[test]
    fn threads_rewind_each_on_their_own() {
        let mut outbox = Outbox::default();
        let now = Instant::now();
        let _ = rewinding(&mut outbox, &rewindable_thread(), now);
        let other = with_id(rewindable_thread(), "t2");
        let step = rewinding(&mut outbox, &other, now);
        assert!(matches!(only_command(&step), Command::Revert { thread, .. } if thread == "t2"));
        assert!(outbox.is_rewinding("t1") && outbox.is_rewinding("t2"));

        let again = rewinding(&mut outbox, &rewindable_thread(), now);
        assert!(again.commands.is_empty());
        assert_eq!(warned(&again), "a rewind is already on its way");
    }

    /// With no word by the deadline, what went is given back anyway: a copy too many is
    /// better than none.
    #[test]
    fn a_rewind_unheard_of_gives_back_at_its_deadline() {
        let mut outbox = Outbox::default();
        let start = Instant::now();
        let _ = rewinding(&mut outbox, &rewindable_thread(), start);
        let _ = rewinding(&mut outbox, &with_id(rewindable_thread(), "t2"), start);
        let screen = Screen::on("t1");
        let step = outbox.tick(start + Duration::from_secs(60), &screen.here());
        assert!(step.give_backs.is_empty() && step.toast.is_none());

        let step = outbox.tick(start + REWIND_TIMEOUT, &screen.here());
        assert_eq!(step.give_backs, back("t1", "u2\n\nu3", Slot::Rewound));
        assert_eq!(outbox.parked("t2"), Some("u2\n\nu3"));
        assert_eq!(
            warned(&step),
            "no word of the rewind from the server · what you sent is back in the composer",
            "said of the thread on screen"
        );
        assert!(!outbox.is_rewinding("t1") && !outbox.is_rewinding("t2"));
    }

    // ── Plans ──────────────────────────────────────────────────────────

    /// `:implement` sends the plan back out of plan mode, naming it, so the server can
    /// mark it built.
    #[test]
    fn implement_builds_the_waiting_plan_here() {
        let mut outbox = Outbox::default();
        let step = outbox.implement(&planned_thread(), false);
        match only_command(&step) {
            Command::Start {
                thread,
                text,
                interaction_mode,
                runtime_mode,
                implementing,
                ..
            } => {
                assert_eq!(thread, "t1");
                assert_eq!(
                    text,
                    "PLEASE IMPLEMENT THIS PLAN:\n# Add a queue\n\n1. do it"
                );
                assert_eq!(interaction_mode, "default");
                assert_eq!(runtime_mode, "full-access");
                assert_eq!(
                    implementing,
                    &Some(Implementing {
                        thread: "t1".into(),
                        plan: "plan".into()
                    })
                );
            }
            other => panic!("not a turn: {other:?}"),
        }
        assert!(step.toast.is_none() && step.go.is_none());
        let answer = outbox.on_answer(refused(&step, "no"), &Screen::on("t1").here());
        assert_eq!(warned(&answer), "command failed: no");
        assert!(answer.give_backs.is_empty(), "nobody wrote it");
    }

    /// `:implement new` makes a thread where this one works, named for the plan, and
    /// moves there only once the server has made it.
    #[test]
    fn implement_new_builds_it_in_a_thread_of_its_own() {
        let mut outbox = Outbox::default();
        let step = outbox.implement(&planned_thread(), true);
        let Command::Create {
            thread,
            new,
            implementing,
            ..
        } = only_command(&step)
        else {
            panic!("not a thread made");
        };
        assert_ne!(thread, "t1");
        assert_eq!(new.title, "Implement Add a queue");
        assert_eq!(new.interaction_mode, "default");
        assert_eq!(new.branch.as_deref(), Some("tria/abc"));
        assert_eq!(new.worktree_path.as_deref(), Some("/worktrees/p-1"));
        assert!(new.worktree.is_none());
        assert_eq!(implementing.as_ref().map(|i| i.plan.as_str()), Some("plan"));
        assert!(step.go.is_none(), "moved before it exists");
        assert_eq!(said(&step), "starting a thread for the plan…");

        let answer = outbox.on_answer(taken(&step), &Screen::on("t1").here());
        assert!(matches!(&answer.go, Some(Go::Open(id)) if id == thread));
    }

    /// Made once the view has moved on, the thread building the plan is left for later
    /// rather than pulling the view away from where it went.
    #[test]
    fn a_plan_thread_opens_only_from_where_it_was_asked() {
        let mut outbox = Outbox::default();
        let step = outbox.implement(&planned_thread(), true);
        let answer = outbox.on_answer(taken(&step), &Screen::on("t2").here());
        assert!(answer.go.is_none(), "pulled away from t2");
    }

    /// A built plan is not built again, and a thread still working builds nothing.
    #[test]
    fn implement_wants_a_plan_nobody_has_built_and_a_thread_at_rest() {
        let mut outbox = Outbox::default();
        let mut thread = planned_thread();
        thread.detail.proposed_plans.retain(|p| p.id == "old");
        let step = outbox.implement(&thread, false);
        assert!(step.commands.is_empty());
        assert_eq!(warned(&step), "no plan waiting to be built");

        let mut thread = planned_thread();
        thread.detail.shell.session =
            Some(serde_json::from_value(json!({"status": "running"})).unwrap());
        let step = outbox.implement(&thread, false);
        assert!(step.commands.is_empty());
        assert!(warned(&step).contains("still working"));
    }

    /// A plan built here would land in the turns a rewind on its way is about to drop.
    #[test]
    fn implement_here_waits_for_the_rewind() {
        let mut outbox = Outbox::default();
        let mut thread = planned_thread();
        let rewindable = rewindable_thread();
        thread.detail.messages = rewindable.detail.messages;
        thread.detail.checkpoints = rewindable.detail.checkpoints;
        let _ = rewinding(&mut outbox, &thread, Instant::now());
        let step = outbox.implement(&thread, false);
        assert!(step.commands.is_empty());
        assert_eq!(warned(&step), "wait for the rewind to finish");
    }

    // ── Drafts ─────────────────────────────────────────────────────────

    /// What is half written waits in the thread it was written in: leaving parks it,
    /// coming back hands it back, a thread never written in has nothing, and blanks are
    /// not worth keeping.
    #[test]
    fn a_draft_is_parked_per_thread() {
        let mut outbox = Outbox::default();
        assert_eq!(
            outbox.swap(None, "", "t1"),
            None,
            "nothing to leave, nothing there"
        );
        assert_eq!(
            outbox.swap(Some("t1"), "half written", "t2"),
            None,
            "t2 starts empty"
        );
        assert_eq!(outbox.swap(Some("t2"), "  \n ", "t1"), Some("half written"));
        assert_eq!(outbox.parked("t2"), None, "blanks parked");

        // Written over with blanks, what was parked goes.
        assert_eq!(outbox.swap(Some("t1"), "   ", "t2"), None);
        assert_eq!(outbox.parked("t1"), None);

        let _ = outbox.swap(Some(NEW_THREAD_DRAFT_KEY), "a new one", "t1");
        assert_eq!(outbox.parked(NEW_THREAD_DRAFT_KEY), Some("a new one"));
        outbox.unpark(NEW_THREAD_DRAFT_KEY);
        assert_eq!(outbox.parked(NEW_THREAD_DRAFT_KEY), None);

        let _ = outbox.swap(Some("t1"), "gone with it", "t2");
        let _ = outbox.on_removed("t1");
        assert_eq!(outbox.parked("t1"), None, "a draft for nothing");
    }
}

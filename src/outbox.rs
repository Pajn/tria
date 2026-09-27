//! The outbox: a message from being sent to being taken, or given back. A message goes to
//! a thread that exists, or makes one; it can be held back until the turn running is
//! over; and until the server has taken it, it is still ours, so a refusal can put it
//! back where it was written.
//!
//! It sends nothing of its own. Asked to send, queue, or told what the thread list now
//! says, it says what commands to send; told what the server answered, it says what to
//! give back, where to, and where the view is to go. The app does the sending, the
//! writing into the composer, and the moving. So which message goes when, which refusal
//! is whose, and what is written over is all decided here, and can be tried without a
//! server.
//!
//! What was written is given back as it was written, never over anything written since:
//! into the composer it belongs to if that is empty, and otherwise into the composer's
//! history, where `Ctrl-p` finds it, with a toast saying so.

use std::collections::HashMap;

use crate::{
    app::NewThreadDraft,
    commands::{self, PullRequestLink},
    model::{Id, LatestTurn, ModelSelection, ThreadShell},
};

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
    },
    /// A thread made for a message, and the message as its first turn.
    Create {
        ticket: Ticket,
        thread: Id,
        text: String,
        new: NewThread,
    },
    /// A pull request put on a thread made for it.
    Link {
        ticket: Ticket,
        thread: Id,
        link: PullRequestLink,
    },
}

impl Command {
    pub fn ticket(&self) -> Ticket {
        match self {
            Command::Start { ticket, .. }
            | Command::Create { ticket, .. }
            | Command::Link { ticket, .. } => *ticket,
        }
    }
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
    /// The branch and checkout of a pull request checked out for it, to work where that
    /// is. Without them the thread works in the project's own checkout.
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

/// Text that did not go, and where it goes back to.
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
    /// The thread's parked draft, which it is not open to and is empty.
    Draft,
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
    /// Back to the draft a thread the server would not make was written in.
    Draft(Box<NewThreadDraft>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Toast {
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
    pub composer_empty: bool,
    /// The text parked for each thread not open, by its id.
    pub drafts: &'a HashMap<String, String>,
}

impl Here<'_> {
    fn draft_free(&self, thread: &str) -> bool {
        self.drafts
            .get(thread)
            .is_none_or(|text| text.trim().is_empty())
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
}

/// The messages on their way, the ones held back, and the threads being made for them.
#[derive(Default)]
pub struct Outbox {
    next: u64,
    /// Messages waiting for their thread's turn to end, one per thread: a second one
    /// queued behind the first joins it, as one message is what the turn gets next.
    queued: HashMap<Id, Queued>,
    /// Threads being made, by the id each was given when its message was sent, so that
    /// any number can be on their way and each answer finds its own.
    creating: HashMap<Id, Creating>,
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
        // A pull request checked out for the thread was checked out for this id.
        let thread = checkout.map_or_else(commands::new_id, |c| c.thread_id.clone());
        let title: String = text
            .lines()
            .next()
            .unwrap_or("New thread")
            .chars()
            .take(60)
            .collect();
        let new = NewThread {
            project_id: draft.project_id.clone(),
            title: title.trim().to_string(),
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
            }],
            go: Some(Go::Opening(thread)),
            ..Step::default()
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

    /// The thread list has news of a thread: send what it has queued once the turn it
    /// waits for is over. A turn that completed is what it was waiting for. One that was
    /// interrupted or failed is not: the message was written for a turn that did not
    /// get where it was going, so it comes back to be read again rather than going out
    /// on its own.
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
                not_sent(&thread.id, queued.text, why, here)
            }
        }
    }

    /// A thread is gone from the server, and what it had queued has nowhere to go. It
    /// goes to the composer's history, and the toast says so, rather than vanishing.
    pub fn on_removed(&mut self, thread: &str) -> Step {
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
            (Asked::Send { thread, text }, Err(error)) => not_sent(&thread, text, &error, here),
            (Asked::Create { thread }, Ok(())) => self.on_created(thread, here),
            (Asked::Create { thread }, Err(error)) => self.on_create_refused(thread, error, here),
            (Asked::Link, Ok(())) => Step::default(),
            (Asked::Link, Err(error)) => Step::warn(format!("command failed: {error}")),
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
        let back = if here.composer_empty {
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

/// A message that was not sent goes back where it was written: into the composer if
/// that is still where you are and nothing has been written since, and into the
/// thread's parked draft if you have gone elsewhere and it has none. Where neither is
/// free, what was typed is not lost — it goes to the composer's history — and the toast
/// says so rather than writing over whatever took its place.
fn not_sent(thread: &str, text: String, why: &str, here: &Here) -> Step {
    let lead = format!("not sent: {why}");
    let here_now = here.thread == Some(thread);
    let (slot, toast) = if here_now && here.composer_empty {
        (Slot::Composer, lead)
    } else if !here_now && here.draft_free(thread) {
        (
            Slot::Draft,
            format!("{lead} · the message is back in that thread"),
        )
    } else {
        return history(thread, text, &lead);
    };
    Step {
        give_backs: vec![GiveBack {
            thread: thread.to_string(),
            text,
            slot,
        }],
        ..Step::warn(toast)
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::app::PreparedCheckout;

    const EARLIER: &str = "2026-01-01T00:00:00Z";
    const LATER: &str = "2026-01-01T00:05:00Z";

    /// What is on screen, as the outbox is lent it.
    struct Screen {
        thread: Option<String>,
        composer: String,
        drafts: HashMap<String, String>,
    }

    impl Screen {
        /// On `thread`, with nothing in the composer and nothing parked.
        fn on(thread: &str) -> Self {
            Self {
                thread: Some(thread.into()),
                composer: String::new(),
                drafts: HashMap::new(),
            }
        }

        fn here(&self) -> Here<'_> {
            Here {
                thread: self.thread.as_deref(),
                composer_empty: self.composer.trim().is_empty(),
                drafts: &self.drafts,
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
            None => panic!("nothing said"),
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
        let refuse = |screen: &Screen| {
            let mut outbox = Outbox::default();
            let step = outbox.send(&on("turn", "completed", ""), "the message", None);
            outbox.on_answer(refused(&step, "refused"), &screen.here())
        };

        let mut screen = Screen::on("t1");
        screen.composer = "the next one".into();
        let step = refuse(&screen);
        assert_eq!(step.give_backs, back("t1", "the message", Slot::History));
        assert_eq!(
            warned(&step),
            "not sent: refused · it is in the composer's history (Ctrl-p)"
        );

        let mut screen = Screen::on("t2");
        screen.composer = "the next one".into();
        let step = refuse(&screen);
        assert_eq!(step.give_backs, back("t1", "the message", Slot::Draft));
        assert_eq!(
            warned(&step),
            "not sent: refused · the message is back in that thread"
        );

        screen.drafts.insert("t1".into(), "parked".into());
        let step = refuse(&screen);
        assert_eq!(step.give_backs, back("t1", "the message", Slot::History));
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
        assert_eq!(answer.give_backs, back("t1", "after that", Slot::Draft));
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
}

//! Wire types for the subset of the T3 Code contracts a chat client needs.
//! Decoding is lenient on purpose: unknown fields are ignored, optional fields
//! default, and open string unions stay `String`.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type Id = String;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelection {
    pub instance_id: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<OptionSelection>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionSelection {
    pub id: String,
    pub value: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: Id,
    pub title: String,
    pub workspace_root: String,
    /// What somebody chose to draw the project as.
    #[serde(default)]
    pub project_icon: Option<ProjectIcon>,
    #[serde(default)]
    pub default_model_selection: Option<ModelSelection>,
    /// Where new threads for this project start: `worktree` or `local`. Unset means
    /// the server-wide setting decides.
    #[serde(default)]
    pub default_thread_env_mode: Option<String>,
}

/// The icon a project was given: a character, or a name and a colour from the drawing
/// set both this and the desktop app draw from. A kind neither of those is no icon at
/// all, since nothing here knows what it would look like.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ProjectIcon {
    Emoji {
        emoji: String,
    },
    Lucide {
        name: String,
        #[serde(default)]
        color: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

impl Project {
    /// The emoji the project is drawn with, where that is what it was given.
    pub fn emoji(&self) -> Option<&str> {
        match &self.project_icon {
            Some(ProjectIcon::Emoji { emoji }) => Some(emoji),
            _ => None,
        }
    }

    /// The drawn icon the project was given, as its name and the colour to draw it in.
    pub fn lucide(&self) -> Option<(&str, Option<&str>)> {
        match &self.project_icon {
            Some(ProjectIcon::Lucide { name, color }) => Some((name, color.as_deref())),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestTurn {
    pub turn_id: Id,
    pub state: String,
    #[serde(default)]
    pub requested_at: String,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub status: String,
    #[serde(default)]
    pub provider_name: Option<String>,
    #[serde(default)]
    pub active_turn_id: Option<Id>,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanProgress {
    pub step: String,
    pub completed_steps: u32,
    pub total_steps: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadShell {
    pub id: Id,
    pub project_id: Id,
    pub title: String,
    pub model_selection: ModelSelection,
    #[serde(default = "default_runtime_mode")]
    pub runtime_mode: String,
    #[serde(default = "default_interaction_mode")]
    pub interaction_mode: String,
    #[serde(default)]
    pub branch: Option<String>,
    /// Checkout the thread works in when it runs on its own worktree.
    #[serde(default)]
    pub worktree_path: Option<String>,
    /// Pull requests linked to the thread, by the agent or by hand.
    #[serde(default)]
    pub pull_requests: Vec<PullRequest>,
    /// The pull request whose head is the thread's branch, when the server knows one.
    #[serde(default)]
    pub branch_pull_request: Option<BranchPullRequest>,
    #[serde(default)]
    pub latest_turn: Option<LatestTurn>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub archived_at: Option<String>,
    #[serde(default)]
    pub pinned_at: Option<String>,
    #[serde(default)]
    pub pin_order_key: Option<String>,
    #[serde(default)]
    pub settled_at: Option<String>,
    #[serde(default)]
    pub settled_override: Option<String>,
    #[serde(default)]
    pub unsettled_at: Option<String>,
    #[serde(default)]
    pub snoozed_until: Option<String>,
    #[serde(default)]
    pub has_actionable_proposed_plan: bool,
    /// Native background work alive after the turn settled: "working" or "monitoring".
    #[serde(default)]
    pub background_liveness: Option<String>,
    #[serde(default)]
    pub session: Option<Session>,
    #[serde(default)]
    pub latest_user_message_at: Option<String>,
    #[serde(default)]
    pub has_pending_approvals: bool,
    #[serde(default)]
    pub has_pending_user_input: bool,
    #[serde(default)]
    pub plan_progress: Option<PlanProgress>,
}

fn default_runtime_mode() -> String {
    "full-access".to_string()
}

fn default_interaction_mode() -> String {
    "default".to_string()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    /// The host the repository is on; with the repository and number, the link's identity.
    #[serde(default)]
    pub host: String,
    pub repository: String,
    pub number: u64,
    pub url: String,
    /// manual | created | agent | stack | stack-dismissed
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub linked_at: String,
    #[serde(default)]
    pub snapshot: Option<PullRequestSnapshot>,
    /// The stack the host itself keeps the pull request in, where it has such a thing.
    #[serde(default)]
    pub stack: Option<PullRequestStack>,
}

impl PullRequest {
    /// A layer taken out of a host's stack by hand stays linked, so the stack does not
    /// bring it back, and is not shown.
    fn is_visible(&self) -> bool {
        self.source != "stack-dismissed"
    }

    /// The repository as an identity: hosts and repositories compare without case.
    fn repository_key(&self) -> String {
        format!(
            "{}/{}",
            self.host.trim().to_lowercase(),
            self.repository.trim().to_lowercase()
        )
    }

    fn head_branch(&self) -> Option<&str> {
        self.snapshot.as_ref()?.head_branch.as_deref()
    }

    fn base_branch(&self) -> Option<&str> {
        self.snapshot.as_ref()?.base_branch.as_deref()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestSnapshot {
    /// open | closed | merged
    pub state: String,
    pub title: String,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default)]
    pub checks_state: Option<String>,
    #[serde(default)]
    pub review_decision: Option<String>,
    #[serde(default)]
    pub head_branch: Option<String>,
    #[serde(default)]
    pub base_branch: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// A stack as the host keeps it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestStack {
    pub id: String,
    /// Bottom to top.
    #[serde(default)]
    pub layers: Vec<PullRequestStackLayer>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestStackLayer {
    pub number: u64,
}

/// Pull requests that build on each other, each on the branch of the one below.
#[derive(Debug, Clone)]
pub struct PullRequestChain {
    /// Bottom to top.
    pub layers: Vec<PullRequestRef>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchPullRequest {
    pub repository: String,
    pub number: u64,
    pub url: String,
}

/// What the UI shows for a thread's pull request.
#[derive(Debug, Clone)]
pub struct PullRequestRef {
    pub host: String,
    pub repository: String,
    pub number: u64,
    pub url: String,
    pub head_branch: Option<String>,
    pub title: Option<String>,
    pub state: Option<String>,
    pub is_draft: bool,
    pub checks_state: Option<String>,
    pub review_decision: Option<String>,
    linked_at: String,
    updated_at: Option<String>,
}

impl PullRequestRef {
    fn from_linked(pr: &PullRequest) -> Self {
        Self {
            host: pr.host.clone(),
            repository: pr.repository.clone(),
            number: pr.number,
            url: pr.url.clone(),
            head_branch: pr.head_branch().map(str::to_string),
            linked_at: pr.linked_at.clone(),
            updated_at: pr.snapshot.as_ref().and_then(|s| s.updated_at.clone()),
            title: pr.snapshot.as_ref().map(|s| s.title.clone()),
            state: pr.snapshot.as_ref().map(|s| s.state.clone()),
            is_draft: pr.snapshot.as_ref().is_some_and(|s| s.is_draft),
            checks_state: pr.snapshot.as_ref().and_then(|s| s.checks_state.clone()),
            review_decision: pr.snapshot.as_ref().and_then(|s| s.review_decision.clone()),
        }
    }

    /// Still to land. A pull request not synced yet has no state, and is taken to be open
    /// until it says otherwise.
    pub fn is_open(&self) -> bool {
        matches!(self.state.as_deref(), None | Some("open"))
    }

    /// The facts after the title in a list of them: where it stands, its checks, its
    /// review, and its repository when the list spans more than one.
    pub fn facts(&self, with_repository: bool) -> String {
        let mut facts: Vec<String> = Vec::new();
        match self.state.as_deref() {
            Some("open") if self.is_draft => facts.push("draft".into()),
            Some(state) => facts.push(state.to_string()),
            None => {}
        }
        match self.checks_state.as_deref() {
            Some("passing") => facts.push("✓ checks passing".into()),
            Some("failing") => facts.push("✗ checks failing".into()),
            Some("pending") => facts.push("○ checks pending".into()),
            _ => {}
        }
        // The server's words for it, `changes-requested` and the like, read as words.
        if let Some(review) = self.review_decision.as_deref().filter(|r| !r.is_empty()) {
            facts.push(review.replace('-', " "));
        }
        if with_repository {
            facts.push(self.repository.clone());
        }
        facts.join(" · ")
    }

    pub fn label(&self) -> String {
        match &self.title {
            Some(title) => format!("#{} {}", self.number, title),
            None => format!("#{}", self.number),
        }
    }
}

/// One terminal session on the server, from `subscribeTerminalMetadata`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSummary {
    pub thread_id: Id,
    pub terminal_id: String,
    pub cwd: String,
    #[serde(default)]
    pub worktree_path: Option<String>,
    /// starting | running | exited | error
    pub status: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub exit_signal: Option<i32>,
    /// Whether a command is executing, as opposed to an idle shell.
    #[serde(default)]
    pub has_running_subprocess: bool,
    /// Server-computed title: the idle shell or the running command.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub updated_at: String,
}

impl TerminalSummary {
    pub fn is_live(&self) -> bool {
        matches!(self.status.as_str(), "starting" | "running")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
#[allow(clippy::large_enum_variant)]
pub enum TerminalEvent {
    Snapshot {
        terminals: Vec<TerminalSummary>,
    },
    Upsert {
        terminal: TerminalSummary,
    },
    // `rename_all` above renames the variants; these fields need their own.
    #[serde(rename_all = "camelCase")]
    Remove {
        thread_id: Id,
        terminal_id: String,
    },
    #[serde(other)]
    Unknown,
}

/// The checkout a thread works in, as `subscribeVcsStatus` reports it. The thread list
/// only carries a branch for threads the server created one for, so this is where the
/// branch comes from for everything else.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsLocal {
    pub is_repo: bool,
    /// The checked-out branch, absent on a detached head.
    pub ref_name: Option<String>,
    #[serde(default)]
    pub is_default_ref: bool,
    #[serde(default)]
    pub has_working_tree_changes: bool,
    #[serde(default)]
    pub working_tree: VcsWorkingTree,
}

/// What is uncommitted, as line counts across the changed files.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsWorkingTree {
    #[serde(default)]
    pub insertions: u32,
    #[serde(default)]
    pub deletions: u32,
    /// A row per file, which is what git would refuse to throw away. Untracked ones are
    /// in here too, a directory nothing tracks appearing as the directory; they carry no
    /// line counts, but neither does a file whose mode alone changed, so the two cannot
    /// be told apart from here and are not labelled as though they could.
    #[serde(default)]
    pub files: Vec<VcsFile>,
}

/// One uncommitted file, as the server counts it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsFile {
    pub path: String,
    #[serde(default)]
    pub insertions: u32,
    #[serde(default)]
    pub deletions: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "_tag", rename_all = "camelCase")]
pub enum VcsEvent {
    Snapshot {
        local: VcsLocal,
        remote: Option<VcsRemote>,
    },
    LocalUpdated {
        local: VcsLocal,
    },
    RemoteUpdated {
        remote: Option<VcsRemote>,
    },
    #[serde(other)]
    Unknown,
}

/// How the checkout stands against its upstream.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsRemote {
    #[serde(default)]
    pub has_upstream: bool,
    #[serde(default)]
    pub ahead_count: u32,
    #[serde(default)]
    pub behind_count: u32,
}

/// A terminal session with its scrollback, as `terminal.attach` and `terminal.open` return it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSessionSnapshot {
    pub thread_id: Id,
    pub terminal_id: String,
    pub cwd: String,
    pub status: String,
    pub pid: Option<i64>,
    /// Replayed output, escape sequences included, to feed the parser on attach.
    #[serde(default)]
    pub history: String,
    #[serde(default)]
    pub label: String,
}

/// What `terminal.attach` streams: the scrollback first, then the live pty output.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
#[allow(clippy::large_enum_variant)]
pub enum TerminalStreamEvent {
    Snapshot {
        snapshot: TerminalSessionSnapshot,
    },
    Output {
        data: String,
    },
    Restarted {
        snapshot: TerminalSessionSnapshot,
    },
    Cleared,
    #[serde(rename_all = "camelCase")]
    Exited {
        exit_code: Option<i64>,
        exit_signal: Option<i64>,
    },
    Closed,
    Error {
        message: String,
    },
    /// The server's own title for the session: the shell, or the command it is running.
    #[serde(rename_all = "camelCase")]
    Activity {
        has_running_subprocess: bool,
        label: String,
    },
    /// Anything a newer server adds.
    #[serde(other)]
    Unknown,
}

/// Sidebar status for a thread, ordered by priority (first match wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadStatus {
    Approval,
    Question,
    Working,
    Failed,
    Monitoring,
    PlanReady,
    Done,
    Idle,
}

impl ThreadStatus {
    /// How a thread in this state asks to be come back to, for a notification, where
    /// the thread is the subject: "Fix the auth redirect · needs approval".
    pub fn notice(self) -> &'static str {
        match self {
            Self::Approval => "needs approval",
            Self::Question => "asks a question",
            Self::Working => "is working",
            Self::Failed => "failed",
            Self::Monitoring => "finished, still watching",
            Self::PlanReady => "has a plan ready",
            Self::Done => "finished",
            Self::Idle => "stopped",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Question => "question",
            Self::Working => "working",
            Self::Failed => "failed",
            Self::Monitoring => "monitoring",
            Self::PlanReady => "plan ready",
            Self::Done => "done",
            Self::Idle => "idle",
        }
    }

    /// Whether the status deserves attention in the list (everything but the resting states).
    pub fn is_notable(self) -> bool {
        !matches!(self, Self::Done | Self::Idle)
    }
}

impl ThreadShell {
    /// A turn is in flight, or the provider session is starting up for one.
    pub fn is_running(&self) -> bool {
        if matches!(
            self.latest_turn.as_ref().map(|t| t.state.as_str()),
            Some("running")
        ) {
            return true;
        }
        matches!(
            self.session.as_ref().map(|s| s.status.as_str()),
            Some("running") | Some("starting")
        )
    }

    fn visible_pull_requests(&self) -> impl Iterator<Item = &PullRequest> {
        self.pull_requests.iter().filter(|pr| pr.is_visible())
    }

    /// The linked pull requests grouped into stacks, as the desktop app groups them. A
    /// stack the host keeps comes first, in the host's order; the rest are chained by one
    /// pull request's base branch being another's head branch in the same repository. One
    /// that chains to nothing is a stack of one.
    pub fn pull_request_chains(&self) -> Vec<PullRequestChain> {
        let visible: Vec<&PullRequest> = self.visible_pull_requests().collect();
        let mut chains: Vec<PullRequestChain> = Vec::new();

        let mut native: Vec<(String, Vec<&PullRequest>)> = Vec::new();
        for pr in &visible {
            let Some(stack) = &pr.stack else { continue };
            let key = format!("{}#{}", pr.repository_key(), stack.id);
            match native.iter_mut().find(|(k, _)| *k == key) {
                Some((_, members)) => members.push(pr),
                None => native.push((key, vec![pr])),
            }
        }
        for (_, mut members) in native {
            let order: Vec<u64> = members[0]
                .stack
                .as_ref()
                .map(|stack| stack.layers.iter().map(|layer| layer.number).collect())
                .unwrap_or_default();
            members.sort_by_key(|pr| order.iter().position(|n| *n == pr.number).unwrap_or(0));
            chains.push(PullRequestChain {
                layers: members
                    .into_iter()
                    .map(PullRequestRef::from_linked)
                    .collect(),
            });
        }

        let remaining: Vec<&PullRequest> = visible
            .into_iter()
            .filter(|pr| pr.stack.is_none())
            .collect();
        let branch_key =
            |pr: &PullRequest, branch: &str| format!("{}:{branch}", pr.repository_key());
        // A head branch two of them share says nothing about which one is the parent.
        let mut by_head: HashMap<String, Option<usize>> = HashMap::new();
        for (index, pr) in remaining.iter().enumerate() {
            if let Some(head) = pr.head_branch() {
                by_head
                    .entry(branch_key(pr, head))
                    .and_modify(|found| *found = None)
                    .or_insert(Some(index));
            }
        }
        let parent_of = |index: usize| {
            let pr = remaining[index];
            pr.base_branch()
                .and_then(|base| by_head.get(&branch_key(pr, base)).copied().flatten())
                .filter(|parent| *parent != index)
        };
        let has_child: HashSet<usize> = (0..remaining.len()).filter_map(parent_of).collect();
        let mut placed = vec![false; remaining.len()];
        // Down from each top, which is one nothing builds on.
        for top in (0..remaining.len()).filter(|index| !has_child.contains(index)) {
            let mut layers = Vec::new();
            let mut cursor = Some(top);
            while let Some(index) = cursor.filter(|index| !placed[*index]) {
                placed[index] = true;
                layers.insert(0, PullRequestRef::from_linked(remaining[index]));
                cursor = parent_of(index);
            }
            if !layers.is_empty() {
                chains.push(PullRequestChain { layers });
            }
        }
        // A cycle has no top. Its members are still shown, without an order made up for them.
        for (index, pr) in remaining.iter().enumerate() {
            if !placed[index] {
                chains.push(PullRequestChain {
                    layers: vec![PullRequestRef::from_linked(pr)],
                });
            }
        }
        chains
    }

    /// The pull request a one-slot surface shows. First the one on the branch checked out
    /// where the thread works, so moving between the layers of a stack moves with it;
    /// then the one the server knows to be on the thread's branch; then as the desktop app
    /// chooses: the only open one, else the highest open layer of the stack linked most
    /// recently, else the top of a finished stack, else the one updated last.
    pub fn current_pull_request(&self, checked_out: Option<&str>) -> Option<PullRequestRef> {
        let visible: Vec<PullRequestRef> = self
            .visible_pull_requests()
            .map(PullRequestRef::from_linked)
            .collect();
        if let Some(branch) = checked_out {
            let on_branch: Vec<&PullRequestRef> = visible
                .iter()
                .filter(|pr| pr.head_branch.as_deref() == Some(branch))
                .collect();
            if let Some(pr) = on_branch
                .iter()
                .find(|pr| pr.is_open())
                .or(on_branch.last())
            {
                return Some((*pr).clone());
            }
        }
        if let Some(branch_pr) = &self.branch_pull_request {
            let linked = self
                .pull_requests
                .iter()
                .find(|pr| pr.number == branch_pr.number && pr.repository == branch_pr.repository);
            match linked {
                Some(pr) if pr.is_visible() => return Some(PullRequestRef::from_linked(pr)),
                Some(_) => {}
                None => {
                    return Some(PullRequestRef {
                        host: String::new(),
                        repository: branch_pr.repository.clone(),
                        number: branch_pr.number,
                        url: branch_pr.url.clone(),
                        head_branch: None,
                        title: None,
                        state: None,
                        is_draft: false,
                        checks_state: None,
                        review_decision: None,
                        linked_at: String::new(),
                        updated_at: None,
                    });
                }
            }
        }
        let open: Vec<&PullRequestRef> = visible.iter().filter(|pr| pr.is_open()).collect();
        if let [only] = open.as_slice() {
            return Some((*only).clone());
        }
        let chains = self.pull_request_chains();
        if open.len() > 1 {
            // Timestamps are the server's own ISO strings, which sort as they read.
            let mut best: Option<(&str, &PullRequestRef)> = None;
            for chain in &chains {
                let Some(top) = chain.layers.iter().rev().find(|pr| pr.is_open()) else {
                    continue;
                };
                let latest = chain
                    .layers
                    .iter()
                    .filter(|pr| pr.is_open())
                    .map(|pr| pr.linked_at.as_str())
                    .max()
                    .unwrap_or_default();
                if best.is_none_or(|(seen, _)| latest > seen) {
                    best = Some((latest, top));
                }
            }
            return best.map(|(_, pr)| pr.clone());
        }
        if let [chain] = chains.as_slice() {
            return chain.layers.last().cloned();
        }
        let mut latest: Option<&PullRequestRef> = None;
        for pr in &visible {
            let at = |pr: &PullRequestRef| pr.updated_at.clone().unwrap_or(pr.linked_at.clone());
            if latest.is_none_or(|seen| at(pr) > at(seen)) {
                latest = Some(pr);
            }
        }
        latest.cloned()
    }

    /// Every pull request to show, the current one first, without duplicates.
    pub fn all_pull_requests(&self, checked_out: Option<&str>) -> Vec<PullRequestRef> {
        let mut out: Vec<PullRequestRef> =
            self.current_pull_request(checked_out).into_iter().collect();
        for pr in self.visible_pull_requests() {
            if !out.iter().any(|p| p.url == pr.url) {
                out.push(PullRequestRef::from_linked(pr));
            }
        }
        out
    }

    /// The worst checks among the open pull requests given: one failing anywhere holds up
    /// whatever depends on it, so it is the one worth seeing.
    pub fn worst_checks<'a>(prs: impl IntoIterator<Item = &'a PullRequestRef>) -> Option<&'a str> {
        let mut worst = None;
        for checks in prs
            .into_iter()
            .filter(|pr| pr.is_open())
            .filter_map(|pr| pr.checks_state.as_deref())
        {
            let rank = |c: &str| match c {
                "failing" => 3,
                "pending" => 2,
                "passing" => 1,
                _ => 0,
            };
            if rank(checks) > worst.map(rank).unwrap_or(0) {
                worst = Some(checks);
            }
        }
        worst
    }

    pub fn is_settled(&self) -> bool {
        self.settled_at.is_some()
    }

    /// `now` is an RFC 3339 timestamp; ISO-8601 strings in UTC compare lexicographically.
    pub fn is_snoozed(&self, now: &str) -> bool {
        self.snoozed_until
            .as_deref()
            .is_some_and(|until| until > now)
    }

    /// Mirrors the desktop sidebar's status resolution order.
    pub fn status(&self) -> ThreadStatus {
        if self.has_pending_approvals {
            return ThreadStatus::Approval;
        }
        if self.has_pending_user_input {
            return ThreadStatus::Question;
        }
        if self.is_running() {
            return ThreadStatus::Working;
        }
        let session_status = self.session.as_ref().map(|s| s.status.as_str());
        if session_status == Some("error") {
            return ThreadStatus::Failed;
        }
        match self.background_liveness.as_deref() {
            Some("working") => return ThreadStatus::Working,
            Some("monitoring") => return ThreadStatus::Monitoring,
            _ => {}
        }
        if self.interaction_mode == "plan" && self.has_actionable_proposed_plan {
            return ThreadStatus::PlanReady;
        }
        match self.latest_turn.as_ref().map(|t| t.state.as_str()) {
            Some("error") => ThreadStatus::Failed,
            Some("completed") => ThreadStatus::Done,
            _ => ThreadStatus::Idle,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: Id,
    pub role: String,
    pub text: String,
    #[serde(default)]
    pub turn_id: Option<Id>,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    pub id: Id,
    #[serde(default)]
    pub tone: String,
    pub kind: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub turn_id: Option<Id>,
    #[serde(default)]
    pub sequence: Option<u64>,
    #[serde(default)]
    pub created_at: String,
}

impl Activity {
    pub fn str(&self, key: &str) -> Option<&str> {
        self.payload.get(key).and_then(Value::as_str)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposedPlan {
    pub id: Id,
    #[serde(default)]
    pub turn_id: Option<Id>,
    #[serde(default)]
    pub plan_markdown: String,
    #[serde(default)]
    pub implemented_at: Option<String>,
    #[serde(default)]
    pub created_at: String,
}

/// The completion timestamp retained for each historical turn.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub turn_id: Id,
    pub completed_at: String,
    /// How many turns the thread had once this one was done: the number a revert is
    /// asked for by. Absent from servers that predate it, which cannot revert either.
    #[serde(default)]
    pub checkpoint_turn_count: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDetail {
    #[serde(flatten)]
    pub shell: ThreadShell,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub activities: Vec<Activity>,
    #[serde(default)]
    pub proposed_plans: Vec<ProposedPlan>,
    #[serde(default)]
    pub checkpoints: Vec<Checkpoint>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDetailPage {
    #[serde(default)]
    pub before_cursor: Option<String>,
    #[serde(default)]
    pub has_more: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDetailSnapshot {
    pub snapshot_sequence: u64,
    pub thread: ThreadDetail,
    #[serde(default)]
    pub page: Option<ThreadDetailPage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellSnapshot {
    pub snapshot_sequence: u64,
    #[serde(default)]
    pub projects: Vec<Project>,
    #[serde(default)]
    pub threads: Vec<ThreadShell>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[allow(clippy::large_enum_variant)]
pub enum ShellItem {
    Synchronized,
    Snapshot {
        snapshot: ShellSnapshot,
    },
    ProjectUpserted {
        sequence: u64,
        project: Project,
    },
    ProjectRemoved {
        sequence: u64,
        #[serde(rename = "projectId")]
        project_id: Id,
    },
    ThreadUpserted {
        sequence: u64,
        thread: ThreadShell,
    },
    ThreadRemoved {
        sequence: u64,
        #[serde(rename = "threadId")]
        thread_id: Id,
    },
    #[serde(other)]
    Unknown,
}

/// A persisted orchestration event. The payload is interpreted by `type`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub sequence: u64,
    #[serde(default)]
    pub event_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub occurred_at: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[allow(clippy::large_enum_variant)]
pub enum ThreadItem {
    Synchronized,
    Snapshot {
        snapshot: ThreadDetailSnapshot,
    },
    Event {
        event: Event,
    },
    #[serde(other)]
    Unknown,
}

// ── Server config ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    #[serde(default)]
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub thread_snapshot_pagination: bool,
    #[serde(default)]
    pub thread_resume_completion_marker: bool,
    #[serde(default)]
    pub shell_resume_completion_marker: bool,
    #[serde(default)]
    pub settings: ServerSettings,
    #[serde(default)]
    pub environment: Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSettings {
    #[serde(default)]
    pub default_model_selection: Option<ModelSelection>,
    #[serde(default)]
    pub default_runtime_mode: Option<String>,
    /// Server-wide default for where new threads start; the server's own default is
    /// the current checkout.
    #[serde(default)]
    pub default_thread_env_mode: Option<String>,
    /// Whether a new worktree starts from the remote's copy of the base branch rather
    /// than the local one.
    #[serde(default)]
    pub new_worktrees_start_from_origin: bool,
    /// What a project asks for in place of the settings above, by project id. This is
    /// the server's own merged view of the per-project settings, so it answers for the
    /// fields on a project as well as for anything set against the project since.
    #[serde(default)]
    pub project_settings_overrides: HashMap<Id, ProjectSettings>,
}

/// One project's answer to the settings that can be set per project.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSettings {
    #[serde(default)]
    pub default_model_selection: Option<ModelSelection>,
    #[serde(default)]
    pub default_thread_env_mode: Option<String>,
}

impl ServerSettings {
    fn for_project(&self, project: Option<&Project>) -> Option<&ProjectSettings> {
        self.project_settings_overrides.get(&project?.id)
    }

    /// The model a new thread in this project starts with. A project's own field is
    /// taken too: the server folds it into the overrides itself, and where it has not,
    /// it is still what the project says.
    pub fn model_selection(&self, project: Option<&Project>) -> Option<ModelSelection> {
        self.for_project(project)
            .and_then(|settings| settings.default_model_selection.clone())
            .or_else(|| project.and_then(|p| p.default_model_selection.clone()))
            .or_else(|| self.default_model_selection.clone())
    }

    /// Where a new thread in this project starts: `worktree` or `local`.
    pub fn thread_env_mode(&self, project: Option<&Project>) -> Option<String> {
        self.for_project(project)
            .and_then(|settings| settings.default_thread_env_mode.clone())
            .or_else(|| project.and_then(|p| p.default_thread_env_mode.clone()))
            .or_else(|| self.default_thread_env_mode.clone())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub instance_id: String,
    #[serde(default)]
    pub driver: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub installed: bool,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub auth: ProviderAuth,
    #[serde(default)]
    pub availability: Option<String>,
    #[serde(default)]
    pub models: Vec<Model>,
    #[serde(default)]
    pub slash_commands: Vec<SlashCommand>,
    #[serde(default)]
    pub skills: Vec<Skill>,
    /// The same, as they stand in particular directories.
    #[serde(default)]
    pub workspace_snapshots: Vec<WorkspaceSnapshot>,
    /// What the subscription behind this account has left, when the provider reports it
    /// at all. An API key or a cloud endpoint has no quota to report and carries none.
    #[serde(default)]
    pub usage_limits: Option<UsageLimits>,
    /// Whether the provider can drop turns from its own history. Only a `false` says it
    /// cannot: the server leaves the field out for the ones that can.
    #[serde(default)]
    pub supports_conversation_rollback: Option<bool>,
}

impl Provider {
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.instance_id)
    }

    /// The commands and skills to offer in `cwd`: what the provider found there when
    /// it looked, and otherwise what it has everywhere.
    pub fn commands_in(&self, cwd: Option<&str>) -> (&[SlashCommand], &[Skill]) {
        match self
            .workspace_snapshots
            .iter()
            .find(|snapshot| Some(snapshot.cwd.as_str()) == cwd)
        {
            Some(snapshot) => (&snapshot.slash_commands, &snapshot.skills),
            None => (&self.slash_commands, &self.skills),
        }
    }

    pub fn is_usable(&self) -> bool {
        self.enabled
            && self.installed
            && self.availability.as_deref() != Some("unavailable")
            && self.status != "disabled"
    }
}

/// The quota the provider last reported for the account signed in to this instance.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimits {
    /// When the figures were taken, which is not now: they are a probe's answer, and a
    /// stale one is worth reading as long as it says how old it is.
    #[serde(default)]
    pub checked_at: String,
    #[serde(default)]
    pub windows: Vec<UsageWindow>,
    /// Set when there is nothing to report: an account that can never report a quota,
    /// or a probe that failed this time.
    #[serde(default)]
    pub unavailable: Option<UsageUnavailable>,
}

/// One rolling quota window, such as Claude's five-hour session or a weekly allowance.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    #[serde(default)]
    pub id: String,
    /// `session`, `weekly`, `monthly` or `other`, which is what orders the rows.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub used_percent: f64,
    #[serde(default)]
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageUnavailable {
    /// `unsupported` for an account with no quota to report, `probeFailed` for one
    /// whose quota could not be read this time.
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub message: Option<String>,
}

impl UsageWindow {
    /// Where the row sits: the window that runs out first is the one worth reading
    /// first, and a provider is free to send them in any order.
    pub fn rank(&self) -> u8 {
        match self.kind.as_str() {
            "session" => 0,
            "weekly" => 1,
            "monthly" => 2,
            _ => 3,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProviderAuth {
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub short_name: Option<String>,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub is_legacy: bool,
    #[serde(default)]
    pub capabilities: Option<ModelCapabilities>,
}

impl Model {
    pub fn option_descriptors(&self) -> &[OptionDescriptor] {
        self.capabilities
            .as_ref()
            .map(|c| c.option_descriptors.as_slice())
            .unwrap_or(&[])
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCapabilities {
    #[serde(default)]
    pub option_descriptors: Vec<OptionDescriptor>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionDescriptor {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub options: Vec<OptionChoice>,
    #[serde(default)]
    pub current_value: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OptionChoice {
    pub id: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// What the command takes after its name, when it takes anything.
    #[serde(default)]
    pub input: Option<SlashCommandInput>,
    /// `false` for one only the agent starts, which is not the user's to offer.
    #[serde(default)]
    pub user_invocable: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommandInput {
    pub hint: String,
}

/// A skill the provider can run, named in a message as `$name`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub short_description: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    /// `false` for one only the agent starts, as for a command.
    #[serde(default)]
    pub user_invocable: Option<bool>,
}

impl Skill {
    pub fn offered(&self) -> bool {
        self.enabled && self.user_invocable != Some(false)
    }
}

/// The commands and skills a provider has in one directory, which the project's own
/// `.claude` or `.agents` adds to.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSnapshot {
    pub cwd: String,
    #[serde(default)]
    pub slash_commands: Vec<SlashCommand>,
    #[serde(default)]
    pub skills: Vec<Skill>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_item_unknown_kind_is_tolerated() {
        let item: ShellItem = serde_json::from_str(r#"{"kind":"something-new","x":1}"#).unwrap();
        assert!(matches!(item, ShellItem::Unknown));
    }

    #[test]
    fn thread_detail_flattens_shell_fields() {
        let json = r#"{"id":"t","projectId":"p","title":"T","modelSelection":{"instanceId":"claudeAgent","model":"m"},
            "runtimeMode":"full-access","latestTurn":null,"session":null,"messages":[],"activities":[]}"#;
        let detail: ThreadDetail = serde_json::from_str(json).unwrap();
        assert_eq!(detail.shell.title, "T");
        assert!(!detail.shell.has_pending_approvals);
    }

    fn shell_with_prs(prs: serde_json::Value) -> ThreadShell {
        let mut shell = serde_json::json!({"id":"t","projectId":"p","title":"T",
            "modelSelection":{"instanceId":"claudeAgent","model":"m"}});
        shell["pullRequests"] = prs;
        serde_json::from_value(shell).unwrap()
    }

    fn linked(number: u64, state: &str, checks: Option<&str>) -> serde_json::Value {
        serde_json::json!({"host":"github.com","repository":"o/r","number":number,
            "url":format!("https://github.com/o/r/pull/{number}"),
            "source":"agent","linkedAt":"2026-01-01T00:00:00.000Z",
            "snapshot":{"state":state,"title":format!("pr {number}"),"checksState":checks}})
    }

    /// A linked pull request on `head`, asking to merge into `base`.
    fn on(number: u64, head: &str, base: &str, linked_at: &str) -> serde_json::Value {
        let mut pr = linked(number, "open", None);
        pr["linkedAt"] = linked_at.into();
        pr["snapshot"]["headBranch"] = head.into();
        pr["snapshot"]["baseBranch"] = base.into();
        pr
    }

    fn numbers(chains: &[PullRequestChain]) -> Vec<Vec<u64>> {
        chains
            .iter()
            .map(|chain| chain.layers.iter().map(|pr| pr.number).collect())
            .collect()
    }

    /// A failing pull request anywhere in the thread is the one to see; a merged one
    /// no longer holds anything up, whatever its checks said.
    #[test]
    fn the_worst_checks_are_from_the_open_ones() {
        let shell = shell_with_prs(serde_json::json!([
            linked(1, "merged", Some("failing")),
            linked(2, "open", Some("passing")),
            linked(3, "open", Some("pending")),
        ]));
        let prs = shell.all_pull_requests(None);
        assert_eq!(ThreadShell::worst_checks(&prs), Some("pending"));
        assert_eq!(ThreadShell::worst_checks(&prs[..1]), Some("passing"));
        assert_eq!(ThreadShell::worst_checks(&[]), None);
    }

    #[test]
    fn facts_read_as_words() {
        let mut pr = linked(4, "open", Some("failing"));
        pr["snapshot"]["isDraft"] = true.into();
        pr["snapshot"]["reviewDecision"] = "changes-requested".into();
        let shell = shell_with_prs(serde_json::json!([pr]));
        let pr = &shell.all_pull_requests(None)[0];
        assert_eq!(
            pr.facts(false),
            "draft · ✗ checks failing · changes requested"
        );
        assert_eq!(
            pr.facts(true),
            "draft · ✗ checks failing · changes requested · o/r"
        );
    }

    /// A layer taken out of a stack by hand stays linked so the stack does not bring it
    /// back; it is not one of the thread's pull requests to show.
    #[test]
    fn a_dismissed_layer_is_not_shown() {
        let mut dismissed = linked(2, "open", None);
        dismissed["source"] = "stack-dismissed".into();
        let shell = shell_with_prs(serde_json::json!([linked(1, "open", None), dismissed]));
        let shown: Vec<u64> = shell
            .all_pull_requests(None)
            .iter()
            .map(|pr| pr.number)
            .collect();
        assert_eq!(shown, vec![1]);
        assert_eq!(numbers(&shell.pull_request_chains()), vec![vec![1]]);
    }

    /// Linked in any order, the layers come out bottom to top, and one that builds on
    /// none of them is a stack of its own.
    #[test]
    fn pull_requests_chain_base_to_head() {
        let shell = shell_with_prs(serde_json::json!([
            on(3, "c", "b", "2026-01-03T00:00:00.000Z"),
            on(1, "a", "main", "2026-01-01T00:00:00.000Z"),
            on(9, "x", "main", "2026-01-04T00:00:00.000Z"),
            on(2, "b", "a", "2026-01-02T00:00:00.000Z"),
        ]));
        assert_eq!(
            numbers(&shell.pull_request_chains()),
            vec![vec![1, 2, 3], vec![9]]
        );
    }

    /// Two pull requests from one head branch cannot say which is the parent, so neither
    /// is; and a cycle has no top to start from, so its members stand alone.
    #[test]
    fn ambiguous_parents_and_cycles_are_not_chained() {
        let shared = shell_with_prs(serde_json::json!([
            on(1, "a", "main", ""),
            on(2, "a", "main", ""),
            on(3, "b", "a", ""),
        ]));
        assert_eq!(
            numbers(&shared.pull_request_chains()),
            vec![vec![1], vec![2], vec![3]]
        );
        let cycle = shell_with_prs(serde_json::json!([
            on(1, "a", "b", ""),
            on(2, "b", "a", "")
        ]));
        assert_eq!(
            numbers(&cycle.pull_request_chains()),
            vec![vec![1], vec![2]]
        );
    }

    /// A stack the host keeps is taken in the host's order, whatever the branches say.
    #[test]
    fn a_host_stack_keeps_its_own_order() {
        let stack = serde_json::json!({"kind":"native","id":"s1","number":1,"url":"u","base":"main",
            "layers":[{"number":5,"headBranch":"p","state":"open"},
                      {"number":4,"headBranch":"q","state":"open"}]});
        let mut top = on(4, "q", "main", "");
        top["stack"] = stack.clone();
        let mut bottom = on(5, "p", "main", "");
        bottom["stack"] = stack;
        let shell = shell_with_prs(serde_json::json!([top, bottom, on(6, "r", "q", "")]));
        assert_eq!(
            numbers(&shell.pull_request_chains()),
            vec![vec![5, 4], vec![6]]
        );
    }

    /// The layer checked out is the one shown, so moving through a stack moves with it;
    /// without one, it is the highest open layer of the stack linked last.
    #[test]
    fn the_current_pull_request_follows_the_checkout() {
        let mut merged = on(1, "a", "main", "2026-01-01T00:00:00.000Z");
        merged["snapshot"]["state"] = "merged".into();
        let shell = shell_with_prs(serde_json::json!([
            merged,
            on(2, "b", "a", "2026-01-02T00:00:00.000Z"),
            on(3, "c", "b", "2026-01-03T00:00:00.000Z"),
            on(7, "x", "main", "2026-01-01T00:00:00.000Z"),
            on(8, "y", "x", "2026-01-01T12:00:00.000Z"),
        ]));
        let current = |branch| shell.current_pull_request(branch).map(|pr| pr.number);
        assert_eq!(current(Some("b")), Some(2));
        assert_eq!(
            current(Some("a")),
            Some(1),
            "a merged layer is still the one checked out"
        );
        assert_eq!(current(Some("main")), Some(3));
        assert_eq!(current(None), Some(3));
    }

    /// When everything has landed, a single stack is shown by its top.
    #[test]
    fn a_finished_stack_is_shown_by_its_top() {
        let mut prs = vec![
            on(1, "a", "main", "2026-01-02T00:00:00.000Z"),
            on(2, "b", "a", "2026-01-01T00:00:00.000Z"),
        ];
        for pr in &mut prs {
            pr["snapshot"]["state"] = "merged".into();
        }
        let shell = shell_with_prs(serde_json::json!(prs));
        assert_eq!(
            shell.current_pull_request(None).map(|pr| pr.number),
            Some(2)
        );
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    fn project(id: &str, env_mode: Option<&str>) -> Project {
        Project {
            id: id.into(),
            title: id.into(),
            workspace_root: format!("/src/{id}"),
            project_icon: None,
            default_model_selection: None,
            default_thread_env_mode: env_mode.map(str::to_string),
        }
    }

    fn settings(overrides: &[(&str, &str)]) -> ServerSettings {
        ServerSettings {
            default_thread_env_mode: Some("local".into()),
            project_settings_overrides: overrides
                .iter()
                .map(|(id, mode)| {
                    (
                        (*id).to_string(),
                        ProjectSettings {
                            default_model_selection: None,
                            default_thread_env_mode: Some((*mode).to_string()),
                        },
                    )
                })
                .collect(),
            ..ServerSettings::default()
        }
    }

    /// What a project asks for is kept apart from the project itself, so reading only
    /// the project is reading a field that is usually empty — and every new thread
    /// starts in the checkout of a project that asked for a worktree.
    #[test]
    fn a_project_setting_is_taken_from_where_the_server_keeps_it() {
        let settings = settings(&[("one", "worktree")]);
        assert_eq!(
            settings
                .thread_env_mode(Some(&project("one", None)))
                .as_deref(),
            Some("worktree")
        );
        // Nothing set for it: the server-wide answer.
        assert_eq!(
            settings
                .thread_env_mode(Some(&project("two", None)))
                .as_deref(),
            Some("local")
        );
        // Set on the project itself, which the server has not folded in yet.
        assert_eq!(
            settings
                .thread_env_mode(Some(&project("two", Some("worktree"))))
                .as_deref(),
            Some("worktree")
        );
        assert_eq!(settings.thread_env_mode(None).as_deref(), Some("local"));
    }
}

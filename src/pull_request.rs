//! A pull request read in place of the conversation, from `pullRequests.detail`.
//!
//! It goes through the chat the way a subagent's transcript does, so it scrolls, searches,
//! and yanks like the conversation around it. Unlike a transcript it is not shaped into
//! messages: labels carry the host's colours, a check's state is its colour as much as its
//! glyph, and a check's link would crowd its row if it were written out on it. So the blocks
//! are drawn here, already styled, and each check row is a region of its own that `gx` opens.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    model::{PullRequestRef, ThreadDetailSnapshot},
    state::ThreadState,
    timeline::{Block, BlockKey, Region},
};

/// The region key a check row is known by, for `gx` to find its link.
const CHECK_KEY: &str = "pr-check:";
/// The region key a reviewer's row is known by, which also folds what they last said.
const REVIEW_KEY: &str = "pr-review:";
const PASSED_KEY: &str = "pr:passed";
const SKIPPED_KEY: &str = "pr:skipped";
const BODY_KEY: &str = "pr:body";
/// A description longer than this is folded to its opening lines.
const BODY_FOLD: usize = 24;
const BODY_SHOWN: usize = 16;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Detail {
    pub repository: String,
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub url: String,
    #[serde(default)]
    pub author: Option<Actor>,
    /// open | closed | merged
    pub state: String,
    #[serde(default)]
    pub is_draft: bool,
    /// mergeable | conflicting | unknown
    #[serde(default)]
    pub mergeability: Option<String>,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
    #[serde(default)]
    pub changed_files: u64,
    pub head_branch: String,
    pub base_branch: String,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub checks: Vec<Check>,
    #[serde(default)]
    pub behind_by: Option<u64>,
    #[serde(default)]
    pub auto_merge_enabled: Option<bool>,
    #[serde(default)]
    pub auto_merge_method: Option<String>,
    #[serde(default)]
    pub workflow_approvals_required: Option<u64>,
    /// Who has been asked to review and has not answered since.
    #[serde(default)]
    pub reviewers: Vec<Actor>,
    /// What the host can do with a pull request, and what this viewer may.
    #[serde(default)]
    pub capabilities: Option<Capabilities>,
    #[serde(default)]
    pub viewer_permissions: Option<Permissions>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Capabilities {
    /// A server that says nothing about labels has no way to change them.
    #[serde(default)]
    pub labels: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Permissions {
    /// Absent is granted, like every permission the server reports.
    #[serde(default)]
    pub labels: Option<bool>,
}

impl Detail {
    /// Whether its labels can be changed from here: the host has to be able to, and the
    /// viewer must not have been told they may not.
    pub fn labels_editable(&self) -> bool {
        let host = self.capabilities.as_ref().and_then(|c| c.labels) == Some(true);
        let viewer = self.viewer_permissions.as_ref().and_then(|p| p.labels) != Some(false);
        host && viewer
    }
}

/// A label the repository has, from `pullRequests.labelCandidates`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelCandidate {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub is_applied: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LabelCandidates {
    #[serde(default)]
    pub candidates: Vec<LabelCandidate>,
    /// The repository has more labels than the server read.
    #[serde(default)]
    pub truncated: bool,
}

/// The conversation half of a pull request, from `pullRequests.activity`, which the server
/// reads separately because a long review history is slow to page through.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    #[serde(default)]
    pub comments: Vec<Comment>,
    #[serde(default)]
    pub review_threads: Vec<ReviewThread>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    /// issue-comment | review-comment | review
    pub kind: String,
    #[serde(default)]
    pub author: Option<Actor>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub url: Option<String>,
    /// The host's word for a review's verdict: APPROVED, CHANGES_REQUESTED, COMMENTED,
    /// DISMISSED, or PENDING for one not yet submitted.
    #[serde(default)]
    pub review_state: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThread {
    #[serde(default)]
    pub is_resolved: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Actor {
    pub login: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub name: String,
    /// The host's hex colour, with or without its `#`.
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Check {
    pub name: String,
    /// pending | action-required | success | failure | skipped | neutral | cancelled
    pub status: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// How each check status reads, most pressing first: the order the rows are listed in.
const STATUSES: [(&str, &str, &str, Color); 7] = [
    ("failure", "✗", "failing", Color::Red),
    ("action-required", "!", "need action", Color::Yellow),
    ("pending", "○", "pending", Color::Yellow),
    ("cancelled", "⊘", "cancelled", Color::DarkGray),
    ("neutral", "–", "neutral", Color::Gray),
    ("skipped", "⊘", "skipped", Color::DarkGray),
    ("success", "✓", "passed", Color::Green),
];

fn status(name: &str) -> (usize, &'static str, &'static str, Color) {
    STATUSES
        .iter()
        .enumerate()
        .find(|(_, (status, ..))| *status == name)
        .map(|(rank, (_, glyph, word, color))| (rank, *glyph, *word, *color))
        .unwrap_or((STATUSES.len(), "?", "unknown", Color::Gray))
}

/// The thread the view stands in for. The chat keeps its place and its cache by the
/// thread's identity and revision, so a re-read has to arrive as a new revision; the body
/// is its one message, which is what yanking the last message takes.
pub fn state(detail: &Detail, revision: u64) -> Result<ThreadState> {
    let snapshot = serde_json::from_value::<ThreadDetailSnapshot>(json!({
        "snapshotSequence": 0,
        "thread": {
            "id": format!("pr:{}", detail.url),
            "projectId": "",
            "title": detail.title,
            "modelSelection": { "instanceId": "", "model": "" },
            "messages": [{
                "id": "pr-body",
                "role": "assistant",
                "text": detail.body.trim_end(),
                "createdAt": "",
                "updatedAt": "",
            }],
            "activities": [],
            "proposedPlans": [],
        },
    }))?;
    let mut state = ThreadState::from_snapshot(snapshot);
    state.revision = revision;
    Ok(state)
}

/// Everything besides the detail that the view is drawn from.
pub struct Context<'a> {
    /// The review history, `None` while it is on its way.
    pub activity: Option<&'a Result<Activity, String>>,
    /// The stack it is a layer of, bottom to top, and which layer it is.
    pub stack: Option<(&'a [PullRequestRef], usize)>,
    /// The folds opened by hand, and how many levels are open everywhere.
    pub expanded: &'a HashSet<String>,
    pub open_levels: u8,
    pub size: (u16, u16),
    /// What has come back for the pictures the description and reviews point at.
    pub images: &'a HashMap<String, Option<String>>,
    pub now: &'a str,
}

/// The view's blocks: where it stands, its checks, its reviews, and what it says. A
/// reviewer's row is open by default when theirs is the latest review, so `za` there shuts
/// it rather than opening it.
pub fn blocks(detail: &Detail, context: &Context<'_>) -> Vec<Block> {
    let Context {
        activity,
        stack,
        expanded,
        open_levels,
        size,
        images,
        now,
    } = *context;
    let open = |key: &str| open_levels > 0 || expanded.contains(key);
    let flipped =
        |key: &str, by_default: bool| open_levels > 0 || expanded.contains(key) != by_default;
    vec![
        summary(detail, stack),
        checks(detail, &open),
        reviews(detail, activity, &flipped, now, size, images),
        body(detail, &open, &flipped, size, images),
    ]
}

/// The stack across the top: each layer by number, bottom to top, in the colour of where it
/// stands, with the one being read picked out.
fn stack_strip(layers: &[PullRequestRef], at: usize) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("stack {}/{}  ", at + 1, layers.len()),
        dim(),
    )];
    for (index, layer) in layers.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" › ", dim()));
        }
        let (glyph, color) = match layer.state.as_deref() {
            Some("merged") => ("◆", Color::Magenta),
            Some("closed") => ("✗", Color::Red),
            _ if layer.is_draft => ("◌", Color::Gray),
            _ => ("●", Color::Green),
        };
        let mut style = Style::default().fg(color);
        if index == at {
            style = style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
        }
        spans.push(Span::styled(format!("{glyph} #{}", layer.number), style));
    }
    spans.push(Span::styled("   [ ] move through it", dim()));
    Line::from(spans)
}

/// The link a region of the view stands for, for `gx`: a check's page, or the review a
/// reviewer's row shows.
pub fn link_at(key: &str, detail: &Detail, activity: Option<&Activity>) -> Option<String> {
    if let Some(url) = key.strip_prefix(CHECK_KEY) {
        return Some(url.to_string());
    }
    let login = key.strip_prefix(REVIEW_KEY)?;
    reviewers(detail, activity?)
        .into_iter()
        .find(|reviewer| reviewer.login == login)?
        .shown?
        .url
        .clone()
}

/// Where a reviewer stands, as the host decides it: their latest approval, request for
/// changes, or dismissal, and only when they have none of those, that they commented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Verdict {
    ChangesRequested,
    Waiting,
    Commented,
    Approved,
    Dismissed,
}

impl Verdict {
    fn from_state(state: &str) -> Option<Self> {
        match state {
            "APPROVED" => Some(Self::Approved),
            "CHANGES_REQUESTED" => Some(Self::ChangesRequested),
            "DISMISSED" => Some(Self::Dismissed),
            "COMMENTED" => Some(Self::Commented),
            _ => None,
        }
    }

    /// Glyph, what a row says they did, what the count calls them, and colour.
    fn looks(self) -> (&'static str, &'static str, &'static str, Color) {
        match self {
            Self::ChangesRequested => ("✗", "requested changes", "changes requested", Color::Red),
            Self::Waiting => ("○", "review requested", "waiting", Color::Yellow),
            Self::Commented => ("◇", "commented", "commented", Color::Gray),
            Self::Approved => ("✓", "approved", "approved", Color::Green),
            Self::Dismissed => ("⊘", "was dismissed", "dismissed", Color::DarkGray),
        }
    }
}

struct Reviewer<'a> {
    login: &'a str,
    verdict: Verdict,
    /// What they said before being asked again, for a reviewer who is waiting.
    earlier: Option<Verdict>,
    /// The review the row stands for: their latest with something to say, else their
    /// latest at all.
    shown: Option<&'a Comment>,
    latest_at: &'a str,
}

/// Everybody who has reviewed or is waiting to, most pressing first and newest first
/// within that. The author's own replies are not reviews of their work.
fn reviewers<'a>(detail: &'a Detail, activity: &'a Activity) -> Vec<Reviewer<'a>> {
    let author = detail.author.as_ref().map(|a| a.login.as_str());
    let mut reviews: Vec<&Comment> = activity
        .comments
        .iter()
        .filter(|c| c.kind == "review")
        .filter(|c| {
            c.review_state
                .as_deref()
                .and_then(Verdict::from_state)
                .is_some()
        })
        .filter(|c| {
            c.author
                .as_ref()
                .is_some_and(|a| Some(a.login.as_str()) != author)
        })
        .collect();
    reviews.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    let mut out: Vec<Reviewer> = Vec::new();
    for review in reviews {
        let login = review
            .author
            .as_ref()
            .map(|a| a.login.as_str())
            .unwrap_or_default();
        let verdict = review.review_state.as_deref().and_then(Verdict::from_state);
        let Some(verdict) = verdict else { continue };
        let at = match out.iter().position(|r| r.login == login) {
            Some(at) => at,
            None => {
                out.push(Reviewer {
                    login,
                    verdict,
                    earlier: None,
                    shown: None,
                    latest_at: "",
                });
                out.len() - 1
            }
        };
        let reviewer = &mut out[at];
        if verdict != Verdict::Commented || reviewer.verdict == Verdict::Commented {
            reviewer.verdict = verdict;
        }
        if !prose(&review.body).is_empty()
            || reviewer.shown.is_none_or(|s| prose(&s.body).is_empty())
        {
            reviewer.shown = Some(review);
        }
        reviewer.latest_at = &review.created_at;
    }
    for asked in &detail.reviewers {
        match out.iter_mut().find(|r| r.login == asked.login) {
            Some(reviewer) => {
                reviewer.earlier = Some(reviewer.verdict);
                reviewer.verdict = Verdict::Waiting;
            }
            None => out.push(Reviewer {
                login: &asked.login,
                verdict: Verdict::Waiting,
                earlier: None,
                shown: None,
                latest_at: "",
            }),
        }
    }
    out.sort_by(|a, b| a.verdict.cmp(&b.verdict).then(b.latest_at.cmp(a.latest_at)));
    out
}

/// How long ago, in the largest unit that fits: `4m`, `3h`, `2d`, `5w`.
fn ago(since: &str, now: &str) -> String {
    let parse = |text: &str| {
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()
    };
    let (Some(since), Some(now)) = (parse(since), parse(now)) else {
        return String::new();
    };
    let seconds = (now - since).whole_seconds().max(0);
    match seconds {
        0..60 => "just now".to_string(),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        86_400..1_209_600 => format!("{}d ago", seconds / 86_400),
        _ => format!("{}w ago", seconds / 604_800),
    }
}

fn reviews(
    detail: &Detail,
    activity: Option<&Result<Activity, String>>,
    open: &dyn Fn(&str, bool) -> bool,
    now: &str,
    (width, height): (u16, u16),
    images: &HashMap<String, Option<String>>,
) -> Block {
    let mut drawn = Drawn::default();
    let activity = match activity {
        None => {
            drawn.lines.push(Line::from(vec![
                heading("Reviews"),
                Span::styled("  loading…", dim()),
            ]));
            drawn.lines.push(Line::default());
            return block("pr-reviews", drawn.lines, drawn.rows);
        }
        Some(Err(error)) => {
            drawn.lines.push(Line::from(vec![
                heading("Reviews"),
                Span::styled(format!("  could not be read: {error}"), dim()),
            ]));
            drawn.lines.push(Line::default());
            return block("pr-reviews", drawn.lines, drawn.rows);
        }
        Some(Ok(activity)) => activity,
    };
    let reviewers = reviewers(detail, activity);
    let unresolved = activity
        .review_threads
        .iter()
        .filter(|t| !t.is_resolved)
        .count();

    let mut header = vec![heading("Reviews")];
    let mut counted: Vec<(Verdict, usize)> = Vec::new();
    for reviewer in &reviewers {
        match counted.last_mut() {
            Some((verdict, count)) if *verdict == reviewer.verdict => *count += 1,
            _ => counted.push((reviewer.verdict, 1)),
        }
    }
    if counted.is_empty() {
        header.push(Span::styled("  none yet", dim()));
    }
    for (index, (verdict, count)) in counted.iter().enumerate() {
        let (glyph, _, word, color) = verdict.looks();
        header.push(Span::styled(
            if index == 0 { "  " } else { " · " }.to_string(),
            dim(),
        ));
        header.push(Span::styled(
            format!("{glyph} {count} {word}"),
            Style::default().fg(color),
        ));
    }
    if unresolved > 0 {
        header.push(Span::styled(
            format!(
                " · {unresolved} unresolved thread{}",
                if unresolved == 1 { "" } else { "s" }
            ),
            Style::default().fg(Color::Yellow),
        ));
    }
    drawn.lines.push(Line::from(header));

    // The latest review is the one most likely to be why anybody is looking.
    let newest = reviewers
        .iter()
        .filter(|r| r.shown.is_some_and(|s| !prose(&s.body).is_empty()))
        .max_by(|a, b| a.latest_at.cmp(b.latest_at))
        .map(|r| r.login);
    for reviewer in &reviewers {
        let (glyph, did, _, color) = reviewer.verdict.looks();
        let key = format!("{REVIEW_KEY}{}", reviewer.login);
        let said = reviewer
            .shown
            .map(|s| prose(&s.body))
            .filter(|b| !b.is_empty());
        let is_open = said.is_some() && open(&key, newest == Some(reviewer.login));
        let first = drawn.lines.len();
        let mut spans = vec![
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::raw(format!("@{}", reviewer.login)),
            Span::styled(format!(" {did}"), Style::default().fg(color)),
        ];
        if let Some(earlier) = reviewer.earlier {
            spans.push(Span::styled(
                format!(" · earlier {}", earlier.looks().1),
                dim(),
            ));
        }
        if let Some(shown) = reviewer.shown {
            let when = ago(&shown.created_at, now);
            if !when.is_empty() {
                spans.push(Span::styled(format!(" · {when}"), dim()));
            }
        }
        if said.is_some() {
            spans.push(Span::styled(if is_open { " ▾" } else { " ▸" }, dim()));
        }
        drawn.lines.push(Line::from(spans));
        let foldable = said.is_some();
        if let (true, Some(said)) = (is_open, said) {
            let id = format!("review-{}", reviewer.login);
            let body = draw_markdown(&id, &said, open, (width.saturating_sub(2), height), images);
            drawn.append(body, 2);
        }
        drawn.rows.push(Region {
            first,
            end: drawn.lines.len(),
            key,
            foldable,
        });
    }
    drawn.lines.push(Line::default());
    let mut block = block("pr-reviews", drawn.lines, drawn.rows);
    block.images = drawn.images;
    block.pictures = drawn.pictures;
    block
}

/// The pictures the description and the reviews shown show, to be fetched.
pub fn image_urls(detail: &Detail, activity: Option<&Activity>) -> Vec<String> {
    let pictures = |body: &str| crate::timeline::web_images(&markdown_images(&prose(body)));
    let mut urls = pictures(&detail.body);
    for reviewer in activity.map(|a| reviewers(detail, a)).unwrap_or_default() {
        if let Some(review) = reviewer.shown {
            urls.extend(pictures(&review.body));
        }
    }
    urls
}

/// Markdown drawn for the view as the chat draws a message, pictures and all, with each
/// `<details>` folded to its summary until `za` opens it.
#[derive(Default)]
struct Drawn {
    lines: Vec<Line<'static>>,
    rows: Vec<Region>,
    images: Vec<crate::timeline::Placed>,
    pictures: Vec<(String, crate::timeline::Picture)>,
}

impl Drawn {
    /// Take in what was drawn on its own, below what is here and `indent` columns in.
    fn append(&mut self, other: Drawn, indent: u16) {
        let offset = self.lines.len();
        for mut line in other.lines {
            if indent > 0 {
                line.spans.insert(0, Span::raw(" ".repeat(indent as usize)));
            }
            self.lines.push(line);
        }
        self.rows
            .extend(other.rows.into_iter().map(|region| Region {
                first: region.first + offset,
                end: region.end + offset,
                ..region
            }));
        self.images.extend(
            other
                .images
                .into_iter()
                .map(|placed| crate::timeline::Placed {
                    line: placed.line + offset,
                    indent: placed.indent + indent,
                    ..placed
                }),
        );
        self.pictures.extend(other.pictures);
    }
}

/// Draw markdown whose folds are known by `id`. A `<details>` is shut unless it was written
/// `<details open>`, as the host shows it.
fn draw_markdown(
    id: &str,
    source: &str,
    open: &dyn Fn(&str, bool) -> bool,
    (width, height): (u16, u16),
    images: &HashMap<String, Option<String>>,
) -> Drawn {
    let mut drawn = Drawn::default();
    for (n, segment) in split_details(&markdown_images(source))
        .into_iter()
        .enumerate()
    {
        match segment {
            Segment::Markdown(text) => {
                let mut rendered = crate::timeline::markdown(text.trim_matches('\n'));
                let (images, rows, pictures) = crate::timeline::place_message_images(
                    &format!("{id}-{n}"),
                    &text,
                    &mut rendered,
                    width,
                    height,
                    Some(images),
                );
                let lines = rendered.lines;
                drawn.append(
                    Drawn {
                        lines,
                        rows,
                        images,
                        pictures,
                    },
                    0,
                );
            }
            Segment::Details {
                summary,
                inner,
                open: by_default,
            } => {
                let key = format!("{id}/details/{n}");
                let is_open = open(&key, by_default);
                let first = drawn.lines.len();
                drawn.lines.push(Line::from(vec![
                    Span::styled(if is_open { "▾ " } else { "▸ " }, dim()),
                    Span::styled(summary, Style::default().add_modifier(Modifier::BOLD)),
                ]));
                if is_open {
                    let inner = draw_markdown(
                        &format!("{id}/{n}"),
                        &inner,
                        open,
                        (width.saturating_sub(2), height),
                        images,
                    );
                    drawn.append(inner, 2);
                }
                drawn.rows.push(fold_region(&key, first, drawn.lines.len()));
            }
        }
    }
    drawn
}

enum Segment {
    Markdown(String),
    Details {
        summary: String,
        inner: String,
        open: bool,
    },
}

/// Markdown cut at its top-level `<details>` blocks, which nest. Code is left as written.
fn split_details(source: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut markdown = String::new();
    let mut raw = String::new();
    let mut fence: Option<char> = None;
    let mut depth = 0usize;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let mark = ['`', '~']
            .into_iter()
            .find(|mark| trimmed.starts_with(&mark.to_string().repeat(3)));
        let in_code = fence.is_some() || mark.is_some();
        match (fence, mark) {
            (None, Some(mark)) => fence = Some(mark),
            (Some(open), Some(mark)) if open == mark => fence = None,
            _ => {}
        }
        // Lowercasing ASCII keeps every byte where it was, so positions carry over.
        let lower = line.to_ascii_lowercase();
        let opens = if in_code {
            0
        } else {
            lower.matches("<details").count()
        };
        let closes = if in_code {
            0
        } else {
            lower.matches("</details>").count()
        };
        if depth == 0 {
            let Some(at) = lower.find("<details").filter(|_| opens > 0) else {
                markdown.push_str(line);
                continue;
            };
            markdown.push_str(&line[..at]);
            if !markdown.trim().is_empty() {
                out.push(Segment::Markdown(std::mem::take(&mut markdown)));
            }
            markdown.clear();
            raw.push_str(&line[at..]);
        } else {
            raw.push_str(line);
        }
        depth = (depth + opens).saturating_sub(closes);
        if depth == 0 {
            out.push(details(&std::mem::take(&mut raw)));
        }
    }
    // An unclosed one runs to the end, as it does in HTML.
    if depth > 0 {
        out.push(details(&raw));
    }
    if !markdown.trim().is_empty() {
        out.push(Segment::Markdown(markdown));
    }
    out
}

/// One `<details>` block, from its opening tag to its closing one.
fn details(raw: &str) -> Segment {
    let lower = raw.to_ascii_lowercase();
    let tag_end = lower.find('>').map_or(raw.len(), |at| at + 1);
    let open = lower[..tag_end].contains(" open");
    let end = lower
        .rfind("</details>")
        .filter(|end| *end >= tag_end)
        .unwrap_or(raw.len());
    let mut inner = &raw[tag_end..end];
    let mut summary = String::new();
    let lower = inner.to_ascii_lowercase();
    if let Some(start) = lower
        .find("<summary")
        .filter(|at| lower[..*at].trim().is_empty())
    {
        let text_start = lower[start..].find('>').map(|at| start + at + 1);
        if let (Some(from), Some(to)) = (text_start, lower.find("</summary>"))
            && to >= from
        {
            summary = strip_tags(&inner[from..to]);
            inner = &inner[to + "</summary>".len()..];
        }
    }
    if summary.is_empty() {
        summary = "Details".to_string();
    }
    Segment::Details {
        summary,
        inner: inner.to_string(),
        open,
    }
}

/// Text with its tags taken out and its whitespace run together, for a line of its own.
fn strip_tags(text: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What a body says, with its HTML comments taken out: a pull request template's
/// instructions, bots' bookkeeping, anything the host itself does not show.
pub fn prose(text: &str) -> String {
    without_comments(text).trim().to_string()
}

/// The markdown with its HTML comments cut out. They are found by the markdown parser, so
/// one written inside code is code and stays. An unclosed comment runs to the end, as it
/// does in HTML.
fn without_comments(text: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser};
    if !text.contains("<!--") {
        return text.to_string();
    }
    let mut options = Options::empty();
    options.insert(Options::ENABLE_GFM);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let mut cut: Vec<std::ops::Range<usize>> = Vec::new();
    // Where a comment began that has not ended yet: an HTML block hands its lines over
    // one at a time, so a comment can span several of them.
    let mut open: Option<usize> = None;
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        if !matches!(event, Event::Html(_) | Event::InlineHtml(_)) {
            continue;
        }
        let mut at = range.start;
        while at < range.end {
            let rest = &text[at..range.end];
            match open {
                Some(start) => match rest.find("-->") {
                    Some(end) => {
                        cut.push(start..at + end + 3);
                        open = None;
                        at += end + 3;
                    }
                    None => break,
                },
                None => match rest.find("<!--") {
                    Some(begin) => {
                        open = Some(at + begin);
                        at += begin + 4;
                    }
                    None => break,
                },
            }
        }
    }
    if let Some(start) = open {
        cut.push(start..text.len());
    }
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    for range in cut {
        out.push_str(&text[from..range.start]);
        from = range.end;
    }
    out.push_str(&text[from..]);
    out
}

/// The description with each `<img>` tag written as the markdown image it stands for. A
/// picture dropped into a pull request on GitHub goes into the body as a tag rather than as
/// markdown, and the renderer only sees markdown's. Code is left as it was written.
fn markdown_images(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut fence: Option<char> = None;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let opens = ['`', '~']
            .into_iter()
            .find(|mark| trimmed.starts_with(&mark.to_string().repeat(3)));
        match (fence, opens) {
            (None, Some(mark)) => fence = Some(mark),
            (Some(open), Some(mark)) if open == mark => fence = None,
            _ => {}
        }
        if fence.is_some() || opens.is_some() || !line.contains("<img") {
            out.push_str(line);
            continue;
        }
        let mut rest = line;
        while let Some(start) = rest.find("<img") {
            let Some(length) = rest[start..].find('>') else {
                break;
            };
            let tag = &rest[start..start + length + 1];
            out.push_str(&rest[..start]);
            match attribute(tag, "src") {
                Some(src) => {
                    let alt = attribute(tag, "alt").unwrap_or_default();
                    out.push_str(&format!("![{alt}](<{src}>)"));
                }
                None => out.push_str(tag),
            }
            rest = &rest[start + length + 1..];
        }
        out.push_str(rest);
    }
    out
}

/// An attribute's value in an HTML tag, quoted either way.
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(at) = tag[from..].find(name) {
        let at = from + at;
        from = at + name.len();
        // The whole attribute, not the end of a longer one: `data-src` is not `src`.
        if !tag[..at]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
        {
            continue;
        }
        let value = tag[from..].trim_start().strip_prefix('=')?.trim_start();
        let quote = value.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let value = &value[1..];
        return value.find(quote).map(|end| &value[..end]);
    }
    None
}

fn block(key: &str, lines: Vec<Line<'static>>, rows: Vec<Region>) -> Block {
    let plain = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    Block {
        key: BlockKey::Section(key.to_string()),
        text: Text::from(lines),
        rows,
        exports: vec![(format!("msg:{key}"), format!("{plain}\n"))],
        images: Vec::new(),
        pictures: Vec::new(),
    }
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn heading(text: &str) -> Span<'static> {
    Span::styled(
        text.to_string(),
        Style::default().add_modifier(Modifier::BOLD),
    )
}

fn summary(detail: &Detail, stack: Option<(&[PullRequestRef], usize)>) -> Block {
    let mut lines = Vec::new();
    if let Some((layers, at)) = stack {
        lines.push(stack_strip(layers, at));
    }
    lines.push(Line::from(Span::styled(
        format!("#{} {}", detail.number, detail.title),
        Style::default().add_modifier(Modifier::BOLD),
    )));

    let (state, color) = match detail.state.as_str() {
        "merged" => ("merged", Color::Magenta),
        "closed" => ("closed", Color::Red),
        _ if detail.is_draft => ("draft", Color::Gray),
        _ => ("open", Color::Green),
    };
    let mut facts = vec![Span::styled(
        state.to_string(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )];
    if let Some(author) = &detail.author {
        facts.push(Span::styled(format!(" · @{}", author.login), dim()));
    }
    facts.push(Span::styled(
        format!(" · {} → {}", detail.head_branch, detail.base_branch),
        dim(),
    ));
    lines.push(Line::from(facts));
    lines.push(Line::from(vec![
        Span::styled(
            format!("+{}", detail.additions),
            Style::default().fg(Color::Green),
        ),
        Span::raw(" "),
        Span::styled(
            format!("−{}", detail.deletions),
            Style::default().fg(Color::Red),
        ),
        Span::styled(
            format!(
                " · {} file{}",
                detail.changed_files,
                if detail.changed_files == 1 { "" } else { "s" }
            ),
            dim(),
        ),
    ]));

    // What stands between it and landing, where somebody has something to do about it.
    let mut attention: Vec<String> = Vec::new();
    if detail.state == "open" {
        if detail.mergeability.as_deref() == Some("conflicting") {
            attention.push(format!("conflicts with {}", detail.base_branch));
        }
        if let Some(behind) = detail.behind_by.filter(|n| *n > 0) {
            attention.push(format!(
                "{behind} commit{} behind {}",
                if behind == 1 { "" } else { "s" },
                detail.base_branch
            ));
        }
        if let Some(waiting) = detail.workflow_approvals_required.filter(|n| *n > 0) {
            attention.push(format!(
                "{waiting} workflow run{} waiting for approval",
                if waiting == 1 { "" } else { "s" }
            ));
        }
    }
    for line in attention {
        lines.push(Line::from(Span::styled(
            format!("⚠ {line}"),
            Style::default().fg(Color::Yellow),
        )));
    }
    if detail.auto_merge_enabled == Some(true) && detail.state == "open" {
        let method = detail
            .auto_merge_method
            .as_deref()
            .map(|method| format!(" ({method})"))
            .unwrap_or_default();
        lines.push(Line::from(Span::styled(
            format!("auto-merge is on{method}"),
            Style::default().fg(Color::Cyan),
        )));
    }

    if !detail.labels.is_empty() {
        let mut chips: Vec<Span> = Vec::new();
        for label in &detail.labels {
            if !chips.is_empty() {
                chips.push(Span::raw(" "));
            }
            chips.push(chip(label));
        }
        lines.push(Line::from(chips));
    }
    lines.push(Line::default());
    block("pr-summary", lines, Vec::new())
}

/// A label in its own colour, with text that stays readable on it.
fn chip(label: &Label) -> Span<'static> {
    let text = format!(" {} ", label.name);
    match label.color.as_deref().and_then(rgb) {
        Some((r, g, b)) => {
            // Relative luminance, near enough: light backgrounds get dark text.
            let light = 299 * r as u32 + 587 * g as u32 + 114 * b as u32 > 150_000;
            Span::styled(
                text,
                Style::default().bg(Color::Rgb(r, g, b)).fg(if light {
                    Color::Black
                } else {
                    Color::White
                }),
            )
        }
        None => Span::styled(text, Style::default().add_modifier(Modifier::REVERSED)),
    }
}

fn rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let part = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
    Some((part(0)?, part(2)?, part(4)?))
}

fn checks(detail: &Detail, open: &dyn Fn(&str) -> bool) -> Block {
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    if detail.checks.is_empty() {
        lines.push(Line::from(vec![
            heading("Checks"),
            Span::styled("  none", dim()),
        ]));
        lines.push(Line::default());
        return block("pr-checks", lines, rows);
    }

    let mut sorted: Vec<&Check> = detail.checks.iter().collect();
    sorted.sort_by_key(|check| status(&check.status).0);
    let mut counts: Vec<(usize, usize)> = Vec::new();
    for check in &sorted {
        let rank = status(&check.status).0;
        match counts.last_mut() {
            Some((last, count)) if *last == rank => *count += 1,
            _ => counts.push((rank, 1)),
        }
    }
    let mut header = vec![heading("Checks")];
    for (index, (rank, count)) in counts.iter().enumerate() {
        let (_, glyph, word, color) =
            STATUSES
                .get(*rank)
                .copied()
                .unwrap_or(("", "?", "unknown", Color::Gray));
        header.push(Span::styled(
            if index == 0 { "  " } else { " · " }.to_string(),
            dim(),
        ));
        header.push(Span::styled(
            format!("{glyph} {count} {word}"),
            Style::default().fg(color),
        ));
    }
    lines.push(Line::from(header));

    // The ones that are fine are counted and folded, so the ones that are not are in front;
    // `za` on the count lists them.
    let mut folded: Option<(&str, usize)> = None;
    for check in sorted {
        let (_, glyph, word, color) = status(&check.status);
        let quiet = match check.status.as_str() {
            "success" => Some(PASSED_KEY),
            "skipped" => Some(SKIPPED_KEY),
            _ => None,
        };
        if let Some(key) = quiet
            && folded.map(|(open_key, _)| open_key) != Some(key)
        {
            if let Some((key, first)) = folded.take() {
                rows.push(fold_region(key, first, lines.len()));
            }
            let count = detail
                .checks
                .iter()
                .filter(|c| c.status == check.status)
                .count();
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{glyph} {count} {word}"),
                    Style::default().fg(color),
                ),
                Span::styled(if open(key) { "" } else { "  za lists them" }, dim()),
            ]));
            folded = Some((key, lines.len() - 1));
        }
        if let Some((key, _)) = folded
            && !open(key)
        {
            continue;
        }
        let indent = if folded.is_some() { "  " } else { "" };
        let mut spans = vec![
            Span::raw(indent.to_string()),
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::raw(check.name.clone()),
        ];
        if let Some(description) = check
            .description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
        {
            spans.push(Span::styled(format!(" — {}", description.trim()), dim()));
        }
        if let Some(url) = &check.url {
            rows.push(Region {
                first: lines.len(),
                end: lines.len() + 1,
                key: format!("{CHECK_KEY}{url}"),
                foldable: false,
            });
        }
        lines.push(Line::from(spans));
    }
    if let Some((key, first)) = folded {
        rows.push(fold_region(key, first, lines.len()));
    }
    lines.push(Line::default());
    block("pr-checks", lines, rows)
}

fn fold_region(key: &str, first: usize, end: usize) -> Region {
    Region {
        first,
        end,
        key: key.to_string(),
        foldable: true,
    }
}

fn body(
    detail: &Detail,
    open: &dyn Fn(&str) -> bool,
    flipped: &dyn Fn(&str, bool) -> bool,
    size: (u16, u16),
    images: &HashMap<String, Option<String>>,
) -> Block {
    let mut lines = vec![Line::from(heading("Description"))];
    let mut rows = Vec::new();
    let said = prose(&detail.body);
    if said.is_empty() {
        lines.push(Line::from(Span::styled("No description.", dim())));
        lines.push(Line::default());
        return block("pr-body", lines, rows);
    }
    let drawn = draw_markdown("pr-body", &said, flipped, size, images);
    let total = drawn.lines.len();
    // Folded, it stops after its opening lines, or after the picture those lines run into:
    // half a picture is not a preview of one.
    let shown = if total > BODY_FOLD && !open(BODY_KEY) {
        drawn
            .rows
            .iter()
            .filter(|region| !region.foldable && region.first < BODY_SHOWN)
            .map(|region| region.end)
            .fold(BODY_SHOWN, usize::max)
            .min(total)
    } else {
        total
    };
    // The heading is the block's first line, so everything drawn moves down by one.
    let offset = lines.len();
    lines.extend(drawn.lines.into_iter().take(shown));
    if shown < total {
        lines.push(Line::from(Span::styled(
            format!("… {} more lines · za unfolds", total - shown),
            dim(),
        )));
    }
    if total > BODY_FOLD {
        rows.push(fold_region(BODY_KEY, 0, lines.len()));
    }
    rows.extend(
        drawn
            .rows
            .into_iter()
            .filter(|region| region.first < shown)
            .map(|region| Region {
                first: region.first + offset,
                end: region.end.min(shown) + offset,
                ..region
            }),
    );
    lines.push(Line::default());
    let mut block = block("pr-body", lines, rows);
    block.images = drawn
        .images
        .into_iter()
        .filter(|placed| placed.line < shown)
        .map(|placed| crate::timeline::Placed {
            line: placed.line + offset,
            ..placed
        })
        .collect();
    block.pictures = drawn.pictures;
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail(checks: serde_json::Value) -> Detail {
        serde_json::from_value(json!({
            "repository": "o/r", "number": 42, "title": "Add the thing", "body": "Why.",
            "url": "https://github.com/o/r/pull/42", "author": {"login": "someone"},
            "state": "open", "isDraft": false, "mergeability": "conflicting",
            "additions": 3, "deletions": 1, "changedFiles": 1,
            "headBranch": "b", "baseBranch": "a",
            "labels": [{"name": "bug", "color": "d73a4a"}],
            "checks": checks,
        }))
        .unwrap()
    }

    fn text(block: &Block) -> Vec<String> {
        block
            .text
            .lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// The ones in trouble come first and stand alone; the ones that passed are a count
    /// that unfolds.
    #[test]
    fn failing_checks_lead_and_passing_ones_fold() {
        let detail = detail(json!([
            {"name": "lint", "status": "success", "url": "https://ci/lint"},
            {"name": "build", "status": "failure", "description": "exit 1", "url": "https://ci/build"},
            {"name": "test", "status": "success", "url": "https://ci/test"},
        ]));
        let shut = checks(&detail, &|_| false);
        assert_eq!(
            text(&shut),
            vec![
                "Checks  ✗ 1 failing · ✓ 2 passed",
                "✗ build — exit 1",
                "✓ 2 passed  za lists them",
                "",
            ]
        );
        assert!(
            shut.rows
                .iter()
                .any(|row| row.first == 1 && row.key == "pr-check:https://ci/build"),
            "the failing row opens its check"
        );

        let open = checks(&detail, &|key| key == PASSED_KEY);
        assert_eq!(text(&open)[2..5], ["✓ 2 passed", "  ✓ lint", "  ✓ test"]);
        let fold = open.rows.iter().find(|row| row.key == PASSED_KEY).unwrap();
        assert_eq!((fold.first, fold.end), (2, 5));
    }

    /// A layer of a stack says where it sits: every layer by number, bottom to top, in the
    /// colour of where it stands, and the one being read picked out.
    #[test]
    fn a_stack_is_drawn_across_the_top() {
        let layers: Vec<serde_json::Value> = [1u64, 2, 3]
            .into_iter()
            .map(|n| {
                json!({
                    "host": "github.com", "repository": "o/r", "number": n,
                    "url": format!("https://github.com/o/r/pull/{n}"), "source": "agent",
                    "linkedAt": "", "snapshot": {
                        "state": if n == 1 { "merged" } else { "open" }, "isDraft": n == 3,
                        "title": "t", "headBranch": format!("b{n}"),
                        "baseBranch": if n == 1 { "main".to_string() } else { format!("b{}", n - 1) },
                    }
                })
            })
            .collect();
        let shell: crate::model::ThreadShell = serde_json::from_value(json!({
            "id": "t", "projectId": "p", "title": "T",
            "modelSelection": {"instanceId": "c", "model": "m"},
            "pullRequests": layers,
        }))
        .unwrap();
        let chains = shell.pull_request_chains();
        let lines = text(&summary(&detail(json!([])), Some((&chains[0].layers, 1))));
        assert_eq!(
            lines[0],
            "stack 2/3  ◆ #1 › ● #2 › ◌ #3   [ ] move through it"
        );
        assert_eq!(lines[1], "#42 Add the thing");
    }

    /// Labels can be changed where the host can and the viewer has not been told otherwise;
    /// a server that says nothing about labels cannot.
    #[test]
    fn labels_are_editable_where_host_and_viewer_allow() {
        let mut pr = detail(json!([]));
        assert!(!pr.labels_editable(), "nothing said is nothing offered");
        pr.capabilities = Some(Capabilities { labels: Some(true) });
        assert!(
            pr.labels_editable(),
            "a permission not mentioned is granted"
        );
        pr.viewer_permissions = Some(Permissions {
            labels: Some(false),
        });
        assert!(!pr.labels_editable());
    }

    #[test]
    fn the_summary_says_what_stands_in_the_way() {
        let lines = text(&summary(&detail(json!([])), None));
        assert_eq!(lines[0], "#42 Add the thing");
        assert_eq!(lines[1], "open · @someone · b → a");
        assert!(lines.contains(&"⚠ conflicts with a".to_string()));
        assert!(lines.contains(&" bug ".to_string()));
    }

    /// GitHub writes a dropped picture as a tag; it is read as the image it is, and a tag
    /// in code, or an attribute that only ends in `src`, is left as written.
    #[test]
    fn img_tags_are_read_as_images() {
        let body = "Before\n<img width=\"300\" alt=\"Shot\" src=\"https://github.com/user-attachments/assets/abc\">\n\
            <img data-src=\"x\">\n```html\n<img src=\"https://a/b.png\">\n```\n";
        assert_eq!(
            markdown_images(body),
            "Before\n![Shot](<https://github.com/user-attachments/assets/abc>)\n\
            <img data-src=\"x\">\n```html\n<img src=\"https://a/b.png\">\n```\n"
        );
        let mut with_image = detail(json!([]));
        with_image.body = body.to_string();
        assert_eq!(
            image_urls(&with_image, None),
            vec!["https://github.com/user-attachments/assets/abc".to_string()]
        );
    }

    /// Until a picture has come back its caption says so, and one that could not be
    /// fetched says that instead.
    #[test]
    fn a_picture_on_its_way_says_so() {
        let mut with_image = detail(json!([]));
        with_image.body = "![Shot](https://example.com/shot.png)".to_string();
        let caption = |images: &HashMap<String, Option<String>>| {
            text(&body(&with_image, &|_| false, &|_, o| o, (80, 24), images))[1].clone()
        };
        assert!(caption(&HashMap::new()).ends_with(" · loading…"));
        let failed = HashMap::from([("https://example.com/shot.png".to_string(), None)]);
        assert!(caption(&failed).ends_with(" · could not be fetched"));
    }

    fn review(login: &str, state: &str, body: &str, at: &str) -> serde_json::Value {
        json!({"kind": "review", "author": {"login": login}, "body": body, "createdAt": at,
            "url": format!("https://github.com/o/r/pull/42#{login}-{at}"), "reviewState": state})
    }

    fn activity(comments: serde_json::Value) -> Activity {
        serde_json::from_value(json!({"comments": comments,
            "reviewThreads": [{"isResolved": false}, {"isResolved": true}]}))
        .unwrap()
    }

    const NOW: &str = "2026-01-10T00:00:00Z";

    /// A reviewer stands where their last approval or request for changes put them, and a
    /// later comment does not take that back. Somebody asked again is waiting, and says what
    /// they said before; the author answering in the thread is not a reviewer.
    #[test]
    fn reviewers_stand_where_their_verdicts_put_them() {
        let mut detail = detail(json!([]));
        detail.reviewers = vec![
            Actor {
                login: "carol".into(),
            },
            Actor {
                login: "dan".into(),
            },
        ];
        let activity = activity(json!([
            review("alice", "APPROVED", "", "2026-01-01T00:00:00Z"),
            review("alice", "COMMENTED", "one nit", "2026-01-02T00:00:00Z"),
            review(
                "bob",
                "CHANGES_REQUESTED",
                "Please split this.",
                "2026-01-08T00:00:00Z"
            ),
            review("carol", "APPROVED", "ok", "2026-01-03T00:00:00Z"),
            review("someone", "COMMENTED", "fixed", "2026-01-09T00:00:00Z"),
            review("eve", "PENDING", "draft", "2026-01-09T00:00:00Z"),
        ]));
        let lines = text(&reviews(
            &detail,
            Some(&Ok(activity)),
            &|_, open| open,
            NOW,
            (80, 24),
            &HashMap::new(),
        ));
        assert_eq!(
            lines,
            vec![
                "Reviews  ✗ 1 changes requested · ○ 2 waiting · ✓ 1 approved · 1 unresolved thread",
                "✗ @bob requested changes · 2d ago ▾",
                "  Please split this.",
                "○ @carol review requested · earlier approved · 7d ago ▸",
                "○ @dan review requested",
                "✓ @alice approved · 8d ago ▸",
                "",
            ]
        );
    }

    /// The latest review is open to begin with, so `za` on it shuts it; the others open.
    #[test]
    fn only_the_latest_review_starts_open() {
        let detail = detail(json!([]));
        let activity = activity(json!([
            review("alice", "APPROVED", "Looks good.", "2026-01-01T00:00:00Z"),
            review("bob", "COMMENTED", "Why this way?", "2026-01-05T00:00:00Z"),
        ]));
        let shut_by_hand = |key: &str, open: bool| open != (key == "pr-review:bob");
        let lines = text(&reviews(
            &detail,
            Some(&Ok(activity.clone())),
            &shut_by_hand,
            NOW,
            (80, 24),
            &HashMap::new(),
        ));
        assert!(!lines.iter().any(|l| l.contains("Why this way?")));
        let opened = |key: &str, open: bool| open != (key == "pr-review:alice");
        let lines = text(&reviews(
            &detail,
            Some(&Ok(activity.clone())),
            &opened,
            NOW,
            (80, 24),
            &HashMap::new(),
        ));
        assert!(lines.iter().any(|l| l.contains("Looks good.")));
        assert_eq!(
            link_at("pr-review:alice", &detail, Some(&activity)).as_deref(),
            Some("https://github.com/o/r/pull/42#alice-2026-01-01T00:00:00Z")
        );
    }

    #[test]
    fn reviews_on_their_way_say_so() {
        let detail = detail(json!([]));
        assert_eq!(
            text(&reviews(
                &detail,
                None,
                &|_, o| o,
                NOW,
                (80, 24),
                &HashMap::new()
            ))[0],
            "Reviews  loading…"
        );
        let failed = Err("gh is not signed in".to_string());
        assert_eq!(
            text(&reviews(
                &detail,
                Some(&failed),
                &|_, o| o,
                NOW,
                (80, 24),
                &HashMap::new()
            ))[0],
            "Reviews  could not be read: gh is not signed in"
        );
    }

    /// Comments are what the host does not show, wherever they are and however long; one
    /// written in code is code.
    #[test]
    fn html_comments_are_not_shown() {
        let body = "<!-- template: say why -->\nWhy.\n\nMore <!-- inline --> text.\n\n\
            <!--\nlong\ninstructions\n-->\n\n```\n<!-- kept -->\n```\n\nand `<!-- kept -->`\n";
        assert_eq!(
            prose(body),
            "Why.\n\nMore  text.\n\n\n\n```\n<!-- kept -->\n```\n\nand `<!-- kept -->`"
        );
        assert_eq!(prose("<!-- only a template -->"), "");
        // Opening a block of its own, it runs to the end; inside a paragraph it is not a
        // comment at all, and the host shows it as text too.
        assert_eq!(prose("before\n\n<!-- never closed\nafter"), "before");
        assert_eq!(prose("a <!-- b"), "a <!-- b");
    }

    /// A `<details>` is its summary until `za` opens it, unless it was written open; one
    /// inside it folds on its own.
    #[test]
    fn details_fold_to_their_summary() {
        let source = "Intro.\n\n<details><summary><b>Walkthrough</b></summary>\n\nThe steps.\n\n\
            <details open>\n<summary>Inner</summary>\n\nDeep.\n</details>\n</details>\n\nAfter.\n";
        let lines = |open: &dyn Fn(&str, bool) -> bool| -> Vec<String> {
            let drawn = draw_markdown("b", source, open, (80, 24), &HashMap::new());
            drawn
                .lines
                .iter()
                .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        assert_eq!(
            lines(&|_, open| open),
            vec!["Intro.", "▸ Walkthrough", "After."]
        );
        let opened = lines(&|key, open| open != (key == "b/details/1"));
        assert_eq!(
            opened,
            vec![
                "Intro.",
                "▾ Walkthrough",
                "  The steps.",
                "  ▾ Inner",
                "    Deep.",
                "After."
            ]
        );
        let drawn = draw_markdown("b", source, &|_, open| open, (80, 24), &HashMap::new());
        let fold = drawn.rows.iter().find(|r| r.key == "b/details/1").unwrap();
        assert!(fold.foldable && (fold.first, fold.end) == (1, 2));
    }

    #[test]
    fn a_long_description_folds() {
        let mut long = detail(json!([]));
        long.body = (1..=40).map(|n| format!("line {n}\n\n")).collect();
        let shut = body(&long, &|_| false, &|_, o| o, (80, 24), &HashMap::new());
        assert!(
            text(&shut)
                .iter()
                .any(|line| line.contains("more lines · za unfolds"))
        );
        let open = body(&long, &|_| true, &|_, o| o, (80, 24), &HashMap::new());
        assert!(text(&open).iter().any(|line| line == "line 40"));
    }
}

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
    model::ThreadDetailSnapshot,
    state::ThreadState,
    timeline::{Block, BlockKey, Region},
};

/// The region key a check row is known by, for `gx` to find its link.
pub const CHECK_KEY: &str = "pr-check:";
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

/// The view's blocks: where it stands, its checks, and what it says. `images` is what
/// has come back for the pictures the description points at, by their links.
pub fn blocks(
    detail: &Detail,
    expanded: &HashSet<String>,
    open_levels: u8,
    size: (u16, u16),
    images: &HashMap<String, Option<String>>,
) -> Vec<Block> {
    let open = |key: &str| open_levels > 0 || expanded.contains(key);
    vec![
        summary(detail),
        checks(detail, &open),
        body(detail, &open, size, images),
    ]
}

/// The pictures the description shows, to be fetched.
pub fn image_urls(detail: &Detail) -> Vec<String> {
    crate::timeline::web_images(&markdown_images(&detail.body))
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

fn summary(detail: &Detail) -> Block {
    let mut lines = vec![Line::from(Span::styled(
        format!("#{} {}", detail.number, detail.title),
        Style::default().add_modifier(Modifier::BOLD),
    ))];

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
    (width, height): (u16, u16),
    images: &HashMap<String, Option<String>>,
) -> Block {
    let mut lines = vec![Line::from(heading("Description"))];
    let mut rows = Vec::new();
    if detail.body.trim().is_empty() {
        lines.push(Line::from(Span::styled("No description.", dim())));
        lines.push(Line::default());
        return block("pr-body", lines, rows);
    }
    let source = markdown_images(detail.body.trim_end());
    let mut rendered = crate::timeline::markdown(&source);
    let (placed, regions, pictures) = crate::timeline::place_message_images(
        "pr-body",
        &source,
        &mut rendered,
        width,
        height,
        Some(images),
    );
    let rendered = rendered.lines;
    let total = rendered.len();
    // Folded, it stops after its opening lines, or after the picture those lines run into:
    // half a picture is not a preview of one.
    let shown = if total > BODY_FOLD && !open(BODY_KEY) {
        regions
            .iter()
            .filter(|region| region.first < BODY_SHOWN)
            .map(|region| region.end)
            .fold(BODY_SHOWN, usize::max)
            .min(total)
    } else {
        total
    };
    // The heading is the block's first line, so everything the description placed moves
    // down by one.
    let offset = lines.len();
    lines.extend(rendered.into_iter().take(shown));
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
        regions
            .into_iter()
            .filter(|region| region.end <= shown)
            .map(|region| Region {
                first: region.first + offset,
                end: region.end + offset,
                ..region
            }),
    );
    lines.push(Line::default());
    let mut block = block("pr-body", lines, rows);
    block.images = placed
        .into_iter()
        .filter(|placed| placed.line < shown)
        .map(|placed| crate::timeline::Placed {
            line: placed.line + offset,
            ..placed
        })
        .collect();
    block.pictures = pictures;
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

    #[test]
    fn the_summary_says_what_stands_in_the_way() {
        let lines = text(&summary(&detail(json!([]))));
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
            image_urls(&with_image),
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
            text(&body(&with_image, &|_| false, (80, 24), images))[1].clone()
        };
        assert!(caption(&HashMap::new()).ends_with(" · loading…"));
        let failed = HashMap::from([("https://example.com/shot.png".to_string(), None)]);
        assert!(caption(&failed).ends_with(" · could not be fetched"));
    }

    #[test]
    fn a_long_description_folds() {
        let mut long = detail(json!([]));
        long.body = (1..=40).map(|n| format!("line {n}\n\n")).collect();
        let shut = body(&long, &|_| false, (80, 24), &HashMap::new());
        assert!(
            text(&shut)
                .iter()
                .any(|line| line.contains("more lines · za unfolds"))
        );
        let open = body(&long, &|_| true, (80, 24), &HashMap::new());
        assert!(text(&open).iter().any(|line| line == "line 40"));
    }
}

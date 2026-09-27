//! The chat laid out: what it shows, broken into the rows it is drawn as at the chat's
//! size, and everything that is only known once it is — how many lines there are, what
//! each one says, where each message starts, which lines fold, and where the pictures went.
//!
//! It lays nothing out of its own accord. Asked to, it is lent a key for what is on the
//! screen and a way to build its blocks, and builds them only when the key or the size has
//! moved since it last did. So a motion moves through the chat as it is now rather than as
//! the last frame drew it, and the frame is painted from the same answer.
//!
//! The size is the renderer's: it says each frame how big the chat is, and the layout keeps
//! the last it was told. Before the first frame there is no size, and so no lines.

use std::{
    cell::OnceCell,
    collections::HashSet,
    hash::{Hash, Hasher},
};

use ratatui::text::Line;

use crate::{
    app::{Scroll, Search, line_matches},
    reader::{Reader, Source},
    state::ThreadState,
    timeline::{self, Block, BlockKey, Picture, Placed, Region, Wrapped},
};

/// What is laid out, by what names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    /// The thread's own conversation, by the thread's id.
    Thread(String),
    /// Something read in its place, by what it is of.
    Reading(Source),
}

/// Everything the blocks are built from but the size, which the layout has of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub subject: Subject,
    /// Moves each time what is laid out changes.
    pub revision: u64,
    /// The folds opened by hand, hashed, and how many levels stand open everywhere.
    pub expanded: u64,
    pub open_levels: u8,
    /// The minute a pull request was laid out in. It says how long ago each review was,
    /// which goes on changing when nothing else about it does. Nothing else laid out
    /// changes with the time, so nothing else is keyed by it.
    pub minute: Option<i64>,
}

impl Key {
    /// The key for what the chat shows: the open reading, or else the thread's own
    /// conversation. `None` with neither, when there is nothing to lay out.
    pub fn of<R>(
        reader: &Reader<R>,
        thread: Option<&ThreadState>,
        expanded: &HashSet<String>,
        open_levels: u8,
        minute: i64,
    ) -> Option<Self> {
        let (subject, revision, minute) = match reader.source() {
            Some(source) => {
                let minute = matches!(source, Source::PullRequest(_)).then_some(minute);
                (Subject::Reading(source), reader.revision(), minute)
            }
            None => {
                let thread = thread?;
                (
                    Subject::Thread(thread.id().to_string()),
                    thread.revision,
                    None,
                )
            }
        };
        Some(Self {
            subject,
            revision,
            expanded: hash_set(expanded),
            open_levels,
            minute,
        })
    }
}

fn hash_set(set: &HashSet<String>) -> u64 {
    let mut keys: Vec<&String> = set.iter().collect();
    keys.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    keys.hash(&mut hasher);
    hasher.finish()
}

/// One block, broken into rows.
struct Laid {
    /// The block's text broken into the rows it is drawn as.
    wrapped: Wrapped,
    exports: Vec<(String, String)>,
    /// Images in wrapped content lines relative to the block start.
    images: Vec<Placed>,
}

impl Laid {
    fn height(&self) -> usize {
        self.wrapped.lines.len()
    }
}

/// What of one block is on the screen, for the renderer to paint.
pub struct Shown<'a> {
    /// The block's rows that are in view, already broken to the width.
    pub lines: &'a [Line<'static>],
    /// How many of its rows are above the view.
    pub skip: usize,
    /// Its pictures, on rows counted from the block's first.
    pub images: &'a [Placed],
}

#[derive(Default)]
pub struct ChatLayout {
    /// The chat's width and height as it was last drawn.
    size: Option<(u16, u16)>,
    /// What the blocks were built from, with the width and picture rows they were built at.
    built: Option<(Key, u16, u16)>,
    blocks: Vec<Laid>,
    total: usize,
    /// First content line of every message block, for `{` and `}`.
    message_starts: Vec<usize>,
    /// Content-line range of every block with its export key.
    block_ranges: Vec<(usize, usize, String)>,
    /// Every region a key folds, in content lines.
    work_ranges: Vec<Region>,
    /// Content-line range of every picture a message's own markdown put in the chat. A
    /// work row is found by the region it folds; a message folds nothing, so its pictures
    /// are found by the lines they were given.
    picture_ranges: Vec<Region>,
    /// Every picture the open rows have, under the key the row that has it is keyed by.
    pictures: Vec<(String, Picture)>,
    /// Every content line as displayed, filled on demand for search.
    lines: OnceCell<Vec<String>>,
    /// How many times the blocks have been built, for tests to see a rebuild.
    #[cfg(test)]
    builds: usize,
}

impl ChatLayout {
    // ── Laying out ─────────────────────────────────────────────────────

    /// Be told how big the chat is. What was laid out at another size is laid out again
    /// the next time it is asked for.
    pub fn resize(&mut self, width: u16, height: u16) {
        self.size = Some((width, height));
    }

    /// Lay out what `key` names, building its blocks at the chat's width and height with
    /// `build` if it is not what is laid out already. Without a size there is nowhere to
    /// lay anything out, and the chat has no lines.
    pub fn lay_out(&mut self, key: Key, build: impl FnOnce(u16, u16) -> Vec<Block>) {
        let Some((width, height)) = self.size else {
            self.clear();
            return;
        };
        // The height only bounds how much of the window a picture may take, so a height
        // that leaves pictures the same room leaves everything else the same too.
        let built = (key, width, timeline::picture_rows(height));
        if self.built.as_ref() == Some(&built) {
            return;
        }
        self.fill(build(width, height), width);
        self.built = Some(built);
    }

    /// Lay out nothing, as when no thread is open.
    pub fn clear(&mut self) {
        if self.built.is_none() && self.blocks.is_empty() {
            return;
        }
        self.fill(Vec::new(), 1);
        self.built = None;
    }

    fn fill(&mut self, blocks: Vec<Block>, width: u16) {
        #[cfg(test)]
        {
            self.builds += 1;
        }
        self.blocks.clear();
        self.message_starts.clear();
        self.block_ranges.clear();
        self.work_ranges.clear();
        self.picture_ranges.clear();
        self.pictures.clear();
        self.lines = OnceCell::new();
        let mut y = 0usize;
        for mut block in blocks {
            let exports = std::mem::take(&mut block.exports);
            let wrapped = timeline::wrap(&block.text, width);
            // Where each line of the text starts turns text-line row ranges into
            // content lines, and says where an image's reserved lines landed.
            let (rows, images) = if block.rows.is_empty() {
                (Vec::new(), Vec::new())
            } else {
                let starts = &wrapped.starts;
                let rows: Vec<Region> = block
                    .rows
                    .iter()
                    .map(|region| Region {
                        first: starts[region.first],
                        end: starts[region.end.min(starts.len() - 1)],
                        ..region.clone()
                    })
                    .collect();
                let images = block
                    .images
                    .iter()
                    .map(|placed| Placed {
                        line: starts[placed.line.min(starts.len() - 1)],
                        ..placed.clone()
                    })
                    .collect();
                (rows, images)
            };
            let start = y;
            let end = y + wrapped.lines.len();
            y = end;
            let at = |region: &Region| Region {
                first: start + region.first,
                end: start + region.end,
                ..region.clone()
            };
            if matches!(block.key, BlockKey::Message(_) | BlockKey::Section(_)) {
                self.message_starts.push(start);
            }
            if let Some((key, _)) = exports.first() {
                self.block_ranges.push((start, end, key.clone()));
            }
            self.pictures.append(&mut block.pictures);
            match &block.key {
                BlockKey::Message(_) => self.picture_ranges.extend(rows.iter().map(at)),
                BlockKey::Section(_) => self.work_ranges.extend(rows.iter().map(at)),
                BlockKey::Work(key) => {
                    self.work_ranges.push(Region {
                        first: start,
                        end,
                        key: key.clone(),
                        foldable: true,
                    });
                    self.work_ranges.extend(rows.iter().map(at));
                }
                _ => {}
            }
            self.blocks.push(Laid {
                wrapped,
                exports,
                images,
            });
        }
        self.total = y;
    }

    #[cfg(test)]
    pub fn builds(&self) -> usize {
        self.builds
    }

    // ── The view ───────────────────────────────────────────────────────

    /// How many content lines the chat has.
    pub fn total(&self) -> usize {
        self.total
    }

    /// How many lines the chat shows at once. None before it has been drawn.
    pub fn height(&self) -> usize {
        self.size.map_or(0, |(_, height)| usize::from(height))
    }

    /// The first content line in view. Following the end is being scrolled as far down
    /// as the chat goes, and an offset past that is as far as it goes.
    pub fn offset(&self, scroll: Scroll) -> usize {
        let most = self.total.saturating_sub(self.height());
        match scroll {
            Scroll::Follow => most,
            Scroll::Offset(offset) => offset.min(most),
        }
    }

    /// Where the chat cursor is, kept on a line the chat has: the last one while the view
    /// follows new output.
    pub fn cursor(&self, scroll: Scroll, cursor: usize) -> usize {
        let last = self.total.saturating_sub(1);
        match scroll {
            Scroll::Follow => last,
            Scroll::Offset(_) => cursor.min(last),
        }
    }

    /// What of each block is in view from `offset` down, top to bottom.
    pub fn shown(&self, offset: usize) -> Vec<Shown<'_>> {
        let mut room = self.height();
        let mut shown = Vec::new();
        let mut y = 0usize;
        for laid in &self.blocks {
            let start = y;
            y += laid.height();
            if y <= offset {
                continue;
            }
            if room == 0 {
                break;
            }
            let skip = offset.saturating_sub(start);
            let visible = (laid.height() - skip).min(room);
            shown.push(Shown {
                lines: &laid.wrapped.lines[skip..skip + visible],
                skip,
                images: &laid.images,
            });
            room -= visible;
        }
        shown
    }

    // ── Lines ──────────────────────────────────────────────────────────

    /// The block a content line belongs to, and which of its rows the line is.
    fn locate(&self, line: usize) -> Option<(&Laid, usize)> {
        let mut y = 0usize;
        for laid in &self.blocks {
            if line < y + laid.height() {
                return Some((laid, line - y));
            }
            y += laid.height();
        }
        None
    }

    /// What a content line says, without the decoration it is drawn with.
    pub fn row(&self, line: usize) -> &str {
        self.locate(line)
            .map_or("", |(laid, row)| laid.wrapped.text(row))
    }

    /// How many characters a content line can be addressed by, its decoration not counted.
    pub fn len(&self, line: usize) -> usize {
        self.locate(line)
            .map_or(0, |(laid, row)| laid.wrapped.len(row))
    }

    /// A line and a character on it, the character kept on the line. A column is held
    /// where it was put, so passing a short line does not pull the cursor left for good.
    pub fn spot(&self, line: usize, column: usize) -> (usize, usize) {
        (line, column.min(self.len(line).saturating_sub(1)))
    }

    /// The character of a content line drawn at a column of the chat, for a click or a drag.
    pub fn index(&self, line: usize, column: u16) -> usize {
        self.locate(line)
            .map_or(0, |(laid, row)| laid.wrapped.index(row, column))
    }

    /// Where a character of a content line is drawn, as a column of the chat.
    pub fn column(&self, line: usize, index: usize) -> u16 {
        self.locate(line)
            .map_or(0, |(laid, row)| laid.wrapped.column(row, index))
    }

    /// The chat text from one point to another, each a content line and a character on it,
    /// the end exclusive. What comes back is what was written rather than what was drawn:
    /// the marks and indents the chat decorates its lines with are left out, and a line
    /// broken over several rows comes back as the one line it was, spaces and all.
    pub fn span(&self, start: (usize, usize), end: (usize, usize)) -> Option<String> {
        // The pieces to take, each a byte range of one line of one block. Rows of the
        // same line join into one piece, which puts back the space a break swallowed.
        let mut pieces: Vec<(usize, usize, usize, usize)> = Vec::new();
        let mut y = 0usize;
        for (index, laid) in self.blocks.iter().enumerate() {
            let block_start = y;
            y += laid.height();
            if y <= start.0 || block_start > end.0 {
                continue;
            }
            for (row, at) in laid.wrapped.rows.iter().enumerate() {
                let line = block_start + row;
                if line < start.0 || line > end.0 {
                    continue;
                }
                let from = if line == start.0 {
                    laid.wrapped.byte(row, start.1)
                } else {
                    at.start
                };
                let to = if line == end.0 {
                    laid.wrapped.byte(row, end.1)
                } else {
                    at.end
                };
                match pieces.last_mut() {
                    Some(last) if (last.0, last.1) == (index, at.line) => last.3 = to.max(last.3),
                    _ => pieces.push((index, at.line, from, to)),
                }
            }
        }
        if pieces.is_empty() {
            return None;
        }
        Some(
            pieces
                .iter()
                .map(|&(block, line, from, to)| {
                    &self.blocks[block].wrapped.texts[line][from..to.max(from)]
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    /// Every content line as displayed, marks and indents and all, kept from one layout to
    /// the next for search to read.
    fn lines(&self) -> &[String] {
        self.lines.get_or_init(|| {
            let mut lines = Vec::with_capacity(self.total);
            for laid in &self.blocks {
                lines.extend(laid.wrapped.lines.iter().map(|line| {
                    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                    text.trim_end().to_string()
                }));
            }
            lines
        })
    }

    /// The next matching content line after (or before) `from`, wrapping around the
    /// conversation. Returns the line and whether the search wrapped.
    pub fn find(&self, search: &Search, from: usize) -> Option<(usize, bool)> {
        let lines = self.lines();
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

    // ── What is where ──────────────────────────────────────────────────

    /// First content line of every message block, top to bottom.
    pub fn message_starts(&self) -> &[usize] {
        &self.message_starts
    }

    /// The export key of the block a content line is in.
    pub fn block_at(&self, line: usize) -> Option<&str> {
        self.block_ranges
            .iter()
            .find(|(start, end, _)| *start <= line && line < *end)
            .map(|(_, _, key)| key.as_str())
    }

    /// The content lines of the block exported under `key`, the end exclusive.
    pub fn block_lines(&self, key: &str) -> Option<(usize, usize)> {
        self.block_ranges
            .iter()
            .find(|(_, _, k)| k == key)
            .map(|&(start, end, _)| (start, end))
    }

    /// Every region a key folds, top to bottom.
    pub fn work_ranges(&self) -> &[Region] {
        &self.work_ranges
    }

    /// The tightest toggle region covering a content line, with whether folding it shows
    /// anything: a tool row inside an expanded group wins over the group itself, even
    /// when the row has nothing to unfold — folding its group instead is not what the
    /// click asked for.
    pub fn region_at(&self, line: usize) -> Option<(&str, bool)> {
        self.work_ranges
            .iter()
            .filter(|region| region.first <= line && line < region.end)
            .min_by_key(|region| region.end - region.first)
            .map(|region| (region.key.as_str(), region.foldable))
    }

    /// The picture on a content line: the one the row it is in has, open or shut, or the
    /// one a message drew there. A line is inside the row wherever it is on the picture
    /// itself, since the lines it was drawn over belong to the row that opened it.
    pub fn picture_at(&self, line: usize) -> Option<&Picture> {
        let key = match self.region_at(line) {
            Some((key, _)) => key,
            None => self.picture_range_at(line)?,
        };
        self.pictures
            .iter()
            .find(|(row, _)| row == key)
            .map(|(_, picture)| picture)
    }

    /// What a message's picture is known by, where one was drawn on this line. The
    /// smallest range wins, so two pictures on one line are told apart by the lines they
    /// were each drawn on.
    fn picture_range_at(&self, line: usize) -> Option<&str> {
        self.picture_ranges
            .iter()
            .filter(|region| region.first <= line && line < region.end)
            .min_by_key(|region| region.end - region.first)
            .map(|region| region.key.as_str())
    }

    /// The content lines of every picture a message drew, under what each is known by.
    #[cfg(test)]
    pub fn picture_ranges(&self) -> &[Region] {
        &self.picture_ranges
    }

    // ── Export ─────────────────────────────────────────────────────────

    /// Plain text of a block or tool row by export key.
    pub fn export(&self, key: &str) -> Option<&str> {
        self.blocks
            .iter()
            .flat_map(|laid| laid.exports.iter())
            .find(|(k, _)| k == key)
            .map(|(_, text)| text.as_str())
    }

    /// Plain text of the whole conversation as currently loaded.
    pub fn export_all(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|laid| laid.exports.first().map(|(_, text)| text.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use ratatui::text::Text;
    use serde_json::json;

    use super::*;

    /// A conversation of one message from each side.
    fn thread(yours: &str, theirs: &str) -> ThreadState {
        let snapshot: crate::model::ThreadDetailSnapshot = serde_json::from_value(json!({
            "snapshotSequence": 1,
            "thread": {
                "id": "t1", "projectId": "p1", "title": "Test",
                "modelSelection": {"instanceId": "claudeAgent", "model": "m"},
                "runtimeMode": "full-access", "latestTurn": null, "session": null,
                "messages": [
                    {"id": "m1", "role": "user", "text": yours,
                        "createdAt": "2026-01-01T10:00:00Z"},
                    {"id": "m2", "role": "assistant", "text": theirs,
                        "createdAt": "2026-01-01T10:01:00Z"},
                ],
                "activities": []
            }
        }))
        .unwrap();
        ThreadState::from_snapshot(snapshot)
    }

    fn key(thread: &ThreadState) -> Key {
        Key {
            subject: Subject::Thread(thread.id().to_string()),
            revision: thread.revision,
            expanded: 0,
            open_levels: 0,
            minute: None,
        }
    }

    /// A layout of the thread at a size, built as the app builds it.
    fn laid_out(thread: &ThreadState, width: u16, height: u16) -> ChatLayout {
        let mut layout = ChatLayout::default();
        layout.resize(width, height);
        layout.lay_out(key(thread), |width, height| {
            timeline::build(thread, &HashSet::new(), 0, width, height)
        });
        layout
    }

    /// A block of `lines` lines of text, one row each.
    fn block(key: BlockKey, lines: usize) -> Block {
        Block {
            key,
            text: Text::from(
                (0..lines)
                    .map(|n| Line::from(n.to_string()))
                    .collect::<Vec<_>>(),
            ),
            rows: Vec::new(),
            exports: Vec::new(),
            images: Vec::new(),
            pictures: Vec::new(),
        }
    }

    const SAID: &str = "one two three four five six seven eight";

    /// Until the chat has been drawn it has no size, and so nothing on it: every question
    /// about a line is answered as though there were none.
    #[test]
    fn before_it_is_drawn_the_chat_has_no_lines() {
        let thread = thread(SAID, "fine");
        let mut layout = ChatLayout::default();
        layout.lay_out(key(&thread), |_, _| panic!("there is nowhere to build it"));
        assert_eq!((layout.total(), layout.height()), (0, 0));
        assert_eq!(layout.row(0), "");
        assert_eq!(layout.offset(Scroll::Follow), 0);
        assert_eq!(layout.cursor(Scroll::Follow, 5), 0);
    }

    /// Where each message starts, and what each line says, is what the width made of it:
    /// a narrower chat breaks a message over more rows and pushes the next one down.
    #[test]
    fn the_lines_are_the_rows_the_width_breaks_the_messages_into() {
        let thread = thread(SAID, "fine");
        let wide = laid_out(&thread, 80, 20);
        let narrow = laid_out(&thread, 30, 20);
        assert_eq!(wide.message_starts()[0], 0);
        assert!(narrow.total() > wide.total());
        assert!(narrow.message_starts()[1] > wide.message_starts()[1]);
        // The label is the block's first row; the message itself starts under it.
        assert_eq!(wide.row(1), SAID);
        assert_eq!(narrow.row(1).trim_end(), "one two three four five six");
        assert_eq!(narrow.row(2), "seven eight");
        assert_eq!(narrow.len(2), "seven eight".len());
        assert_eq!(narrow.block_at(2), Some("msg:m1"));
        let (start, end) = narrow.block_lines("msg:m2").unwrap();
        assert_eq!(start, narrow.message_starts()[1]);
        assert!((start..end).any(|line| narrow.row(line).contains("fine")));
        assert!(end <= narrow.total());
        // A column is kept on the line it is on.
        assert_eq!(narrow.spot(2, 99), (2, "seven eight".len() - 1));
    }

    /// What is taken out of the chat is what was written into it, not what was drawn: no
    /// marks, and a message broken over rows comes back as the one line it was.
    #[test]
    fn what_is_yanked_is_the_message_and_not_its_decoration() {
        let layout = laid_out(&thread(SAID, "fine"), 28, 14);
        let start = layout.message_starts()[0];
        let text = layout
            .span((start + 1, 0), (start + 2, usize::MAX))
            .unwrap();
        assert_eq!(text, SAID);
        // And a piece of a row is only that piece.
        assert_eq!(
            layout.span((start + 1, 4), (start + 1, 7)),
            Some("two".into())
        );
        // A click on a character lands on that character, the mark in front of it not
        // counted, and the character is drawn where the click was.
        let at = layout.column(start + 1, 4);
        assert_eq!(layout.index(start + 1, at), 4);
    }

    #[test]
    fn a_block_is_exported_by_its_key_and_the_whole_chat_in_order() {
        let layout = laid_out(&thread("a question", "an answer"), 60, 20);
        assert!(layout.export("msg:m1").unwrap().contains("a question"));
        assert_eq!(layout.export("msg:nobody"), None);
        let all = layout.export_all();
        let (question, answer) = (all.find("a question"), all.find("an answer"));
        assert!(question.is_some() && question < answer, "{all}");
    }

    /// Search reads the lines as they are drawn and goes round the end to the other side,
    /// saying when it did.
    #[test]
    fn search_finds_the_next_line_that_matches_and_wraps() {
        let layout = laid_out(&thread("find the needle", "no Needle here"), 60, 20);
        let search = |query: &str, backward| Search {
            query: query.into(),
            backward,
        };
        let (first, _) = layout.find(&search("needle", false), 0).unwrap();
        assert!(layout.row(first).contains("find the needle"));
        let (second, wrapped) = layout.find(&search("needle", false), first).unwrap();
        assert!(layout.row(second).contains("no Needle here") && !wrapped);
        assert_eq!(
            layout.find(&search("needle", false), second),
            Some((first, true))
        );
        assert_eq!(
            layout.find(&search("needle", true), second),
            Some((first, false))
        );
        // A capital asks for that case.
        assert_eq!(
            layout.find(&search("Needle", false), 0),
            Some((second, false))
        );
        assert_eq!(layout.find(&search("haystack", false), 0), None);
    }

    /// The blocks are built again only when what they are built from moved: the key, the
    /// width, or the room a picture is given. A height that leaves pictures the same room
    /// is the same layout.
    #[test]
    fn the_blocks_are_built_again_only_when_what_they_are_built_from_moves() {
        let thread = thread(SAID, "fine");
        let mut layout = laid_out(&thread, 60, 20);
        let lay_out = |layout: &mut ChatLayout, key: Key| {
            layout.lay_out(key, |width, height| {
                timeline::build(&thread, &HashSet::new(), 0, width, height)
            })
        };
        assert_eq!(layout.builds(), 1);
        lay_out(&mut layout, key(&thread));
        assert_eq!(layout.builds(), 1, "nothing moved");
        layout.resize(60, 21);
        lay_out(&mut layout, key(&thread));
        assert_eq!(layout.builds(), 1, "pictures are given ten rows either way");
        layout.resize(60, 30);
        lay_out(&mut layout, key(&thread));
        assert_eq!(layout.builds(), 2, "and fifteen here");
        layout.resize(50, 30);
        lay_out(&mut layout, key(&thread));
        assert_eq!(layout.builds(), 3);
        lay_out(
            &mut layout,
            Key {
                revision: 9,
                ..key(&thread)
            },
        );
        assert_eq!(layout.builds(), 4);
        layout.clear();
        assert_eq!(layout.total(), 0);
    }

    /// Following the end is being as far down as the chat goes, with the cursor on its
    /// last line; an offset is held to what the chat has.
    #[test]
    fn following_is_the_bottom_of_the_chat() {
        let mut layout = ChatLayout::default();
        layout.resize(40, 4);
        layout.lay_out(key(&thread("", "")), |_, _| {
            vec![block(BlockKey::Message("m1".into()), 10)]
        });
        assert_eq!(layout.offset(Scroll::Follow), 6);
        assert_eq!(layout.offset(Scroll::Offset(2)), 2);
        assert_eq!(layout.offset(Scroll::Offset(50)), 6);
        assert_eq!(layout.cursor(Scroll::Follow, 0), 9);
        assert_eq!(layout.cursor(Scroll::Offset(0), 3), 3);
        assert_eq!(layout.cursor(Scroll::Offset(0), 30), 9);
    }

    /// What is painted is the rows in view, block by block, with how much of each is
    /// above the top so its pictures land where its lines did.
    #[test]
    fn what_is_shown_is_the_rows_in_view() {
        let mut layout = ChatLayout::default();
        layout.resize(40, 5);
        layout.lay_out(key(&thread("", "")), |_, _| {
            vec![
                block(BlockKey::Message("m1".into()), 4),
                block(BlockKey::Message("m2".into()), 4),
                block(BlockKey::Message("m3".into()), 4),
            ]
        });
        let shown = layout.shown(2);
        let parts: Vec<(usize, usize)> = shown.iter().map(|s| (s.lines.len(), s.skip)).collect();
        assert_eq!(parts, [(2, 2), (3, 0)]);
        assert_eq!(shown[0].lines[0].to_string(), "2");
    }

    /// `gx` on a row with a picture opens the picture, which is the one thing on a chat
    /// line that is really somewhere else: the terminal only ever drew a thumbnail.
    #[test]
    fn the_picture_on_a_line_is_the_one_its_row_or_message_has() {
        let region = |first, end, key: &str| Region {
            first,
            end,
            key: key.into(),
            foldable: true,
        };
        let shot = Picture::File("/tmp/shot.png".into());
        let shown = Picture::File("/tmp/shown.png".into());
        let mut layout = ChatLayout::default();
        layout.resize(40, 40);
        layout.lay_out(key(&thread("", "")), |_, _| {
            vec![
                Block {
                    rows: vec![region(2, 8, "work-1/t1")],
                    pictures: vec![("work-1/t1".into(), shot.clone())],
                    ..block(BlockKey::Work("work-1".into()), 9)
                },
                block(BlockKey::Message("m0".into()), 11),
                Block {
                    rows: vec![region(0, 6, "msg:m1/0")],
                    pictures: vec![("msg:m1/0".into(), shown.clone())],
                    ..block(BlockKey::Message("m1".into()), 8)
                },
            ]
        });
        // Anywhere in the row, including the lines the picture was drawn over.
        assert_eq!(layout.picture_at(5), Some(&shot));
        // The group around it is not the row, and has no picture of its own.
        assert_eq!(layout.region_at(1), Some(("work-1", true)));
        assert_eq!(layout.picture_at(1), None);
        // A picture a message drew belongs to no row at all: it answers for the caption
        // and the lines under it, and `gx` opens it from any of them.
        for line in [20, 25] {
            assert_eq!(layout.picture_at(line), Some(&shown), "line {line}");
        }
        assert_eq!(layout.picture_at(26), None, "and no further");
    }
}

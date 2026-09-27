//! The chat as it is read: the conversation laid out at the chat's size, and where the
//! reader is in it — how far it is scrolled, the line and character the cursor is on, what
//! is selected, a count being typed, and the search `n` goes on with.
//!
//! Every motion is here, so what `j` or `}` or `n` does can be tried on blocks at a size
//! without an app. The app decodes keys into these calls, and does whatever a motion says
//! it came to: a toast, the clipboard, older turns asked for, the focus. The folds are the
//! app's rather than the view's: opening one reads files, and the reader shares them.
//!
//! What is laid out is lent per call, as the layout is: the app hands over a key and a way
//! to build the blocks, and the view builds them only when either has moved. Every motion
//! assumes the app has just done so, which is what asking the app for the view does.

mod layout;

pub use layout::{Key, Layout, Shown};

use crate::{reader, timeline::Block};

/// How the chat is scrolled: following the end, where a thread still working writes what
/// is new, or held at a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scroll {
    #[default]
    Follow,
    Offset(usize),
}

/// An accepted chat search, reused by `n` and `N` and for highlighting.
#[derive(Debug, Clone)]
pub struct Search {
    pub query: String,
    pub backward: bool,
}

/// Where a visual selection in the chat began, and whether it takes whole lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub line: usize,
    pub column: usize,
    pub whole_lines: bool,
}

/// Where the reader is in the conversation: how it was scrolled and the line the cursor
/// was on. It is what is kept while something is read over the conversation, and put back
/// after. The character on the line is not kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    pub scroll: Scroll,
    pub cursor: usize,
}

/// Where a search went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// To the next line that matches, the way it was going.
    Found,
    /// Round the end of the chat and in from the other side, which is worth saying.
    Wrapped { backward: bool },
    /// Nowhere: no line matches.
    Missing,
}

/// What `y` took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Yank {
    pub text: String,
    /// Part of one line rather than whole lines.
    pub piece: bool,
}

/// The chat and where the reader is in it.
#[derive(Default)]
pub struct ChatView {
    layout: Layout,
    scroll: Scroll,
    /// Line the cursor is on, as a content line index. Tracks the last line while the view
    /// follows new output.
    cursor: usize,
    /// Character the cursor is on, held where it was so a shorter line in passing does not
    /// lose the column. `usize::MAX` is the end of whatever line it is on.
    column: usize,
    visual: Option<Anchor>,
    /// Count typed before a motion.
    count: Option<usize>,
    search: Option<Search>,
}

/// Lines kept between the cursor and the viewport edge while moving.
const SCROLLOFF: usize = 3;

impl ChatView {
    // ── Laying out ─────────────────────────────────────────────────────

    /// Be told how big the chat is, by the frame that draws it.
    pub fn resize(&mut self, width: u16, height: u16) {
        self.layout.resize(width, height);
    }

    /// Lay out what `key` names, building its blocks with `build` if it is not what is laid
    /// out already.
    pub fn lay_out(&mut self, key: Key, build: impl FnOnce(u16, u16) -> Vec<Block>) {
        self.layout.lay_out(key, build);
    }

    /// Lay out nothing, as when no thread is open.
    pub fn clear(&mut self) {
        self.layout.clear();
    }

    /// Keep the cursor on a line the chat has: the last one while the view follows new
    /// output. Done while the chat has the keys; elsewhere the cursor stays where it was
    /// left, for when they come back.
    pub fn settle(&mut self) {
        self.cursor = self.layout.cursor(self.scroll, self.cursor);
    }

    /// What is laid out, for what is on a line: its folds, pictures, and text.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    // ── Where the reader is ────────────────────────────────────────────

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The cursor as a line and the character it is on, the character kept on the line.
    pub fn spot(&self) -> (usize, usize) {
        self.layout.spot(self.cursor, self.column)
    }

    /// The first content line in view.
    pub fn offset(&self) -> usize {
        self.layout.offset(self.scroll)
    }

    /// How many lines the chat shows at once.
    pub fn height(&self) -> usize {
        self.layout.height()
    }

    /// How many content lines the chat has.
    pub fn total(&self) -> usize {
        self.layout.total()
    }

    pub fn visual(&self) -> Option<Anchor> {
        self.visual
    }

    pub fn search(&self) -> Option<&Search> {
        self.search.as_ref()
    }

    /// Whether nothing is half done: no selection, and no count waiting for its motion.
    pub fn is_idle(&self) -> bool {
        self.visual.is_none() && self.count.is_none()
    }

    /// Where the reader is, to be put back later.
    pub fn place(&self) -> Place {
        Place {
            scroll: self.scroll,
            cursor: self.cursor,
        }
    }

    /// Put the reader back where they were, with nothing selected or searched.
    pub fn restore(&mut self, place: Place) {
        self.scroll = place.scroll;
        self.cursor = place.cursor;
        self.visual = None;
        self.search = None;
    }

    /// Put the reader where something just opened says: at the top of it, or following its
    /// end, with nothing selected or searched. `Keep` leaves them where they were.
    pub fn open(&mut self, place: reader::Place) {
        match place {
            reader::Place::Keep => return,
            reader::Place::End => self.scroll = Scroll::Follow,
            reader::Place::Top => {
                self.scroll = Scroll::Offset(0);
                self.cursor = 0;
            }
        }
        self.visual = None;
        self.search = None;
    }

    /// Follow new output from here on.
    pub fn follow(&mut self) {
        self.scroll = Scroll::Follow;
    }

    // ── Painting ───────────────────────────────────────────────────────

    /// What of each block is in view, top to bottom.
    pub fn shown(&self) -> Vec<Shown<'_>> {
        self.layout.shown(self.offset())
    }

    /// The row of the view a content line is drawn on, where it is in view.
    fn row_in_view(&self, line: usize) -> Option<usize> {
        let offset = self.offset();
        (line >= offset && line - offset < self.height()).then(|| line - offset)
    }

    /// Where the cursor is drawn: the row of the view, and the column of its character.
    pub fn cursor_cell(&self) -> Option<(usize, u16)> {
        let (line, column) = self.spot();
        Some((self.row_in_view(line)?, self.layout.column(line, column)))
    }

    /// The visual selection as it is drawn: for each row of the view it covers, the columns
    /// from and to. A character-wise selection reaches through the character the cursor is
    /// on, and an empty line in it still shows that it is.
    pub fn selection(&self, width: u16) -> Vec<(usize, u16, u16)> {
        let Some(anchor) = self.visual else {
            return Vec::new();
        };
        let head = (anchor.line, anchor.column.min(self.layout.len(anchor.line)));
        let spot = self.spot();
        let (first, last) = if head <= spot {
            (head, spot)
        } else {
            (spot, head)
        };
        (first.0..=last.0)
            .filter_map(|line| {
                let row = self.row_in_view(line)?;
                if anchor.whole_lines {
                    return Some((row, 0, width));
                }
                let from = if line == first.0 {
                    self.layout.column(line, first.1)
                } else {
                    0
                };
                let to = if line == last.0 {
                    self.layout.column(line, last.1 + 1)
                } else {
                    self.layout.column(line, self.layout.len(line))
                };
                Some((row, from, to.max(from + 1)))
            })
            .collect()
    }

    // ── Counts ─────────────────────────────────────────────────────────

    /// A digit typed before a motion. A `0` with no count before it is not one: it is the
    /// motion to the start of the line, and this says so by taking nothing.
    pub fn digit(&mut self, digit: usize) -> bool {
        if digit == 0 && self.count.is_none() {
            return false;
        }
        let current = self.count.unwrap_or(0);
        self.count = Some((current * 10 + digit).min(100_000));
        true
    }

    pub fn clear_count(&mut self) {
        self.count = None;
    }

    fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1).max(1)
    }

    /// Let go of a selection or a count half typed, saying whether there was either.
    pub fn cancel(&mut self) -> bool {
        let busy = !self.is_idle();
        self.visual = None;
        self.count = None;
        busy
    }

    // ── Motions ────────────────────────────────────────────────────────

    /// Place the cursor and scroll just enough to keep it in view with a margin. Landing on
    /// the last line resumes following new output.
    pub fn set_cursor(&mut self, line: usize) {
        let (height, total) = (self.height(), self.total());
        if total == 0 || height == 0 {
            return;
        }
        let line = line.min(total - 1);
        self.cursor = line;
        let max_offset = self.layout.offset(Scroll::Follow);
        let mut offset = self.offset();
        let margin = SCROLLOFF.min(height.saturating_sub(1) / 2);
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

    /// Bring the cursor to where the reader is looking, as the chat takes the keys: the
    /// last line while following, else the nearest line in view.
    pub fn bring_cursor_into_view(&mut self) {
        if self.scroll == Scroll::Follow {
            self.cursor = self.total().saturating_sub(1);
        } else {
            let offset = self.offset();
            self.cursor = self.cursor.clamp(
                offset,
                (offset + self.height()).saturating_sub(1).max(offset),
            );
        }
    }

    /// `j`: down a line, or as many as the count.
    pub fn down(&mut self) {
        let n = self.take_count();
        self.set_cursor(self.cursor + n);
    }

    /// `k`: up a line, or as many as the count.
    pub fn up(&mut self) {
        let n = self.take_count();
        self.set_cursor(self.cursor.saturating_sub(n));
    }

    /// `h`: back a character.
    pub fn left(&mut self) {
        let n = self.take_count();
        self.column = self.spot().1.saturating_sub(n);
    }

    /// `l`: on a character, no further than the line goes.
    pub fn right(&mut self) {
        let n = self.take_count();
        let last = self.layout.len(self.cursor).saturating_sub(1);
        self.column = (self.spot().1 + n).min(last);
    }

    /// `0`: the start of the line.
    pub fn line_start(&mut self) {
        self.column = 0;
    }

    /// `$`: the end of the line, held past it so the cursor stays there down a ragged
    /// block.
    pub fn line_end(&mut self) {
        self.column = usize::MAX;
    }

    /// `w` and `b` over the characters of the chat, carrying on into the line above or
    /// below when the one under the cursor runs out.
    pub fn word(&mut self, forward: bool) {
        let n = self.take_count();
        for _ in 0..n {
            let (mut line, mut column) = self.spot();
            let total = self.total();
            let row = |line: usize| -> Vec<char> { self.layout.row(line).chars().collect() };
            let mut chars = row(line);
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
                    chars = row(line);
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
                    chars = row(line);
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
            self.set_cursor(line);
            self.column = column.min(chars.len().saturating_sub(1));
        }
    }

    /// `gg`: the first line, scrolled to the top.
    pub fn top(&mut self) {
        self.count = None;
        self.set_cursor(0);
        self.scroll = Scroll::Offset(0);
    }

    /// `G`: the last line, following the end.
    pub fn bottom(&mut self) {
        self.count = None;
        self.cursor = self.total().saturating_sub(1);
        self.scroll = Scroll::Follow;
    }

    /// `{`: back to the start of a message, or as many as the count.
    pub fn previous_message(&mut self) {
        let n = self.take_count();
        let mut target = self.cursor;
        for _ in 0..n {
            match self
                .layout
                .message_starts()
                .iter()
                .rev()
                .find(|&&s| s < target)
            {
                Some(&s) => target = s,
                None => break,
            }
        }
        self.set_cursor(target);
    }

    /// `}`: on to the start of the next message, or the last line after the last one.
    pub fn next_message(&mut self) {
        let n = self.take_count();
        let mut target = self.cursor;
        for _ in 0..n {
            match self.layout.message_starts().iter().find(|&&s| s > target) {
                Some(&s) => target = s,
                None => {
                    target = self.total().saturating_sub(1);
                    break;
                }
            }
        }
        self.set_cursor(target);
    }

    /// Scroll the view, leaving the cursor where it is. Scrolled to the bottom is following
    /// again. Says whether it went up as far as the chat goes, where older turns may be.
    pub fn scroll_by(&mut self, delta: isize) -> bool {
        let max_offset = self.layout.offset(Scroll::Follow);
        let current = self.offset();
        let next = (current as isize + delta).clamp(0, max_offset as isize) as usize;
        self.scroll = if next >= max_offset {
            Scroll::Follow
        } else {
            Scroll::Offset(next)
        };
        next == 0 && delta < 0
    }

    /// Scroll the view and carry the cursor along, like Vim's Ctrl-d and Ctrl-u. Says
    /// whether it went up as far as the chat goes.
    pub fn page(&mut self, delta: isize) -> bool {
        let (height, total) = (self.height(), self.total());
        let before = self.offset();
        let top = self.scroll_by(delta);
        let after = self.offset();
        let moved = after as isize - before as isize;
        let target = (self.cursor as isize + if moved == 0 { delta } else { moved })
            .clamp(0, total.saturating_sub(1) as isize) as usize;
        self.cursor = target.clamp(after, (after + height).saturating_sub(1).max(after));
        if self.cursor + 1 >= total {
            self.scroll = Scroll::Follow;
        }
        top
    }

    /// The content line drawn on a row of the view.
    pub fn line_at(&self, row: usize) -> usize {
        self.offset() + row
    }

    /// A click on a line, at a column of the chat: the cursor goes to the character there.
    pub fn click(&mut self, line: usize, column: u16) {
        self.visual = None;
        self.set_cursor(line);
        self.column = self.layout.index(line, column);
    }

    /// What a drag over the view covers, from one row and column of it to another, both
    /// ends taken in. The marks and indents it was drawn with are left out.
    pub fn text_between(&self, from: (usize, u16), to: (usize, u16)) -> Option<String> {
        let spot = |(row, column): (usize, u16), past: usize| {
            let line = self.line_at(row);
            (line, self.layout.index(line, column) + past)
        };
        self.layout.span(spot(from, 0), spot(to, 1))
    }

    // ── Selecting ──────────────────────────────────────────────────────

    /// `v` takes the text a character at a time, `V` whole lines; either one pressed again
    /// lets the selection go, and each takes over from the other.
    pub fn toggle_visual(&mut self, whole_lines: bool) {
        self.visual = match self.visual {
            Some(anchor) if anchor.whole_lines == whole_lines => None,
            Some(anchor) => Some(Anchor {
                whole_lines,
                ..anchor
            }),
            None => Some(Anchor {
                line: self.cursor,
                column: self.spot().1,
                whole_lines,
            }),
        };
    }

    pub fn clear_visual(&mut self) {
        self.visual = None;
    }

    /// `y`: what is selected, letting the selection go; with nothing selected the line, or
    /// as many as the count. `None` when that is nothing but blank.
    pub fn yank(&mut self) -> Option<Yank> {
        let spot = self.spot();
        let cursor = self.cursor;
        let (start, end) = match self.visual.take() {
            // A character-wise selection reaches through the character the cursor is on,
            // which is where the block cursor is drawn.
            Some(anchor) if !anchor.whole_lines => {
                let head = (anchor.line, anchor.column.min(self.layout.len(anchor.line)));
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
                let n = self.take_count();
                let last = self.total().saturating_sub(1);
                ((cursor, 0), ((cursor + n - 1).min(last), usize::MAX))
            }
        };
        let text = self
            .layout
            .span(start, end)
            .filter(|text| !text.trim().is_empty())?;
        Some(Yank {
            text,
            piece: start.0 == end.0 && end.1 != usize::MAX,
        })
    }

    // ── Searching ──────────────────────────────────────────────────────

    /// While a search is typed, the cursor previews the first match from where the search
    /// started, and goes back there while there is nothing to look for.
    pub fn preview(&mut self, search: &Search, origin: usize) {
        if search.query.is_empty() {
            self.set_cursor(origin);
            return;
        }
        if let Some((line, _)) = self.layout.find(search, origin) {
            self.set_cursor(line);
        }
    }

    /// Search from a line, and keep the search for `n` and `N` to go on with.
    pub fn search_from(&mut self, search: Search, from: usize) -> Hit {
        let hit = self.go_to_match(&search, from);
        self.search = Some(search);
        hit
    }

    /// `n` and `N`: the last search again from the cursor, `N` against its direction.
    /// `None` when there has been no search.
    pub fn search_next(&mut self, reverse: bool) -> Option<Hit> {
        self.count = None;
        let mut search = self.search.clone()?;
        if reverse {
            search.backward = !search.backward;
        }
        Some(self.go_to_match(&search, self.cursor))
    }

    fn go_to_match(&mut self, search: &Search, from: usize) -> Hit {
        match self.layout.find(search, from) {
            Some((line, wrapped)) => {
                self.set_cursor(line);
                if wrapped {
                    Hit::Wrapped {
                        backward: search.backward,
                    }
                } else {
                    Hit::Found
                }
            }
            None => Hit::Missing,
        }
    }

    /// Go to what a search found in the block exported under `key`: the line in it that
    /// matches, or failing that its first, with the search kept for `n`. Says whether the
    /// block is laid out to go to.
    pub fn open_match(&mut self, key: &str, search: Search) -> bool {
        let Some((start, end)) = self.layout.block_lines(key) else {
            return false;
        };
        let line = self
            .layout
            .find(&search, start.saturating_sub(1))
            .filter(|(line, _)| (start..end).contains(line))
            .map_or(start, |(line, _)| line);
        self.search = Some(search);
        self.set_cursor(line);
        true
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
        // Lowercasing changed byte lengths, and can change how many characters there are
        // too (`İ` lowers to two), so match over the lowered characters, each kept with
        // the bytes of the character of `line` it came from.
        let mut hay: Vec<char> = Vec::new();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for (byte, ch) in line.char_indices() {
            for lower in ch.to_lowercase() {
                hay.push(lower);
                spans.push((byte, byte + ch.len_utf8()));
            }
        }
        let needle: Vec<char> = query.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i + needle.len() <= hay.len() {
            if hay[i..i + needle.len()] == needle[..] {
                let start = spans[i].0;
                let end = spans[i + needle.len() - 1].1;
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

#[cfg(test)]
mod tests {
    use ratatui::text::{Line, Text};

    use super::*;
    use crate::timeline::BlockKey;

    fn key(revision: u64) -> Key {
        Key {
            subject: layout::Subject::Thread("t1".into()),
            revision,
            expanded: 0,
            open_levels: 0,
            minute: None,
        }
    }

    /// A message of these lines, one row each at the width the tests lay out at.
    fn message(id: &str, lines: &[&str]) -> Block {
        Block {
            key: BlockKey::Message(id.into()),
            text: Text::from(
                lines
                    .iter()
                    .map(|line| Line::from(line.to_string()))
                    .collect::<Vec<_>>(),
            ),
            rows: Vec::new(),
            exports: vec![(format!("msg:{id}"), lines.join("\n"))],
            images: Vec::new(),
            pictures: Vec::new(),
        }
    }

    /// A chat of these messages, laid out forty columns wide and `height` tall.
    fn chat(messages: &[&[&str]], height: u16) -> ChatView {
        let mut view = ChatView::default();
        view.resize(40, height);
        view.lay_out(key(1), |_, _| {
            messages
                .iter()
                .enumerate()
                .map(|(at, lines)| message(&format!("m{at}"), lines))
                .collect()
        });
        view
    }

    /// Three messages of ten numbered lines each, ten lines of which are in view.
    fn numbered() -> ChatView {
        let lines: Vec<String> = (0..10).map(|n| format!("line {n}")).collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        chat(&[&lines, &lines, &lines], 10)
    }

    fn count(view: &mut ChatView, n: usize) {
        for digit in n.to_string().chars() {
            view.digit(digit.to_digit(10).unwrap() as usize);
        }
    }

    #[test]
    fn a_count_moves_as_many_lines_and_no_further_than_the_chat() {
        let mut view = numbered();
        view.top();
        count(&mut view, 3);
        view.down();
        assert_eq!(view.cursor(), 3);
        view.up();
        assert_eq!(view.cursor(), 2, "the count went with the motion");
        count(&mut view, 12);
        view.down();
        assert_eq!(view.cursor(), 14);
        count(&mut view, 99);
        view.down();
        assert_eq!(view.cursor(), 29);
        count(&mut view, 99);
        view.up();
        assert_eq!(view.cursor(), 0);
    }

    /// Following keeps the cursor on the last line. Moving up is reading something, and
    /// stops following; coming back down to the end, or `G`, follows again.
    #[test]
    fn moving_up_stops_following_and_the_end_follows_again() {
        let mut view = numbered();
        view.settle();
        assert_eq!(view.cursor(), 29);
        view.up();
        assert_eq!(view.cursor(), 28);
        assert_eq!(view.place().scroll, Scroll::Offset(20));
        view.down();
        assert_eq!(view.place().scroll, Scroll::Follow);
        view.top();
        assert_eq!((view.cursor(), view.offset()), (0, 0));
        view.bottom();
        assert_eq!(
            view.place(),
            Place {
                scroll: Scroll::Follow,
                cursor: 29
            }
        );
    }

    /// A cursor further down than the chat now goes, since what it showed was folded or
    /// put away, is kept on its last line.
    #[test]
    fn the_cursor_stays_on_the_chat_when_it_shrinks() {
        let mut view = numbered();
        view.set_cursor(25);
        view.lay_out(key(2), |_, _| vec![message("m0", &["one", "two", "three"])]);
        view.settle();
        assert_eq!(view.cursor(), 2);
        assert_eq!(view.offset(), 0);
    }

    #[test]
    fn braces_go_from_message_to_message() {
        let mut view = numbered();
        view.top();
        view.next_message();
        assert_eq!(view.cursor(), 10);
        count(&mut view, 2);
        view.next_message();
        assert_eq!(view.cursor(), 29, "past the last message is the last line");
        view.previous_message();
        assert_eq!(view.cursor(), 20);
        count(&mut view, 2);
        view.previous_message();
        assert_eq!(view.cursor(), 0);
    }

    /// `$` holds the end of whatever line the cursor is on, so it stays there down a
    /// ragged block; any other column is only kept to the line's length.
    #[test]
    fn a_column_is_kept_down_a_ragged_block() {
        let mut view = chat(
            &[&["a longer line here", "short", "a longer line here"]],
            10,
        );
        view.set_cursor(0);
        view.line_end();
        assert_eq!(view.spot(), (0, 17));
        view.down();
        assert_eq!(view.spot(), (1, 4));
        view.down();
        assert_eq!(view.spot(), (2, 17));
        view.line_start();
        count(&mut view, 3);
        view.right();
        assert_eq!(view.spot(), (2, 3));
        view.left();
        assert_eq!(view.spot(), (2, 2));
        count(&mut view, 50);
        view.right();
        assert_eq!(view.spot(), (2, 17), "no further than the line goes");
    }

    /// `w` and `b` stop where Vim's do: at each word and each run of punctuation, and at
    /// a blank line on the way past it.
    #[test]
    fn words_carry_on_over_the_lines() {
        let mut view = chat(&[&["one two", "", "three-four"]], 10);
        view.top();
        view.word(true);
        assert_eq!(view.spot(), (0, 4));
        view.word(true);
        assert_eq!(view.spot(), (1, 0), "a blank line is a stop");
        view.word(true);
        assert_eq!(view.spot(), (2, 0));
        view.word(true);
        assert_eq!(view.spot(), (2, 5));
        view.word(false);
        assert_eq!(view.spot(), (2, 0));
        count(&mut view, 2);
        view.word(false);
        assert_eq!(view.spot(), (0, 4));
    }

    /// Ctrl-d and Ctrl-u move the view and carry the cursor with it, and say when they
    /// have gone up as far as the chat goes, where there may be older turns.
    #[test]
    fn paging_carries_the_cursor_and_says_when_it_is_at_the_top() {
        let mut view = numbered();
        view.top();
        assert!(!view.page(5));
        assert_eq!((view.offset(), view.cursor()), (5, 5));
        assert!(view.page(-5));
        assert_eq!((view.offset(), view.cursor()), (0, 0));
        view.page(100);
        assert_eq!(view.place().scroll, Scroll::Follow);
        assert_eq!(view.offset(), 20);
        // Scrolling on its own leaves the cursor where it was.
        view.set_cursor(22);
        assert!(!view.scroll_by(-3));
        assert_eq!((view.offset(), view.cursor()), (16, 22));
        assert!(view.scroll_by(-30));
    }

    #[test]
    fn n_goes_on_to_the_next_match_and_round_the_end() {
        let mut view = chat(&[&["a needle", "hay"], &["hay", "another needle"]], 10);
        assert_eq!(view.search_next(false), None, "there has been no search");
        view.top();
        let search = |query: &str| Search {
            query: query.into(),
            backward: false,
        };
        assert_eq!(view.search_from(search("needle"), 0), Hit::Found);
        assert_eq!(view.cursor(), 3);
        assert_eq!(
            view.search_next(false),
            Some(Hit::Wrapped { backward: false })
        );
        assert_eq!(view.cursor(), 0);
        assert_eq!(
            view.search_next(true),
            Some(Hit::Wrapped { backward: true })
        );
        assert_eq!(view.cursor(), 3);
        assert_eq!(view.search_next(true), Some(Hit::Found));
        assert_eq!(view.cursor(), 0);
        assert_eq!(view.search_from(search("thread"), 0), Hit::Missing);
        assert_eq!(view.cursor(), 0);
        assert_eq!(view.search().map(|s| s.query.as_str()), Some("thread"));
    }

    /// While a search is typed the cursor shows where it would go, and goes back to
    /// where it started while there is nothing to look for.
    #[test]
    fn a_search_being_typed_previews_its_match() {
        let mut view = numbered();
        view.set_cursor(12);
        let typed = |query: &str| Search {
            query: query.into(),
            backward: false,
        };
        view.preview(&typed("line 7"), 12);
        assert_eq!(view.cursor(), 17);
        view.preview(&typed(""), 12);
        assert_eq!(view.cursor(), 12);
        assert!(
            view.search().is_none(),
            "nothing is kept until it is entered"
        );
    }

    #[test]
    fn a_match_found_elsewhere_opens_on_its_line() {
        let mut view = chat(&[&["needle"], &["you", "hay", "the needle"]], 10);
        let search = || Search {
            query: "needle".into(),
            backward: false,
        };
        assert!(!view.open_match("msg:nowhere", search()));
        assert!(view.open_match("msg:m1", search()));
        assert_eq!(view.cursor(), 3);
        assert!(view.search().is_some());
    }

    /// What `y` takes is what is selected, then the line or as many as the count, and a
    /// piece of a line is told from whole lines.
    #[test]
    fn a_yank_takes_the_selection_or_the_lines() {
        let mut view = chat(&[&["one two three", "four", ""]], 10);
        view.set_cursor(0);
        count(&mut view, 4);
        view.right();
        view.toggle_visual(false);
        view.right();
        view.right();
        assert_eq!(
            view.yank(),
            Some(Yank {
                text: "two".into(),
                piece: true
            })
        );
        assert_eq!(view.visual(), None, "the selection is let go");

        view.toggle_visual(true);
        view.down();
        assert_eq!(view.yank().unwrap().text, "one two three\nfour");

        view.set_cursor(0);
        count(&mut view, 2);
        let yank = view.yank().unwrap();
        assert_eq!(
            (yank.text.as_str(), yank.piece),
            ("one two three\nfour", false)
        );
        view.set_cursor(2);
        assert_eq!(view.yank(), None, "a blank line is nothing to yank");
    }

    /// `v` and `V` take over from each other, and either one again lets go.
    #[test]
    fn visual_modes_take_over_from_each_other() {
        let mut view = numbered();
        view.set_cursor(4);
        view.toggle_visual(false);
        view.toggle_visual(true);
        assert!(view.visual().is_some_and(|anchor| anchor.whole_lines));
        view.toggle_visual(true);
        assert_eq!(view.visual(), None);
    }

    /// The cursor and the selection are painted on the rows of the view they are on,
    /// and nothing is painted for what is scrolled out of it.
    #[test]
    fn the_cursor_and_the_selection_are_drawn_where_they_are_in_view() {
        let mut view = chat(&[&["one", "two", "three", "four", "five", "six"]], 3);
        view.set_cursor(1);
        assert_eq!(view.offset(), 0);
        view.toggle_visual(true);
        view.down();
        view.down();
        view.down();
        // Scrolled to keep a line under the cursor: "four" and "five" are at the top.
        assert_eq!(view.offset(), 3);
        assert_eq!(view.cursor_cell(), Some((1, 0)));
        assert_eq!(view.selection(40), [(0, 0, 40), (1, 0, 40)]);
    }

    #[test]
    fn a_count_is_typed_before_its_motion_and_esc_lets_it_go() {
        let mut view = numbered();
        assert!(!view.digit(0), "a 0 on its own is the start of the line");
        assert!(view.digit(2));
        assert!(view.digit(0));
        assert!(!view.is_idle());
        assert!(view.cancel());
        assert!(view.is_idle());
        assert!(!view.cancel(), "and then there is nothing to let go");
    }

    /// What was read over the conversation opens at its top, or following its end, and
    /// with nothing of the conversation's selected or searched; leaving puts the reader
    /// back where they were.
    #[test]
    fn something_read_opens_in_its_place_and_the_conversation_is_put_back() {
        let mut view = numbered();
        view.set_cursor(12);
        view.toggle_visual(false);
        let kept = view.place();
        view.search_from(
            Search {
                query: "line".into(),
                backward: false,
            },
            12,
        );
        view.open(reader::Place::Keep);
        assert!(view.search().is_some(), "kept where it was");
        view.open(reader::Place::Top);
        assert_eq!((view.cursor(), view.offset()), (0, 0));
        assert!(view.visual().is_none() && view.search().is_none());
        view.open(reader::Place::End);
        assert_eq!(view.place().scroll, Scroll::Follow);
        view.restore(kept);
        assert_eq!(view.place(), kept);
    }

    /// The chat taking the keys brings the cursor to what is on the screen.
    #[test]
    fn the_cursor_is_brought_into_view_as_the_chat_takes_the_keys() {
        let mut view = numbered();
        view.set_cursor(2);
        view.scroll_by(10);
        view.bring_cursor_into_view();
        assert_eq!(view.cursor(), view.offset());
        view.follow();
        view.bring_cursor_into_view();
        assert_eq!(view.cursor(), 29);
    }

    #[test]
    fn search_matching_is_smartcase() {
        assert!(line_matches("Nx cache", "nx"));
        assert!(!line_matches("nx cache", "Nx"));
        assert_eq!(match_ranges("a nx b NX", "nx"), vec![(2, 4), (7, 9)]);
        assert_eq!(match_ranges("ÄÖ nx", "nx"), vec![(5, 7)]);
        // `İ` lowers to two characters, so the lowered line is longer than the line.
        assert_eq!(match_ranges("İa", "a"), vec![(2, 3)]);
        assert_eq!(match_ranges("İa", "i"), vec![(0, 2)]);
        assert!(match_ranges("abc", "").is_empty());
    }
}

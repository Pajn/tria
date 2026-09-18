//! A small multi-line text editor with prompt history, behind the message composer and
//! the one-line field a custom answer is typed into.

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
};

const MAX_HISTORY: usize = 200;

#[derive(Debug, Default)]
pub struct Composer {
    pub(crate) lines: Vec<String>,
    pub(crate) row: usize,
    /// Cursor column in characters.
    pub(crate) col: usize,
    pub(crate) vim: crate::vim::VimState,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: String,
}

impl Composer {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            ..Default::default()
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.trim().is_empty())
    }

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
        self.history_index = None;
        self.draft.clear();
    }

    pub fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(str::to_string).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.col = char_len(&self.lines[self.row]);
    }

    pub fn push_history(&mut self, text: String) {
        if self.history.last() != Some(&text) {
            self.history.push(text);
            if self.history.len() > MAX_HISTORY {
                self.history.remove(0);
            }
        }
        self.history_index = None;
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_index {
            None => {
                self.draft = self.text();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.history_index = Some(next);
        let text = self.history[next].clone();
        self.set_text(&text);
    }

    pub fn history_next(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.history.len() {
            self.history_index = Some(index + 1);
            let text = self.history[index + 1].clone();
            self.set_text(&text);
        } else {
            self.history_index = None;
            let draft = std::mem::take(&mut self.draft);
            self.set_text(&draft);
        }
    }

    fn line(&self) -> &String {
        &self.lines[self.row]
    }

    fn byte_index(line: &str, col: usize) -> usize {
        line.char_indices()
            .nth(col)
            .map(|(i, _)| i)
            .unwrap_or(line.len())
    }

    pub fn insert_char(&mut self, ch: char) {
        let idx = Self::byte_index(self.line(), self.col);
        self.lines[self.row].insert(idx, ch);
        self.col += 1;
    }

    pub fn insert_str(&mut self, text: &str) {
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                self.newline();
            }
            for ch in part.chars() {
                if ch != '\r' {
                    self.insert_char(ch);
                }
            }
        }
    }

    pub fn newline(&mut self) {
        let idx = Self::byte_index(self.line(), self.col);
        let rest = self.lines[self.row].split_off(idx);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            let idx = Self::byte_index(self.line(), self.col - 1);
            self.lines[self.row].remove(idx);
            self.col -= 1;
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = char_len(&self.lines[self.row]);
            self.lines[self.row].push_str(&line);
        }
    }

    pub fn delete(&mut self) {
        if self.col < char_len(self.line()) {
            let idx = Self::byte_index(self.line(), self.col);
            self.lines[self.row].remove(idx);
        } else if self.row + 1 < self.lines.len() {
            let line = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&line);
        }
    }

    pub fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = char_len(self.line());
        }
    }

    pub fn right(&mut self) {
        if self.col < char_len(self.line()) {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    /// Returns false when already on the first row so callers can reuse the key.
    pub fn up(&mut self) -> bool {
        if self.row == 0 {
            return false;
        }
        self.row -= 1;
        self.col = self.col.min(char_len(self.line()));
        true
    }

    pub fn down(&mut self) -> bool {
        if self.row + 1 >= self.lines.len() {
            return false;
        }
        self.row += 1;
        self.col = self.col.min(char_len(self.line()));
        true
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = char_len(self.line());
    }

    pub fn word_left(&mut self) {
        let chars: Vec<char> = self.line().chars().collect();
        let mut col = self.col;
        while col > 0 && chars[col - 1].is_whitespace() {
            col -= 1;
        }
        while col > 0 && !chars[col - 1].is_whitespace() {
            col -= 1;
        }
        if col == self.col && self.col == 0 {
            self.left();
        } else {
            self.col = col;
        }
    }

    pub fn word_right(&mut self) {
        let chars: Vec<char> = self.line().chars().collect();
        let mut col = self.col;
        while col < chars.len() && !chars[col].is_whitespace() {
            col += 1;
        }
        while col < chars.len() && chars[col].is_whitespace() {
            col += 1;
        }
        if col == self.col {
            self.right();
        } else {
            self.col = col;
        }
    }

    pub fn kill_word_back(&mut self) {
        let start = self.col;
        self.word_left();
        if self.row < self.lines.len() && start > self.col {
            let line = &self.lines[self.row];
            let a = Self::byte_index(line, self.col);
            let b = Self::byte_index(line, start);
            self.lines[self.row].replace_range(a..b, "");
        }
    }

    pub fn kill_to_end(&mut self) {
        let idx = Self::byte_index(self.line(), self.col);
        if idx < self.lines[self.row].len() {
            self.lines[self.row].truncate(idx);
        } else {
            self.delete();
        }
    }

    pub fn kill_to_start(&mut self) {
        let idx = Self::byte_index(self.line(), self.col);
        self.lines[self.row].replace_range(..idx, "");
        self.col = 0;
    }

    /// What to draw in a one-line field `width` columns wide, and the column the cursor
    /// lands on in it. The window follows the cursor and keeps some of the line ahead of
    /// it in view where there is any, so typing at the end keeps the end in sight and
    /// moving back through a long answer scrolls it along.
    pub fn line_window(&self, width: usize) -> (String, usize) {
        if width == 0 {
            return (String::new(), 0);
        }
        let chars: Vec<char> = self.line().chars().collect();
        let col = self.col.min(chars.len());
        // The cursor sits one past the last character, so the window leaves it a column.
        let max_offset = (chars.len() + 1).saturating_sub(width);
        let offset = col.saturating_sub(width * 2 / 3).min(max_offset);
        let end = (offset + width).min(chars.len());
        (chars[offset..end].iter().collect(), col - offset)
    }

    /// Number of screen rows needed at `width`, between 1 and `max`.
    pub fn height(&self, width: u16, max: u16) -> u16 {
        let width = width.max(1) as usize;
        let rows: usize = self
            .lines
            .iter()
            .map(|l| char_len(l).max(1).div_ceil(width).max(1))
            .sum();
        (rows as u16).clamp(1, max)
    }

    /// The lines as they are drawn: each one cut into rows of `width` characters, with
    /// the line it came from and the part of it shown. Wrapping is by width alone, so a
    /// row is a slice and a screen position maps straight back to a character.
    fn wrapped(&self, width: usize) -> Vec<Row> {
        let mut rows = Vec::new();
        for (line, text) in self.lines.iter().enumerate() {
            let chars = char_len(text);
            for chunk in 0..chars.div_ceil(width).max(1) {
                rows.push(Row {
                    line,
                    start: chunk * width,
                    end: ((chunk + 1) * width).min(chars),
                    last: (chunk + 1) * width >= chars,
                });
            }
        }
        rows
    }

    /// Which row the cursor is on, and how far along it.
    fn cursor_row(&self, rows: &[Row]) -> (u16, u16) {
        for (index, row) in rows.iter().enumerate() {
            if row.line == self.row
                && self.col >= row.start
                && (self.col < row.end || (self.col == row.end && row.last))
            {
                return ((self.col - row.start) as u16, index as u16);
            }
        }
        (0, 0)
    }

    /// How far the rows are scrolled to keep the cursor on screen.
    fn scroll(&self, rows: &[Row], height: u16) -> u16 {
        self.cursor_row(rows).1.saturating_sub(height.max(1) - 1)
    }

    /// Put the cursor where a click landed, given the area the composer was drawn in.
    /// Past the end of a line is the end of it, and past the last line is the last one:
    /// clicking into the empty part of the box means the nearest place to type.
    pub fn click(&mut self, area: Rect, column: u16, row: u16) {
        let width = area.width.max(1) as usize;
        let rows = self.wrapped(width);
        let scroll = self.scroll(&rows, area.height) as usize;
        let wanted = scroll + row.saturating_sub(area.y) as usize;
        let Some(row) = rows.get(wanted).or_else(|| rows.last()) else {
            return;
        };
        self.row = row.line;
        let along = column.saturating_sub(area.x) as usize;
        self.col = (row.start + along).min(row.end);
        // A half-typed operator has nothing to do with where the mouse went.
        self.vim_cancel();
    }

    /// Wrapped lines plus the cursor position relative to `area`, scrolled so the cursor is visible.
    pub fn render(&self, area: Rect, placeholder: &str) -> (Vec<Line<'static>>, (u16, u16)) {
        let width = area.width.max(1) as usize;
        let cursor = self.cursor_row(&self.wrapped(width));
        let mut rows: Vec<Line<'static>> = Vec::new();
        for line in &self.lines {
            let chars: Vec<char> = line.chars().collect();
            let chunks = chars.len().div_ceil(width).max(1);
            for chunk in 0..chunks {
                let start = chunk * width;
                let end = ((chunk + 1) * width).min(chars.len());
                rows.push(Line::from(chars[start..end].iter().collect::<String>()));
            }
        }
        if self.is_empty() && self.lines.len() == 1 {
            rows[0] = Line::from(Span::styled(
                placeholder.to_string(),
                Style::default().fg(Color::DarkGray),
            ));
        }
        let height = area.height.max(1);
        let scroll = cursor.1.saturating_sub(height - 1);
        let visible: Vec<Line<'static>> = rows
            .into_iter()
            .skip(scroll as usize)
            .take(height as usize)
            .collect();
        (
            visible,
            (
                area.x + cursor.0.min(area.width.saturating_sub(1)),
                area.y + cursor.1 - scroll,
            ),
        )
    }
}

/// One line as one row of the screen: which line, and the slice of it shown.
struct Row {
    line: usize,
    start: usize,
    end: usize,
    /// The last row of its line, which is the one that can hold the cursor past the end.
    last: bool,
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A click is a character, and the two have to agree about wrapping and scrolling
    /// or the cursor lands somewhere the text is not.
    #[test]
    fn a_click_lands_on_the_character_under_it() {
        let area = Rect::new(4, 2, 10, 3);
        let mut c = Composer::new();
        c.set_text("0123456789abcde\nsecond");

        // Second row of the first line: ten characters in, plus three along.
        c.click(area, 4 + 3, 2 + 1);
        assert_eq!((c.row, c.col), (0, 13));

        // Past the end of a row that is the end of its line: the end of it.
        c.click(area, 4 + 9, 2 + 2);
        assert_eq!((c.row, c.col), (1, 6));

        // Below everything: the last line, which is the nearest place to type.
        c.click(area, 4, 2 + 2);
        assert_eq!((c.row, c.col), (1, 0));

        // The first character of all.
        c.click(area, 4, 2);
        assert_eq!((c.row, c.col), (0, 0));
    }

    /// A composer taller than its box is scrolled to keep the cursor in view, and a
    /// click has to be read through the same scroll.
    #[test]
    fn a_click_reads_through_the_scroll() {
        let area = Rect::new(0, 0, 10, 2);
        let mut c = Composer::new();
        c.set_text("one\ntwo\nthree\nfour");
        // The cursor is at the end, so the last two lines are the ones drawn.
        assert_eq!((c.row, c.col), (3, 4));
        c.click(area, 1, 0);
        assert_eq!(
            (c.row, c.col),
            (2, 1),
            "the top row shown is the third line"
        );
    }

    /// The mouse is not part of a half-typed operator.
    #[test]
    fn a_click_cancels_a_pending_operator() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let mut c = Composer::new();
        c.set_text("hello world");
        c.vim_key(KeyEvent::from(KeyCode::Char('d')));
        assert!(c.vim_pending());
        c.click(Rect::new(0, 0, 20, 1), 2, 0);
        assert!(!c.vim_pending());
        assert_eq!(c.text(), "hello world");
    }

    #[test]
    fn editing_round_trips() {
        let mut c = Composer::new();
        c.insert_str("hello wörld");
        c.word_left();
        c.insert_char('X');
        assert_eq!(c.text(), "hello Xwörld");
        c.newline();
        c.insert_str("two");
        assert_eq!(
            c.text(),
            "hello X\nwörldtwo"
                .replace("wörldtwo", "wörld")
                .replace("X\n", "X\ntwo")
        );
        c.backspace();
        c.backspace();
        c.backspace();
        c.backspace();
        assert_eq!(c.text(), "hello Xwörld");
    }

    #[test]
    fn a_short_line_fills_the_window_from_the_start() {
        let mut c = Composer::new();
        c.insert_str("hej hopp");
        assert_eq!(c.line_window(20), ("hej hopp".to_string(), 8));
        c.home();
        assert_eq!(c.line_window(20), ("hej hopp".to_string(), 0));
        assert_eq!(c.line_window(0), (String::new(), 0));
    }

    #[test]
    fn a_long_line_scrolls_to_keep_the_cursor_in_view() {
        let mut c = Composer::new();
        c.insert_str(&"abcdefghij".repeat(10));
        // At the end: the last column is the cursor's, so the tail is what shows.
        let (text, cursor) = c.line_window(30);
        assert_eq!(cursor, 29);
        assert_eq!(text.chars().count(), 29);
        assert!("abcdefghij".repeat(10).ends_with(&text));
        // Back in the middle: the window keeps a third of itself ahead of the cursor.
        c.home();
        for _ in 0..50 {
            c.right();
        }
        let (text, cursor) = c.line_window(30);
        assert_eq!(cursor, 20);
        assert_eq!(text.chars().count(), 30);
        // Near the start there is nothing to scroll to.
        c.home();
        c.right();
        assert_eq!(c.line_window(30).1, 1);
    }

    #[test]
    fn the_window_counts_characters_not_bytes() {
        let mut c = Composer::new();
        c.insert_str("åäöåäöåäö");
        assert_eq!(c.line_window(5), ("öåäö".to_string(), 4));
    }

    #[test]
    fn history_navigation() {
        let mut c = Composer::new();
        c.push_history("first".into());
        c.push_history("second".into());
        c.insert_str("draft");
        c.history_prev();
        assert_eq!(c.text(), "second");
        c.history_prev();
        assert_eq!(c.text(), "first");
        c.history_next();
        c.history_next();
        assert_eq!(c.text(), "draft");
    }
}

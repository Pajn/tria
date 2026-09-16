//! A small multi-line text editor for the message composer, with prompt history.

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

    /// Wrapped lines plus the cursor position relative to `area`, scrolled so the cursor is visible.
    pub fn render(&self, area: Rect, placeholder: &str) -> (Vec<Line<'static>>, (u16, u16)) {
        let width = area.width.max(1) as usize;
        let mut rows: Vec<Line<'static>> = Vec::new();
        let mut cursor = (0u16, 0u16);
        for (row_index, line) in self.lines.iter().enumerate() {
            let chars: Vec<char> = line.chars().collect();
            let chunks = chars.len().div_ceil(width).max(1);
            for chunk in 0..chunks {
                let start = chunk * width;
                let end = ((chunk + 1) * width).min(chars.len());
                let text: String = chars[start..end].iter().collect();
                if row_index == self.row
                    && self.col >= start
                    && (self.col < end || (self.col == end && chunk == chunks - 1))
                {
                    cursor = ((self.col - start) as u16, rows.len() as u16);
                }
                rows.push(Line::from(text));
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

fn char_len(s: &str) -> usize {
    s.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

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

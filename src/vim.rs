//! Vim normal-mode editing for the composer.
//!
//! The engine covers motions (`h j k l w b e W B E 0 ^ $ G f F t T ; ,`), operators
//! (`d c y`) with motions, text objects (`iw aw iW aW` and quote or bracket pairs), the
//! line and shortcut forms (`dd cc yy D C Y x X`), `p P`, `r`, `~`, insert entry
//! (`i a I A o O`), counts, and a single unnamed register with `u` and `Ctrl-r` history.
//! `gg` is handled by the caller because `g` is a shared prefix.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::composer::Composer;

type Pos = (usize, usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Delete,
    Change,
    Yank,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindKind {
    Forward,
    Backward,
    TillForward,
    TillBackward,
}

impl FindKind {
    fn reversed(self) -> Self {
        match self {
            Self::Forward => Self::Backward,
            Self::Backward => Self::Forward,
            Self::TillForward => Self::TillBackward,
            Self::TillBackward => Self::TillForward,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordForward { big: bool },
    WordEnd { big: bool },
    WordBack { big: bool },
    LineStart,
    FirstNonBlank,
    LineEnd,
    Top,
    Bottom,
    Find(FindKind, char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Exclusive,
    Inclusive,
    Linewise,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Pending {
    #[default]
    None,
    Op(Op),
    Find {
        op: Option<Op>,
        kind: FindKind,
    },
    Object {
        op: Op,
        inner: bool,
    },
    Replace,
}

#[derive(Debug, Clone)]
struct Register {
    text: String,
    linewise: bool,
}

#[derive(Debug, Clone)]
struct Snapshot {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

#[derive(Debug, Default)]
pub struct VimState {
    count: Option<usize>,
    /// Count typed before the operator, multiplied with the motion count.
    op_count: Option<usize>,
    pending: Pending,
    last_find: Option<(FindKind, char)>,
    /// Set while `;` or `,` runs so `t`/`T` skip a target they are already next to.
    find_repeat: bool,
    register: Option<Register>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
}

/// What the caller has to do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    /// Switch to insert mode; the cursor is already placed.
    EnterInsert,
    /// Text landed in the register; the caller may mirror it to the clipboard.
    Yanked(String),
}

const MAX_UNDO: usize = 200;

fn class(ch: char, big: bool) -> u8 {
    if ch.is_whitespace() {
        0
    } else if big || ch.is_alphanumeric() || ch == '_' {
        1
    } else {
        2
    }
}

impl Composer {
    /// Whether a multi-key command is waiting for more input.
    pub fn vim_pending(&self) -> bool {
        self.vim.pending != Pending::None || self.vim.count.is_some()
    }

    /// Text shown in the status line for a partially typed command.
    pub fn vim_pending_label(&self) -> String {
        let mut out = String::new();
        if let Some(n) = self.vim.op_count {
            out.push_str(&n.to_string());
        }
        match self.vim.pending {
            Pending::None => {}
            Pending::Op(op) | Pending::Object { op, .. } => out.push(op_char(op)),
            Pending::Find { op, kind } => {
                if let Some(op) = op {
                    out.push(op_char(op));
                }
                out.push(match kind {
                    FindKind::Forward => 'f',
                    FindKind::Backward => 'F',
                    FindKind::TillForward => 't',
                    FindKind::TillBackward => 'T',
                });
            }
            Pending::Replace => out.push('r'),
        }
        if let Pending::Object { inner, .. } = self.vim.pending {
            out.push(if inner { 'i' } else { 'a' });
        }
        if let Some(n) = self.vim.count {
            out.push_str(&n.to_string());
        }
        out
    }

    pub fn vim_cancel(&mut self) {
        self.vim.pending = Pending::None;
        self.vim.count = None;
        self.vim.op_count = None;
    }

    /// Leaving insert mode: Vim steps the cursor back onto the last typed character.
    pub fn leave_insert(&mut self) {
        self.col = self.col.saturating_sub(1);
        self.clamp_normal();
    }

    /// Keep the cursor on a character, as normal mode requires.
    pub fn clamp_normal(&mut self) {
        self.row = self.row.min(self.lines.len() - 1);
        let len = self.line_len(self.row);
        self.col = self.col.min(len.saturating_sub(1));
    }

    /// Record the current text so `u` can bring it back.
    pub fn checkpoint(&mut self) {
        self.vim.undo.push(Snapshot {
            lines: self.lines.clone(),
            row: self.row,
            col: self.col,
        });
        if self.vim.undo.len() > MAX_UNDO {
            self.vim.undo.remove(0);
        }
        self.vim.redo.clear();
    }

    /// `gg`: first line, first non-blank. Kept public because `g` is a caller prefix.
    pub fn vim_top(&mut self) {
        let count = self.vim.count.take();
        self.vim.op_count = None;
        self.move_to_target(Motion::Top, count.unwrap_or(1));
    }

    pub fn vim_key(&mut self, key: KeyEvent) -> Effect {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc {
            self.vim_cancel();
            return Effect::None;
        }
        if ctrl {
            if key.code == KeyCode::Char('r') {
                self.redo();
            }
            return Effect::None;
        }
        let pending = self.vim.pending;
        match pending {
            Pending::Find { op, kind } => {
                let KeyCode::Char(ch) = key.code else {
                    self.vim_cancel();
                    return Effect::None;
                };
                self.vim.pending = Pending::None;
                self.vim.last_find = Some((kind, ch));
                self.run_motion(op, Motion::Find(kind, ch))
            }
            Pending::Object { op, inner } => {
                self.vim.pending = Pending::None;
                let KeyCode::Char(ch) = key.code else {
                    self.vim_cancel();
                    return Effect::None;
                };
                let count = self.take_count();
                match self.text_object(ch, inner, count) {
                    Some((start, end)) => self.apply_op(op, start, end, Kind::Exclusive),
                    None => {
                        self.vim_cancel();
                        Effect::None
                    }
                }
            }
            Pending::Replace => {
                self.vim.pending = Pending::None;
                let KeyCode::Char(ch) = key.code else {
                    self.vim_cancel();
                    return Effect::None;
                };
                let count = self.take_count();
                let len = self.line_len(self.row);
                if self.col + count > len {
                    return Effect::None;
                }
                self.checkpoint();
                let row = self.row;
                let mut chars: Vec<char> = self.lines[row].chars().collect();
                for c in chars.iter_mut().skip(self.col).take(count) {
                    *c = ch;
                }
                self.lines[row] = chars.into_iter().collect();
                self.col += count - 1;
                Effect::None
            }
            Pending::Op(op) => self.op_key(op, key.code),
            Pending::None => self.plain_key(key.code),
        }
    }

    fn take_count(&mut self) -> usize {
        let count = self.vim.count.take().unwrap_or(1) * self.vim.op_count.take().unwrap_or(1);
        count.max(1)
    }

    fn push_digit(&mut self, digit: usize) {
        let current = self.vim.count.unwrap_or(0);
        self.vim.count = Some((current * 10 + digit).min(10_000));
    }

    fn plain_key(&mut self, code: KeyCode) -> Effect {
        match code {
            KeyCode::Char(c @ '1'..='9') => {
                self.push_digit(c as usize - '0' as usize);
                Effect::None
            }
            KeyCode::Char('0') if self.vim.count.is_some() => {
                self.push_digit(0);
                Effect::None
            }
            KeyCode::Char('d') => self.start_op(Op::Delete),
            KeyCode::Char('c') => self.start_op(Op::Change),
            KeyCode::Char('y') => self.start_op(Op::Yank),
            KeyCode::Char('x') | KeyCode::Delete => {
                let count = self.take_count();
                let len = self.line_len(self.row);
                if len == 0 {
                    return Effect::None;
                }
                let end = (self.row, (self.col + count).min(len));
                self.apply_op(Op::Delete, (self.row, self.col), end, Kind::Exclusive)
            }
            KeyCode::Char('X') => {
                let count = self.take_count();
                if self.col == 0 {
                    return Effect::None;
                }
                let start = (self.row, self.col.saturating_sub(count));
                self.apply_op(Op::Delete, start, (self.row, self.col), Kind::Exclusive)
            }
            KeyCode::Char('D') => {
                self.take_count();
                self.apply_to_line_end(Op::Delete)
            }
            KeyCode::Char('C') => {
                self.take_count();
                self.apply_to_line_end(Op::Change)
            }
            KeyCode::Char('Y') => {
                let count = self.take_count();
                self.apply_lines(Op::Yank, count)
            }
            KeyCode::Char('p') => self.paste(true),
            KeyCode::Char('P') => self.paste(false),
            KeyCode::Char('r') => {
                self.vim.pending = Pending::Replace;
                Effect::None
            }
            KeyCode::Char('~') => {
                let count = self.take_count();
                let len = self.line_len(self.row);
                if len == 0 {
                    return Effect::None;
                }
                self.checkpoint();
                let row = self.row;
                let mut chars: Vec<char> = self.lines[row].chars().collect();
                let end = (self.col + count).min(len);
                for c in chars.iter_mut().take(end).skip(self.col) {
                    *c = if c.is_uppercase() {
                        c.to_lowercase().next().unwrap_or(*c)
                    } else {
                        c.to_uppercase().next().unwrap_or(*c)
                    };
                }
                self.lines[row] = chars.into_iter().collect();
                self.col = end.min(len - 1);
                Effect::None
            }
            KeyCode::Char('u') => {
                self.vim_cancel();
                self.undo();
                Effect::None
            }
            KeyCode::Char('i') => {
                self.vim_cancel();
                self.checkpoint();
                Effect::EnterInsert
            }
            KeyCode::Char('a') => {
                self.vim_cancel();
                self.checkpoint();
                self.col = (self.col + 1).min(self.line_len(self.row));
                Effect::EnterInsert
            }
            KeyCode::Char('I') => {
                self.vim_cancel();
                self.checkpoint();
                self.col = self.first_non_blank(self.row);
                Effect::EnterInsert
            }
            KeyCode::Char('A') => {
                self.vim_cancel();
                self.checkpoint();
                self.col = self.line_len(self.row);
                Effect::EnterInsert
            }
            KeyCode::Char('o') => {
                self.vim_cancel();
                self.checkpoint();
                self.lines.insert(self.row + 1, String::new());
                self.row += 1;
                self.col = 0;
                Effect::EnterInsert
            }
            KeyCode::Char('O') => {
                self.vim_cancel();
                self.checkpoint();
                self.lines.insert(self.row, String::new());
                self.col = 0;
                Effect::EnterInsert
            }
            KeyCode::Char('f') => self.start_find(None, FindKind::Forward),
            KeyCode::Char('F') => self.start_find(None, FindKind::Backward),
            KeyCode::Char('t') => self.start_find(None, FindKind::TillForward),
            KeyCode::Char('T') => self.start_find(None, FindKind::TillBackward),
            KeyCode::Char(';') => self.repeat_find(None, false),
            KeyCode::Char(',') => self.repeat_find(None, true),
            _ => match motion_for(code) {
                Some(motion) => self.run_motion(None, motion),
                None => {
                    self.vim_cancel();
                    Effect::None
                }
            },
        }
    }

    fn start_op(&mut self, op: Op) -> Effect {
        self.vim.op_count = self.vim.count.take();
        self.vim.pending = Pending::Op(op);
        Effect::None
    }

    fn start_find(&mut self, op: Option<Op>, kind: FindKind) -> Effect {
        self.vim.pending = Pending::Find { op, kind };
        Effect::None
    }

    fn repeat_find(&mut self, op: Option<Op>, reverse: bool) -> Effect {
        let Some((kind, ch)) = self.vim.last_find else {
            self.vim_cancel();
            return Effect::None;
        };
        let kind = if reverse { kind.reversed() } else { kind };
        self.vim.find_repeat = true;
        let effect = self.run_motion(op, Motion::Find(kind, ch));
        self.vim.find_repeat = false;
        effect
    }

    fn op_key(&mut self, op: Op, code: KeyCode) -> Effect {
        match code {
            KeyCode::Char(c @ '0'..='9') if c != '0' || self.vim.count.is_some() => {
                self.push_digit(c as usize - '0' as usize);
                Effect::None
            }
            KeyCode::Char(c) if c == op_char(op) => {
                self.vim.pending = Pending::None;
                let count = self.take_count();
                self.apply_lines(op, count)
            }
            KeyCode::Char('i') => {
                self.vim.pending = Pending::Object { op, inner: true };
                Effect::None
            }
            KeyCode::Char('a') => {
                self.vim.pending = Pending::Object { op, inner: false };
                Effect::None
            }
            KeyCode::Char('f') => self.start_find(Some(op), FindKind::Forward),
            KeyCode::Char('F') => self.start_find(Some(op), FindKind::Backward),
            KeyCode::Char('t') => self.start_find(Some(op), FindKind::TillForward),
            KeyCode::Char('T') => self.start_find(Some(op), FindKind::TillBackward),
            KeyCode::Char(';') => {
                self.vim.pending = Pending::None;
                self.repeat_find(Some(op), false)
            }
            KeyCode::Char(',') => {
                self.vim.pending = Pending::None;
                self.repeat_find(Some(op), true)
            }
            _ => {
                self.vim.pending = Pending::None;
                match motion_for(code) {
                    Some(motion) => self.run_motion(Some(op), motion),
                    None => {
                        self.vim_cancel();
                        Effect::None
                    }
                }
            }
        }
    }

    /// Move, or apply the pending operator over the motion.
    fn run_motion(&mut self, op: Option<Op>, motion: Motion) -> Effect {
        let count = self.take_count();
        let Some(op) = op else {
            self.move_to_target(motion, count);
            return Effect::None;
        };
        // `cw` on a word behaves like `ce`, as in Vim.
        let motion = match motion {
            Motion::WordForward { big }
                if op == Op::Change
                    && self
                        .char_at((self.row, self.col))
                        .is_some_and(|c| !c.is_whitespace()) =>
            {
                Motion::WordEnd { big }
            }
            other => other,
        };
        let Some((target, kind)) = self.motion_target(motion, count) else {
            return Effect::None;
        };
        let from = (self.row, self.col);
        let (mut start, mut end) = if target < from {
            (target, from)
        } else {
            (from, target)
        };
        if kind == Kind::Exclusive
            && end.1 == 0
            && end.0 > start.0
            && matches!(motion, Motion::WordForward { .. })
        {
            // A word motion that lands on the next line's start stops at this line's end.
            end = (end.0 - 1, self.line_len(end.0 - 1));
        }
        if kind == Kind::Inclusive {
            end = (end.0, (end.1 + 1).min(self.line_len(end.0)));
        }
        if kind == Kind::Linewise {
            start = (start.0, 0);
            end = (end.0, self.line_len(end.0));
        }
        self.apply_op(op, start, end, kind)
    }

    fn move_to_target(&mut self, motion: Motion, count: usize) {
        if let Some((target, _)) = self.motion_target(motion, count) {
            self.row = target.0;
            self.col = target.1;
            self.clamp_normal();
        }
    }

    fn motion_target(&self, motion: Motion, count: usize) -> Option<(Pos, Kind)> {
        let mut pos = (self.row, self.col);
        let last_row = self.lines.len() - 1;
        match motion {
            Motion::Left => Some(((pos.0, pos.1.saturating_sub(count)), Kind::Exclusive)),
            Motion::Right => {
                let len = self.line_len(pos.0);
                Some(((pos.0, (pos.1 + count).min(len)), Kind::Exclusive))
            }
            Motion::Up => Some(((pos.0.saturating_sub(count), pos.1), Kind::Linewise)),
            Motion::Down => Some((((pos.0 + count).min(last_row), pos.1), Kind::Linewise)),
            Motion::LineStart => Some(((pos.0, 0), Kind::Exclusive)),
            Motion::FirstNonBlank => Some(((pos.0, self.first_non_blank(pos.0)), Kind::Exclusive)),
            Motion::LineEnd => {
                let row = (pos.0 + count - 1).min(last_row);
                Some(((row, self.line_len(row).saturating_sub(1)), Kind::Inclusive))
            }
            Motion::Top => {
                let row = (count - 1).min(last_row);
                Some(((row, self.first_non_blank(row)), Kind::Linewise))
            }
            Motion::Bottom => {
                let row = if count > 1 {
                    (count - 1).min(last_row)
                } else {
                    last_row
                };
                Some(((row, self.first_non_blank(row)), Kind::Linewise))
            }
            Motion::WordForward { big } => {
                for _ in 0..count {
                    pos = self.next_word_start(pos, big);
                }
                Some((pos, Kind::Exclusive))
            }
            Motion::WordEnd { big } => {
                for _ in 0..count {
                    pos = self.word_end(pos, big);
                }
                Some((pos, Kind::Inclusive))
            }
            Motion::WordBack { big } => {
                for _ in 0..count {
                    pos = self.word_back(pos, big);
                }
                Some((pos, Kind::Exclusive))
            }
            Motion::Find(kind, ch) => {
                let chars: Vec<char> = self.lines[pos.0].chars().collect();
                let mut col = pos.1;
                for _ in 0..count {
                    col = match kind {
                        FindKind::Forward | FindKind::TillForward => {
                            let adjacent = kind == FindKind::TillForward
                                && self.vim.find_repeat
                                && chars.get(col + 1) == Some(&ch);
                            let skip = if adjacent { col + 2 } else { col + 1 };
                            (skip..chars.len()).find(|&i| chars[i] == ch)?
                        }
                        FindKind::Backward | FindKind::TillBackward => {
                            let adjacent = kind == FindKind::TillBackward
                                && self.vim.find_repeat
                                && col >= 1
                                && chars.get(col - 1) == Some(&ch);
                            let end = if adjacent { col - 1 } else { col };
                            (0..end).rev().find(|&i| chars[i] == ch)?
                        }
                    };
                }
                let col = match kind {
                    FindKind::TillForward => col - 1,
                    FindKind::TillBackward => col + 1,
                    _ => col,
                };
                let inclusive = matches!(kind, FindKind::Forward | FindKind::TillForward);
                Some((
                    (pos.0, col),
                    if inclusive {
                        Kind::Inclusive
                    } else {
                        Kind::Exclusive
                    },
                ))
            }
        }
    }

    // ── Buffer helpers ─────────────────────────────────────────────────

    fn line_len(&self, row: usize) -> usize {
        self.lines[row].chars().count()
    }

    fn first_non_blank(&self, row: usize) -> usize {
        self.lines[row]
            .chars()
            .position(|c| !c.is_whitespace())
            .unwrap_or(0)
    }

    /// Character at a position; the end of every line but the last reads as a newline.
    fn char_at(&self, pos: Pos) -> Option<char> {
        let line = self.lines.get(pos.0)?;
        match line.chars().nth(pos.1) {
            Some(c) => Some(c),
            None if pos.0 + 1 < self.lines.len() => Some('\n'),
            None => None,
        }
    }

    fn next_pos(&self, pos: Pos) -> Option<Pos> {
        if pos.1 < self.line_len(pos.0) {
            Some((pos.0, pos.1 + 1))
        } else if pos.0 + 1 < self.lines.len() {
            Some((pos.0 + 1, 0))
        } else {
            None
        }
    }

    fn prev_pos(&self, pos: Pos) -> Option<Pos> {
        if pos.1 > 0 {
            Some((pos.0, pos.1 - 1))
        } else if pos.0 > 0 {
            Some((pos.0 - 1, self.line_len(pos.0 - 1)))
        } else {
            None
        }
    }

    fn class_at(&self, pos: Pos, big: bool) -> Option<u8> {
        self.char_at(pos).map(|c| class(c, big))
    }

    fn next_word_start(&self, mut pos: Pos, big: bool) -> Pos {
        let Some(start_class) = self.class_at(pos, big) else {
            return pos;
        };
        if start_class != 0 {
            while let Some(next) = self.next_pos(pos) {
                if self.class_at(next, big) != Some(start_class) || next.0 != pos.0 {
                    pos = next;
                    break;
                }
                pos = next;
            }
            if self.class_at(pos, big) == Some(start_class) && self.next_pos(pos).is_none() {
                return pos;
            }
        }
        // Skip blanks; an empty line counts as a word.
        loop {
            match self.char_at(pos) {
                None => return pos,
                Some('\n') if self.line_len(pos.0) == 0 => return pos,
                Some(c) if c.is_whitespace() => match self.next_pos(pos) {
                    Some(next) => pos = next,
                    None => return pos,
                },
                Some(_) => return pos,
            }
        }
    }

    fn word_end(&self, mut pos: Pos, big: bool) -> Pos {
        let Some(mut next) = self.next_pos(pos) else {
            return pos;
        };
        pos = next;
        while self.char_at(pos).is_some_and(|c| c.is_whitespace()) {
            match self.next_pos(pos) {
                Some(n) => pos = n,
                None => return pos,
            }
        }
        let Some(cls) = self.class_at(pos, big) else {
            return pos;
        };
        loop {
            next = match self.next_pos(pos) {
                Some(n) => n,
                None => return pos,
            };
            if next.0 != pos.0 || self.class_at(next, big) != Some(cls) {
                return pos;
            }
            pos = next;
        }
    }

    fn word_back(&self, mut pos: Pos, big: bool) -> Pos {
        let Some(prev) = self.prev_pos(pos) else {
            return pos;
        };
        pos = prev;
        // Skip blanks backwards; an empty line stops.
        loop {
            match self.char_at(pos) {
                Some('\n') if self.line_len(pos.0) == 0 => return pos,
                Some(c) if c.is_whitespace() => match self.prev_pos(pos) {
                    Some(p) => pos = p,
                    None => return pos,
                },
                _ => break,
            }
        }
        let Some(cls) = self.class_at(pos, big) else {
            return pos;
        };
        while let Some(prev) = self.prev_pos(pos) {
            if prev.0 != pos.0 || self.class_at(prev, big) != Some(cls) {
                break;
            }
            pos = prev;
        }
        pos
    }

    /// Range of a text object around the cursor, end exclusive.
    fn text_object(&self, ch: char, inner: bool, count: usize) -> Option<(Pos, Pos)> {
        let row = self.row;
        let chars: Vec<char> = self.lines[row].chars().collect();
        match ch {
            'w' | 'W' => {
                if chars.is_empty() {
                    return None;
                }
                let big = ch == 'W';
                let cls = class(chars[self.col], big);
                let mut start = self.col;
                while start > 0 && class(chars[start - 1], big) == cls {
                    start -= 1;
                }
                let mut end = self.col;
                for _ in 0..count {
                    let c = class(chars[end], big);
                    while end < chars.len() && class(chars[end], big) == c {
                        end += 1;
                    }
                    if !inner && c != 0 {
                        // `aw` takes the trailing blanks too.
                        while end < chars.len() && chars[end].is_whitespace() {
                            end += 1;
                        }
                    }
                    if end >= chars.len() {
                        break;
                    }
                }
                if !inner && cls != 0 && end == chars.len() {
                    // No trailing blanks: take the leading ones instead.
                    while start > 0 && chars[start - 1].is_whitespace() {
                        start -= 1;
                    }
                }
                Some(((row, start), (row, end)))
            }
            '"' | '\'' | '`' => {
                let quotes: Vec<usize> = chars
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| **c == ch)
                    .map(|(i, _)| i)
                    .collect();
                // Pair quotes from the line start; pick the pair containing or following the cursor.
                let pair = quotes
                    .chunks(2)
                    .filter(|p| p.len() == 2)
                    .find(|p| self.col <= p[1])?;
                let (open, close) = (pair[0], pair[1]);
                if inner {
                    Some(((row, open + 1), (row, close)))
                } else {
                    Some(((row, open), (row, close + 1)))
                }
            }
            '(' | ')' | 'b' => self.bracket_object('(', ')', inner),
            '[' | ']' => self.bracket_object('[', ']', inner),
            '{' | '}' | 'B' => self.bracket_object('{', '}', inner),
            '<' | '>' => self.bracket_object('<', '>', inner),
            _ => None,
        }
    }

    fn bracket_object(&self, open: char, close: char, inner: bool) -> Option<(Pos, Pos)> {
        let cursor = (self.row, self.col);
        // Find the unmatched opening bracket at or before the cursor.
        let mut pos = cursor;
        let mut depth = 0i32;
        let start = loop {
            match self.char_at(pos) {
                Some(c) if c == close && pos != cursor => depth += 1,
                Some(c) if c == open => {
                    if depth == 0 {
                        break pos;
                    }
                    depth -= 1;
                }
                _ => {}
            }
            pos = self.prev_pos(pos)?;
        };
        let mut pos = self.next_pos(start)?;
        let mut depth = 0i32;
        let end = loop {
            match self.char_at(pos) {
                Some(c) if c == open => depth += 1,
                Some(c) if c == close => {
                    if depth == 0 {
                        break pos;
                    }
                    depth -= 1;
                }
                None => return None,
                _ => {}
            }
            pos = self.next_pos(pos)?;
        };
        if inner {
            Some((self.next_pos(start)?, end))
        } else {
            Some((start, self.next_pos(end).unwrap_or(end)))
        }
    }

    // ── Operators ──────────────────────────────────────────────────────

    fn apply_to_line_end(&mut self, op: Op) -> Effect {
        let len = self.line_len(self.row);
        let start = (self.row, self.col.min(len));
        self.apply_op(op, start, (self.row, len), Kind::Exclusive)
    }

    fn apply_lines(&mut self, op: Op, count: usize) -> Effect {
        let last = (self.row + count - 1).min(self.lines.len() - 1);
        let start = (self.row, 0);
        let end = (last, self.line_len(last));
        self.apply_op(op, start, end, Kind::Linewise)
    }

    /// Slice of the buffer between two positions, end exclusive.
    fn slice(&self, start: Pos, end: Pos) -> String {
        if start.0 == end.0 {
            return self.lines[start.0]
                .chars()
                .skip(start.1)
                .take(end.1.saturating_sub(start.1))
                .collect();
        }
        let mut out: String = self.lines[start.0].chars().skip(start.1).collect();
        for row in start.0 + 1..end.0 {
            out.push('\n');
            out.push_str(&self.lines[row]);
        }
        out.push('\n');
        out.extend(self.lines[end.0].chars().take(end.1));
        out
    }

    fn remove(&mut self, start: Pos, end: Pos) {
        let head: String = self.lines[start.0].chars().take(start.1).collect();
        let tail: String = self.lines[end.0].chars().skip(end.1).collect();
        self.lines.drain(start.0..=end.0);
        self.lines.insert(start.0, head + &tail);
    }

    fn apply_op(&mut self, op: Op, start: Pos, end: Pos, kind: Kind) -> Effect {
        let text = self.slice(start, end);
        let linewise = kind == Kind::Linewise;
        if op == Op::Yank {
            self.vim.register = Some(Register {
                text: text.clone(),
                linewise,
            });
            if !linewise {
                self.row = start.0;
                self.col = start.1;
            }
            self.clamp_normal();
            return Effect::Yanked(text);
        }
        self.checkpoint();
        self.vim.register = Some(Register {
            text: text.clone(),
            linewise,
        });
        if linewise {
            if op == Op::Change {
                // Keep one empty line to type into.
                self.lines.drain(start.0..=end.0);
                self.lines.insert(start.0, String::new());
                self.row = start.0;
                self.col = 0;
                return Effect::EnterInsert;
            }
            self.lines.drain(start.0..=end.0);
            if self.lines.is_empty() {
                self.lines.push(String::new());
            }
            self.row = start.0.min(self.lines.len() - 1);
            self.col = self.first_non_blank(self.row);
            self.clamp_normal();
            return Effect::None;
        }
        self.remove(start, end);
        self.row = start.0;
        self.col = start.1;
        if op == Op::Change {
            return Effect::EnterInsert;
        }
        self.clamp_normal();
        Effect::None
    }

    fn paste(&mut self, after: bool) -> Effect {
        let count = self.take_count();
        let Some(register) = self.vim.register.clone() else {
            return Effect::None;
        };
        self.checkpoint();
        if register.linewise {
            let mut lines: Vec<String> = Vec::new();
            for _ in 0..count {
                lines.extend(register.text.split('\n').map(str::to_string));
            }
            let at = if after { self.row + 1 } else { self.row };
            self.lines.splice(at..at, lines);
            self.row = at;
            self.col = self.first_non_blank(self.row);
        } else {
            let len = self.line_len(self.row);
            if after && len > 0 {
                self.col = (self.col + 1).min(len);
            }
            let text = register.text.repeat(count);
            let (row, col) = (self.row, self.col);
            self.insert_at(row, col, &text);
            // Cursor rests on the last pasted character.
            if let Some(prev) = self.prev_pos((self.row, self.col)) {
                self.row = prev.0;
                self.col = prev.1;
            }
        }
        self.clamp_normal();
        Effect::None
    }

    /// Insert text at a position, leaving the cursor after it.
    fn insert_at(&mut self, row: usize, col: usize, text: &str) {
        self.row = row;
        self.col = col;
        self.insert_str(text);
    }

    fn undo(&mut self) {
        let Some(snapshot) = self.vim.undo.pop() else {
            return;
        };
        self.vim.redo.push(Snapshot {
            lines: self.lines.clone(),
            row: self.row,
            col: self.col,
        });
        self.restore(snapshot);
    }

    fn redo(&mut self) {
        let Some(snapshot) = self.vim.redo.pop() else {
            return;
        };
        self.vim.undo.push(Snapshot {
            lines: self.lines.clone(),
            row: self.row,
            col: self.col,
        });
        self.restore(snapshot);
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.lines = snapshot.lines;
        self.row = snapshot.row;
        self.col = snapshot.col;
        self.clamp_normal();
    }
}

fn op_char(op: Op) -> char {
    match op {
        Op::Delete => 'd',
        Op::Change => 'c',
        Op::Yank => 'y',
    }
}

fn motion_for(code: KeyCode) -> Option<Motion> {
    Some(match code {
        KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => Motion::Left,
        KeyCode::Char('l') | KeyCode::Right | KeyCode::Char(' ') => Motion::Right,
        KeyCode::Char('k') | KeyCode::Up => Motion::Up,
        KeyCode::Char('j') | KeyCode::Down => Motion::Down,
        KeyCode::Char('w') => Motion::WordForward { big: false },
        KeyCode::Char('W') => Motion::WordForward { big: true },
        KeyCode::Char('e') => Motion::WordEnd { big: false },
        KeyCode::Char('E') => Motion::WordEnd { big: true },
        KeyCode::Char('b') => Motion::WordBack { big: false },
        KeyCode::Char('B') => Motion::WordBack { big: true },
        KeyCode::Char('0') | KeyCode::Home => Motion::LineStart,
        KeyCode::Char('^') => Motion::FirstNonBlank,
        KeyCode::Char('$') | KeyCode::End => Motion::LineEnd,
        KeyCode::Char('G') => Motion::Bottom,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composer(text: &str) -> Composer {
        let mut c = Composer::new();
        c.set_text(text);
        c.row = 0;
        c.col = 0;
        c
    }

    fn keys(c: &mut Composer, input: &str) -> Vec<Effect> {
        input
            .chars()
            .map(|ch| c.vim_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)))
            .collect()
    }

    #[test]
    fn word_motions() {
        let mut c = composer("foo bar.baz  qux");
        keys(&mut c, "w");
        assert_eq!((c.row, c.col), (0, 4));
        keys(&mut c, "w");
        assert_eq!((c.row, c.col), (0, 7));
        keys(&mut c, "W");
        assert_eq!((c.row, c.col), (0, 13));
        keys(&mut c, "b");
        assert_eq!((c.row, c.col), (0, 8));
        keys(&mut c, "e");
        assert_eq!((c.row, c.col), (0, 10));
        keys(&mut c, "$");
        assert_eq!((c.row, c.col), (0, 15));
        keys(&mut c, "0");
        assert_eq!((c.row, c.col), (0, 0));
    }

    #[test]
    fn words_cross_lines() {
        let mut c = composer("one\ntwo three");
        keys(&mut c, "w");
        assert_eq!((c.row, c.col), (1, 0));
        keys(&mut c, "w");
        assert_eq!((c.row, c.col), (1, 4));
        keys(&mut c, "bb");
        assert_eq!((c.row, c.col), (0, 0));
    }

    #[test]
    fn find_and_till() {
        let mut c = composer("a,b,c,d");
        keys(&mut c, "f,");
        assert_eq!(c.col, 1);
        keys(&mut c, ";");
        assert_eq!(c.col, 3);
        keys(&mut c, ",");
        assert_eq!(c.col, 1);
        keys(&mut c, "t,");
        assert_eq!(c.col, 2);
        keys(&mut c, "0dt,");
        assert_eq!(c.text(), ",b,c,d");
        keys(&mut c, "$F,");
        assert_eq!(c.col, 4);
        keys(&mut c, "D");
        assert_eq!(c.text(), ",b,c");
    }

    #[test]
    fn operators_and_counts() {
        let mut c = composer("alpha beta gamma delta");
        keys(&mut c, "dw");
        assert_eq!(c.text(), "beta gamma delta");
        keys(&mut c, "2dw");
        assert_eq!(c.text(), "delta");
        keys(&mut c, "x");
        assert_eq!(c.text(), "elta");
        keys(&mut c, "u");
        assert_eq!(c.text(), "delta");
        keys(&mut c, "uu");
        assert_eq!(c.text(), "alpha beta gamma delta");
        keys(&mut c, "d$");
        assert_eq!(c.text(), "");
    }

    #[test]
    fn change_word_enters_insert_like_ce() {
        let mut c = composer("hello world");
        let effects = keys(&mut c, "cw");
        assert_eq!(effects.last(), Some(&Effect::EnterInsert));
        assert_eq!(c.text(), " world");
        assert_eq!(c.col, 0);
    }

    #[test]
    fn linewise_operators_and_paste() {
        let mut c = composer("one\ntwo\nthree");
        keys(&mut c, "jdd");
        assert_eq!(c.text(), "one\nthree");
        assert_eq!(c.row, 1);
        keys(&mut c, "p");
        assert_eq!(c.text(), "one\nthree\ntwo");
        c.vim_top();
        keys(&mut c, "yyP");
        assert_eq!(c.text(), "one\none\nthree\ntwo");
        keys(&mut c, "3dd");
        assert_eq!(c.text(), "two");
        keys(&mut c, "cc");
        assert_eq!(c.text(), "");
    }

    #[test]
    fn charwise_paste_and_register() {
        let mut c = composer("abc");
        keys(&mut c, "yl$p");
        assert_eq!(c.text(), "abca");
        keys(&mut c, "0xP");
        assert_eq!(c.text(), "abca");
    }

    #[test]
    fn text_objects() {
        let mut c = composer("say \"hello there\" (to (them) all)");
        keys(&mut c, "fhdi\"");
        assert_eq!(c.text(), "say \"\" (to (them) all)");
        keys(&mut c, "fmci(");
        assert_eq!(c.text(), "say \"\" (to () all)");
        keys(&mut c, "0f(da(");
        assert_eq!(c.text(), "say \"\" ");
        let mut c = composer("one two three");
        keys(&mut c, "wdaw");
        assert_eq!(c.text(), "one three");
        keys(&mut c, "wdiw");
        assert_eq!(c.text(), "one ");
    }

    #[test]
    fn insert_entry_positions() {
        let mut c = composer("  text");
        keys(&mut c, "A");
        assert_eq!(c.col, 6);
        c.leave_insert();
        keys(&mut c, "I");
        assert_eq!(c.col, 2);
        c.leave_insert();
        keys(&mut c, "o");
        assert_eq!((c.row, c.col), (1, 0));
        assert_eq!(c.text(), "  text\n");
        c.leave_insert();
        keys(&mut c, "O");
        assert_eq!(c.text(), "  text\n\n");
        assert_eq!(c.row, 1);
    }

    #[test]
    fn replace_and_case() {
        let mut c = composer("abcd");
        keys(&mut c, "rx");
        assert_eq!(c.text(), "xbcd");
        keys(&mut c, "l2~");
        assert_eq!(c.text(), "xBCd");
        keys(&mut c, "0ry");
        assert_eq!(c.text(), "yBCd");
    }
}

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
    /// `i` or `a` in visual mode: the object becomes the selection.
    Select {
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

/// A selection being made, anchored where it started. The other end is the cursor, so a
/// selection is extended by moving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Visual {
    anchor: Pos,
    pub linewise: bool,
}

/// What a change did, as what was meant rather than as the keys that meant it: `.`
/// repeats one by running the same command where the cursor is now. Vim replays the
/// keystrokes; this replays the command, which comes to the same thing everywhere it
/// matters and keeps the engine the one place that decides what a command does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum What {
    /// An operator over a motion, `dw` and its kind.
    Motion {
        op: Op,
        motion: Motion,
        count: usize,
    },
    /// An operator over a text object, `ciw` and its kind.
    Object {
        op: Op,
        object: char,
        inner: bool,
        count: usize,
    },
    /// The line forms, `dd` and `cc`.
    Lines {
        op: Op,
        count: usize,
    },
    /// `D` and `C`.
    ToLineEnd {
        op: Op,
    },
    /// `x` and `X`.
    Erase {
        count: usize,
        back: bool,
    },
    Replace {
        ch: char,
        count: usize,
    },
    Case {
        count: usize,
    },
    Paste {
        after: bool,
        count: usize,
    },
    /// Insert entry: the key that opened it, `i a I A o O`.
    Enter {
        key: char,
    },
    Join {
        count: usize,
    },
}

/// A change and whatever was typed into the insert it opened.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    what: What,
    typed: Option<String>,
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
    visual: Option<Visual>,
    /// The change `.` repeats.
    last_change: Option<Change>,
    /// The text as the insert now being typed found it, so that what is typed into it
    /// can be read off the buffer when it ends and repeated with the change that opened
    /// it. Vim records the keys instead; the text they left is the part `.` needs.
    insert_before: Option<Vec<String>>,
    /// Set while `.` runs, so a repeat does not record itself as the change to repeat.
    replaying: bool,
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
            Pending::Select { .. } => {}
            Pending::Replace => out.push('r'),
        }
        if let Pending::Object { inner, .. } | Pending::Select { inner } = self.vim.pending {
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
        self.vim.visual = None;
    }

    /// Whether the composer is in the middle of something — a half-typed command or a
    /// selection being made. The app reads this before taking a key for itself: a key
    /// that is part of something already begun belongs to the editor.
    pub fn vim_busy(&self) -> bool {
        self.vim_pending() || self.vim.visual.is_some()
    }

    /// Forget an insert that was open: the text has been replaced from outside, so
    /// whatever stands there now is nobody's typing and `.` should not offer it.
    pub(crate) fn vim_forget_insert(&mut self) {
        self.vim.insert_before = None;
    }

    /// The selection being made, for the status bar and for drawing it.
    pub fn vim_visual(&self) -> Option<Visual> {
        self.vim.visual
    }

    /// The selection as positions in the text, end exclusive. Linewise covers whole
    /// lines; charwise takes in the character the cursor is on, as Vim does.
    fn visual_range(&self) -> Option<(Pos, Pos)> {
        let visual = self.vim.visual?;
        let cursor = (self.row, self.col);
        let (start, end) = if visual.anchor <= cursor {
            (visual.anchor, cursor)
        } else {
            (cursor, visual.anchor)
        };
        Some(if visual.linewise {
            ((start.0, 0), (end.0, self.line_len(end.0)))
        } else {
            (start, (end.0, (end.1 + 1).min(self.line_len(end.0))))
        })
    }

    /// Which characters of a line are selected, as columns, end exclusive. A line inside
    /// the selection is selected to its end, so a run over several lines reads as one.
    pub(crate) fn selected_columns(&self, row: usize) -> Option<(usize, usize)> {
        let (start, end) = self.visual_range()?;
        if row < start.0 || row > end.0 {
            return None;
        }
        let from = if row == start.0 { start.1 } else { 0 };
        let to = if row == end.0 {
            end.1
        } else {
            self.line_len(row)
        };
        Some((from, to.max(from)))
    }

    /// Leaving insert mode: Vim steps the cursor back onto the last typed character.
    /// What was typed goes to the change that opened the insert, so `.` can type it
    /// again — read off the buffer rather than out of the keys, so a word backspaced and
    /// written again repeats as what was left, which is what was meant.
    pub fn leave_insert(&mut self) {
        if let Some(before) = self.vim.insert_before.take()
            && let Some(typed) = typed_text(&before, &self.lines)
            && let Some(change) = self.vim.last_change.as_mut()
        {
            change.typed = Some(typed);
        }
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
            Pending::Select { inner } => {
                self.vim.pending = Pending::None;
                let KeyCode::Char(ch) = key.code else {
                    self.vim_cancel();
                    return Effect::None;
                };
                let count = self.take_count();
                if let Some((start, end)) = self.text_object(ch, inner, count) {
                    self.vim.visual = Some(Visual {
                        anchor: start,
                        linewise: false,
                    });
                    self.row = end.0;
                    self.col = end.1.saturating_sub(1);
                    self.clamp_normal();
                }
                Effect::None
            }
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
                    Some((start, end)) => {
                        let effect = self.apply_op(op, start, end, Kind::Exclusive);
                        let what = What::Object {
                            op,
                            object: ch,
                            inner,
                            count,
                        };
                        self.changed(op, what, effect)
                    }
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
                let effect = self.replace_chars(ch, count);
                self.recorded(What::Replace { ch, count }, effect)
            }
            Pending::Op(op) => self.op_key(op, key.code),
            Pending::None if self.vim.visual.is_some() => self.visual_key(key.code),
            Pending::None => self.plain_key(key.code),
        }
    }

    /// A key with a selection up. The operators take the selection instead of waiting
    /// for a motion; everything else moves the cursor, which is what extends it.
    fn visual_key(&mut self, code: KeyCode) -> Effect {
        match code {
            KeyCode::Char(c @ '1'..='9') => {
                self.push_digit(c as usize - '0' as usize);
                Effect::None
            }
            KeyCode::Char('0') if self.vim.count.is_some() => {
                self.push_digit(0);
                Effect::None
            }
            // The same key again drops the selection; the other one changes what it
            // covers, as in Vim.
            KeyCode::Char('v') => {
                self.set_visual(false);
                Effect::None
            }
            KeyCode::Char('V') => {
                self.set_visual(true);
                Effect::None
            }
            // Swap the ends, so a selection made in the wrong direction can be grown the
            // other way rather than started again.
            KeyCode::Char('o') => {
                if let Some(visual) = self.vim.visual.as_mut() {
                    let anchor = std::mem::replace(&mut visual.anchor, (self.row, self.col));
                    self.row = anchor.0;
                    self.col = anchor.1;
                    self.clamp_normal();
                }
                Effect::None
            }
            KeyCode::Char('d') | KeyCode::Char('x') | KeyCode::Delete => self.visual_op(Op::Delete),
            KeyCode::Char('c') | KeyCode::Char('s') => self.visual_op(Op::Change),
            KeyCode::Char('y') => self.visual_op(Op::Yank),
            // The shifted forms take whole lines, whatever the selection covers.
            KeyCode::Char('D') | KeyCode::Char('X') => {
                self.whole_lines();
                self.visual_op(Op::Delete)
            }
            KeyCode::Char('S') | KeyCode::Char('C') => {
                self.whole_lines();
                self.visual_op(Op::Change)
            }
            KeyCode::Char('Y') => {
                self.whole_lines();
                self.visual_op(Op::Yank)
            }
            KeyCode::Char('~') => {
                let Some((start, end)) = self.visual_range() else {
                    return Effect::None;
                };
                self.vim.visual = None;
                self.flip_case(start, end);
                Effect::None
            }
            KeyCode::Char('J') => {
                let (start, end) = match self.visual_range() {
                    Some(range) => range,
                    None => return Effect::None,
                };
                self.vim.visual = None;
                self.row = start.0;
                self.col = 0;
                self.join(end.0 - start.0 + 1);
                Effect::None
            }
            KeyCode::Char('i') => {
                self.vim.pending = Pending::Select { inner: true };
                Effect::None
            }
            KeyCode::Char('a') => {
                self.vim.pending = Pending::Select { inner: false };
                Effect::None
            }
            KeyCode::Char('f') => self.start_find(None, FindKind::Forward),
            KeyCode::Char('F') => self.start_find(None, FindKind::Backward),
            KeyCode::Char('t') => self.start_find(None, FindKind::TillForward),
            KeyCode::Char('T') => self.start_find(None, FindKind::TillBackward),
            KeyCode::Char(';') => self.repeat_find(None, false),
            KeyCode::Char(',') => self.repeat_find(None, true),
            // A key that means nothing here leaves the selection alone rather than
            // dropping it: it costs a keystroke to make and `Esc` is how to let it go.
            _ => match motion_for(code) {
                Some(motion) => self.run_motion(None, motion),
                None => Effect::None,
            },
        }
    }

    /// Start a selection, or change or drop the one there is. Asking for the kind it
    /// already has is how Vim says it is finished with it.
    fn set_visual(&mut self, linewise: bool) {
        match self.vim.visual {
            Some(visual) if visual.linewise == linewise => self.vim.visual = None,
            Some(_) => {
                if let Some(visual) = self.vim.visual.as_mut() {
                    visual.linewise = linewise;
                }
            }
            None => {
                self.vim.visual = Some(Visual {
                    anchor: (self.row, self.col),
                    linewise,
                });
            }
        }
    }

    /// Apply an operator to the selection, which the operator then ends.
    fn visual_op(&mut self, op: Op) -> Effect {
        let Some(visual) = self.vim.visual else {
            return Effect::None;
        };
        let Some((start, end)) = self.visual_range() else {
            return Effect::None;
        };
        self.vim.visual = None;
        self.vim.count = None;
        let kind = if visual.linewise {
            Kind::Linewise
        } else {
            Kind::Exclusive
        };
        self.apply_op(op, start, end, kind)
    }

    /// Take whole lines, whatever the selection covers: the shifted operators do.
    fn whole_lines(&mut self) {
        if let Some(visual) = self.vim.visual.as_mut() {
            visual.linewise = true;
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
                let effect = self.erase(count, false);
                self.recorded(What::Erase { count, back: false }, effect)
            }
            KeyCode::Char('X') => {
                let count = self.take_count();
                let effect = self.erase(count, true);
                self.recorded(What::Erase { count, back: true }, effect)
            }
            KeyCode::Char('D') => {
                self.take_count();
                let effect = self.apply_to_line_end(Op::Delete);
                self.recorded(What::ToLineEnd { op: Op::Delete }, effect)
            }
            KeyCode::Char('C') => {
                self.take_count();
                let effect = self.apply_to_line_end(Op::Change);
                self.recorded(What::ToLineEnd { op: Op::Change }, effect)
            }
            KeyCode::Char('Y') => {
                let count = self.take_count();
                self.apply_lines(Op::Yank, count)
            }
            KeyCode::Char('p') | KeyCode::Char('P') => {
                let after = code == KeyCode::Char('p');
                let count = self.take_count();
                let effect = self.paste(after, count);
                self.recorded(What::Paste { after, count }, effect)
            }
            KeyCode::Char('r') => {
                self.vim.pending = Pending::Replace;
                Effect::None
            }
            KeyCode::Char('~') => {
                let count = self.take_count();
                let effect = self.flip_case_forward(count);
                self.recorded(What::Case { count }, effect)
            }
            KeyCode::Char('u') => {
                self.vim_cancel();
                self.undo();
                Effect::None
            }
            KeyCode::Char('.') => self.repeat(),
            // Bare `J` opens the next thread and never reaches here. `3J` does, because
            // a count is the composer's — and a count is the reason to reach for `J`
            // rather than `gJ`.
            KeyCode::Char('J') => {
                let count = self.take_count().max(2);
                self.join(count);
                self.recorded(What::Join { count }, Effect::None)
            }
            KeyCode::Char('v') => {
                self.take_count();
                self.set_visual(false);
                Effect::None
            }
            KeyCode::Char('V') => {
                self.take_count();
                self.set_visual(true);
                Effect::None
            }
            KeyCode::Char(key @ ('i' | 'a' | 'I' | 'A' | 'o' | 'O')) => {
                let effect = self.enter_insert(key);
                self.recorded(What::Enter { key }, effect)
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
                let effect = self.apply_lines(op, count);
                self.changed(op, What::Lines { op, count }, effect)
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
        let what = What::Motion { op, motion, count };
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
        let effect = self.apply_op(op, start, end, kind);
        self.changed(op, what, effect)
    }

    /// Keep a change for `.`, unless it changed nothing: a yank is not a change, and
    /// Vim does not repeat one.
    fn changed(&mut self, op: Op, what: What, effect: Effect) -> Effect {
        match op {
            Op::Yank => effect,
            _ => self.recorded(what, effect),
        }
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

    // ── The changes themselves ─────────────────────────────────────────

    /// `x` and `X`: characters either side of the cursor.
    fn erase(&mut self, count: usize, back: bool) -> Effect {
        if back {
            if self.col == 0 {
                return Effect::None;
            }
            let start = (self.row, self.col.saturating_sub(count));
            return self.apply_op(Op::Delete, start, (self.row, self.col), Kind::Exclusive);
        }
        let len = self.line_len(self.row);
        if len == 0 {
            return Effect::None;
        }
        let end = (self.row, (self.col + count).min(len));
        self.apply_op(Op::Delete, (self.row, self.col), end, Kind::Exclusive)
    }

    /// `r`: the character under the cursor, and the ones after it with a count.
    fn replace_chars(&mut self, ch: char, count: usize) -> Effect {
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

    /// `~`: the characters from the cursor on, leaving it after them.
    fn flip_case_forward(&mut self, count: usize) -> Effect {
        let len = self.line_len(self.row);
        if len == 0 {
            return Effect::None;
        }
        let end = (self.col + count).min(len);
        self.flip_case((self.row, self.col), (self.row, end));
        self.col = end.min(len - 1);
        Effect::None
    }

    /// Turn a stretch of text the other way round, cursor left at its start.
    fn flip_case(&mut self, start: Pos, end: Pos) {
        let text = self.slice(start, end);
        if text.is_empty() {
            return;
        }
        let flipped: String = text
            .chars()
            .map(|c| {
                if c.is_uppercase() {
                    c.to_lowercase().next().unwrap_or(c)
                } else {
                    c.to_uppercase().next().unwrap_or(c)
                }
            })
            .collect();
        self.checkpoint();
        self.remove(start, end);
        self.insert_at(start.0, start.1, &flipped);
        self.row = start.0;
        self.col = start.1;
        self.clamp_normal();
    }

    /// Insert entry: `i a I A o O`, the cursor put where each of them puts it.
    fn enter_insert(&mut self, key: char) -> Effect {
        self.vim_cancel();
        self.checkpoint();
        match key {
            'a' => self.col = (self.col + 1).min(self.line_len(self.row)),
            'I' => self.col = self.first_non_blank(self.row),
            'A' => self.col = self.line_len(self.row),
            'o' => {
                self.lines.insert(self.row + 1, String::new());
                self.row += 1;
                self.col = 0;
            }
            'O' => {
                self.lines.insert(self.row, String::new());
                self.col = 0;
            }
            _ => {}
        }
        Effect::EnterInsert
    }

    /// `gJ`, and `J` over a selection: the lines after this one pulled onto it, a single
    /// space where they meet. `J` itself opens the next thread, so the join is on `gJ`.
    /// The cursor lands where the join is, as Vim leaves it.
    pub fn vim_join(&mut self, count: usize) {
        let count = count.max(2);
        self.vim_cancel();
        self.join(count);
    }

    fn join(&mut self, count: usize) {
        if self.row + 1 >= self.lines.len() {
            return;
        }
        self.checkpoint();
        for _ in 0..count.max(2) - 1 {
            if self.row + 1 >= self.lines.len() {
                break;
            }
            let next = self.lines.remove(self.row + 1);
            let line = &mut self.lines[self.row];
            let trimmed = line.trim_end().to_string();
            let next = next.trim_start();
            *line = if trimmed.is_empty() {
                next.to_string()
            } else if next.is_empty() {
                trimmed
            } else {
                format!("{trimmed} {next}")
            };
            self.col = self.lines[self.row]
                .chars()
                .count()
                .saturating_sub(next.chars().count() + 1)
                .min(self.line_len(self.row));
        }
        self.clamp_normal();
    }

    /// Keep a change for `.`, and where it opened an insert, keep the text as it stands
    /// so that what is typed next can be read off against it.
    fn recorded(&mut self, what: What, effect: Effect) -> Effect {
        if self.vim.replaying {
            return effect;
        }
        self.vim.last_change = Some(Change { what, typed: None });
        self.vim.insert_before = (effect == Effect::EnterInsert).then(|| self.lines.clone());
        effect
    }

    /// `.`: the last change again, where the cursor is now. A change that opened an
    /// insert types what was typed into it and stays in normal mode, so a repeat is one
    /// key rather than one key and an `Esc`.
    fn repeat(&mut self) -> Effect {
        let count = self.vim.count.take();
        self.vim.op_count = None;
        let Some(change) = self.vim.last_change.clone() else {
            return Effect::None;
        };
        self.vim.replaying = true;
        let effect = self.run_change(change.what, count);
        self.vim.replaying = false;
        if effect != Effect::EnterInsert {
            return effect;
        }
        // An insert left without typing anything types nothing again.
        if let Some(text) = &change.typed {
            self.insert_str(text);
            self.col = self.col.saturating_sub(1);
        }
        self.clamp_normal();
        Effect::None
    }

    /// Run a recorded change. A count typed before `.` replaces the one it was made
    /// with, as in Vim.
    fn run_change(&mut self, what: What, count: Option<usize>) -> Effect {
        match what {
            What::Motion {
                op,
                motion,
                count: n,
            } => {
                self.vim.count = Some(count.unwrap_or(n));
                self.run_motion(Some(op), motion)
            }
            What::Object {
                op,
                object,
                inner,
                count: n,
            } => {
                let n = count.unwrap_or(n);
                match self.text_object(object, inner, n) {
                    Some((start, end)) => self.apply_op(op, start, end, Kind::Exclusive),
                    None => Effect::None,
                }
            }
            What::Lines { op, count: n } => self.apply_lines(op, count.unwrap_or(n)),
            What::ToLineEnd { op } => self.apply_to_line_end(op),
            What::Erase { count: n, back } => self.erase(count.unwrap_or(n), back),
            What::Replace { ch, count: n } => self.replace_chars(ch, count.unwrap_or(n)),
            What::Case { count: n } => self.flip_case_forward(count.unwrap_or(n)),
            What::Paste { after, count: n } => self.paste(after, count.unwrap_or(n)),
            What::Enter { key } => self.enter_insert(key),
            What::Join { count: n } => {
                self.join(count.unwrap_or(n));
                Effect::None
            }
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

    fn paste(&mut self, after: bool, count: usize) -> Effect {
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

/// What an insert added: the one run of characters the text gained while it was open.
/// `None` where it gained none, or where more than typing changed it — text put there
/// from somewhere else is not something `.` should type out again.
fn typed_text(before: &[String], after: &[String]) -> Option<String> {
    let before: Vec<char> = before.join("\n").chars().collect();
    let after: Vec<char> = after.join("\n").chars().collect();
    if after.len() <= before.len() {
        return None;
    }
    let mut head = 0;
    while head < before.len() && before[head] == after[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < before.len() - head
        && before[before.len() - 1 - tail] == after[after.len() - 1 - tail]
    {
        tail += 1;
    }
    (head + tail == before.len()).then(|| after[head..after.len() - tail].iter().collect())
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

    /// Typing into an insert a change opened is part of the change: `.` makes the change
    /// again and types the same text, without stopping in insert mode on the way.
    fn typed(c: &mut Composer, text: &str) {
        c.insert_str(text);
        c.leave_insert();
    }

    #[test]
    fn a_change_can_be_made_again() {
        let mut c = composer("one two three");
        keys(&mut c, "dw");
        assert_eq!(c.text(), "two three");
        keys(&mut c, ".");
        assert_eq!(
            c.text(),
            "three",
            "the same change, where the cursor is now"
        );

        // A change that types: the text goes with it.
        let mut c = composer("alpha beta");
        assert_eq!(keys(&mut c, "cw"), vec![Effect::None, Effect::EnterInsert]);
        typed(&mut c, "ONE");
        assert_eq!(c.text(), "ONE beta");
        keys(&mut c, "w.");
        assert_eq!(c.text(), "ONE ONE", "and is typed again");
        // Still normal mode: the repeat did the typing itself.
        assert_eq!(keys(&mut c, "."), vec![Effect::None]);

        // A count before `.` replaces the one the change was made with.
        let mut c = composer("a b c d e");
        keys(&mut c, "dw");
        keys(&mut c, "2.");
        assert_eq!(c.text(), "d e");
    }

    /// Backspacing over what you have just typed is part of typing it; reaching further
    /// back, or having the text replaced from outside, is not something to repeat.
    #[test]
    fn what_a_repeat_types_is_what_the_insert_left() {
        let mut c = composer("x");
        keys(&mut c, "A");
        c.insert_str("teh");
        c.backspace();
        c.backspace();
        c.insert_str("he");
        c.leave_insert();
        assert_eq!(c.text(), "xthe");
        keys(&mut c, ".");
        assert_eq!(c.text(), "xthethe");

        // A draft dropped in from elsewhere is nobody's typing.
        let mut c = composer("one");
        keys(&mut c, "A");
        c.set_text("something else entirely");
        c.leave_insert();
        keys(&mut c, ".");
        assert_eq!(c.text(), "something else entirely");
    }

    /// A yank is not a change, so `.` after one still repeats the change before it.
    #[test]
    fn a_yank_is_not_a_change_to_repeat() {
        let mut c = composer("one two three four");
        keys(&mut c, "dw");
        assert_eq!(c.text(), "two three four");
        keys(&mut c, "yw.");
        assert_eq!(c.text(), "three four");
    }

    #[test]
    fn a_selection_is_extended_by_moving_and_taken_by_an_operator() {
        let mut c = composer("one two three");
        keys(&mut c, "vww");
        assert_eq!(c.vim_visual().map(|v| v.linewise), Some(false));
        // The selection takes in the character the cursor is on.
        assert_eq!(c.selected_columns(0), Some((0, 9)));
        keys(&mut c, "d");
        assert_eq!(c.text(), "hree", "up to and including the cursor");
        assert!(c.vim_visual().is_none(), "the operator ends it");

        // `V` takes whole lines however far along them it started.
        let mut c = composer("one\ntwo\nthree");
        keys(&mut c, "jlVj");
        assert_eq!(c.selected_columns(1), Some((0, 3)));
        assert_eq!(c.selected_columns(2), Some((0, 5)));
        assert_eq!(c.selected_columns(0), None);
        keys(&mut c, "d");
        assert_eq!(c.text(), "one");

        // A text object is a selection too, and `o` swaps which end moves.
        let mut c = composer("one two three");
        keys(&mut c, "wviw");
        assert_eq!(c.selected_columns(0), Some((4, 7)));
        keys(&mut c, "oh");
        assert_eq!(c.selected_columns(0), Some((3, 7)));
    }

    /// The selection is what the operator works on, and `c` leaves it ready to type.
    #[test]
    fn a_selection_can_be_changed_yanked_and_turned_around() {
        let mut c = composer("keep THIS one");
        keys(&mut c, "wve");
        assert_eq!(keys(&mut c, "c"), vec![Effect::EnterInsert]);
        assert_eq!(c.text(), "keep  one");
        typed(&mut c, "that");
        assert_eq!(c.text(), "keep that one");

        let mut c = composer("yank me");
        let effects = keys(&mut c, "v$y");
        assert_eq!(effects.last(), Some(&Effect::Yanked("yank me".into())));
        assert!(c.vim_visual().is_none());

        let mut c = composer("flip me");
        keys(&mut c, "ve~");
        assert_eq!(c.text(), "FLIP me");

        // Esc lets a selection go without touching the text.
        let mut c = composer("leave it");
        keys(&mut c, "vee");
        c.vim_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(c.vim_visual().is_none());
        assert_eq!(c.text(), "leave it");
    }

    /// `v` and `V` swap between them; the same one again is how Vim says it is done.
    #[test]
    fn the_selection_keys_change_and_end_the_selection() {
        let mut c = composer("one\ntwo");
        keys(&mut c, "v");
        assert_eq!(c.vim_visual().map(|v| v.linewise), Some(false));
        keys(&mut c, "V");
        assert_eq!(c.vim_visual().map(|v| v.linewise), Some(true));
        keys(&mut c, "V");
        assert!(c.vim_visual().is_none());
    }

    /// `J` is the next thread, so joining is `gJ`, and it joins the way Vim's `J` does:
    /// one space where the lines meet, whatever the indentation was.
    #[test]
    fn lines_are_joined_with_one_space() {
        let mut c = composer("one\n   two\nthree");
        c.vim_join(2);
        assert_eq!(c.text(), "one two\nthree");
        assert_eq!((c.row, c.col), (0, 3), "the cursor is where the join is");
        // A count joins that many lines; over a selection, all of them. Bare `J` opens
        // the next thread and never reaches the composer, but `3J` does.
        let mut c = composer("a\nb\nc\nd");
        c.vim_join(3);
        assert_eq!(c.text(), "a b c\nd");
        let mut c = composer("a\nb\nc\nd");
        keys(&mut c, "3J");
        assert_eq!(c.text(), "a b c\nd");
        keys(&mut c, ".");
        assert_eq!(c.text(), "a b c d");
        let mut c = composer("a\nb\nc\nd");
        keys(&mut c, "VjjJ");
        assert_eq!(c.text(), "a b c\nd");
        // Nothing under the last line to join to it.
        let mut c = composer("only");
        c.vim_join(2);
        assert_eq!(c.text(), "only");
    }

    /// The composer says when it is in the middle of something, and the app asks before
    /// taking a letter for itself: `3` then `s` is a count and a substitute, not a count
    /// and the sidebar.
    #[test]
    fn a_half_typed_command_says_so() {
        let mut c = composer("one two");
        assert!(!c.vim_busy());
        keys(&mut c, "3");
        assert!(c.vim_busy(), "a count is the start of a command");
        keys(&mut c, "d");
        assert!(c.vim_busy(), "and so is an operator with no motion yet");
        c.vim_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!c.vim_busy());
        keys(&mut c, "v");
        assert!(c.vim_busy(), "so is a selection being made");
    }
}

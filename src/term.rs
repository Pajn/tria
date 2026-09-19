//! The attached terminal: a vt100 screen fed by the server's output stream, and
//! the encoder that turns key events back into the bytes a pty expects.
//!
//! The server owns the pty. This side only parses what it sends and writes what
//! the user types, so there is no process handling here.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// How many lines of scrollback the parser keeps beyond the visible screen.
const SCROLLBACK: usize = 5_000;

/// Answers the capability queries a program sends to the terminal it runs in.
///
/// These matter for startup time: a program that asks what the terminal supports
/// waits for the reply, and a full-screen one will sit on a timeout of a second or
/// more before drawing anything. The server's pty passes the queries through, so
/// this side has to answer them the way a real emulator would.
#[derive(Default)]
struct Answers {
    replies: Vec<String>,
}

impl vt100::Callbacks for Answers {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let first = params.first().and_then(|p| p.first()).copied();
        let reply = match (i1, i2, c) {
            // Primary and secondary device attributes: a VT220 with color.
            (None, _, 'c') => Some("\x1b[?62;22c".to_string()),
            (Some(b'>'), _, 'c') => Some("\x1b[>1;10;0c".to_string()),
            // XTVERSION.
            (Some(b'>'), _, 'q') => {
                Some(format!("\x1bP>|tria({})\x1b\\", env!("CARGO_PKG_VERSION")))
            }
            // Device status: ready, and the cursor position.
            (None, _, 'n') if first == Some(5) => Some("\x1b[0n".to_string()),
            (None, _, 'n') if first == Some(6) => {
                let (row, col) = screen.cursor_position();
                Some(format!("\x1b[{};{}R", row + 1, col + 1))
            }
            // The text area size in characters.
            (None, _, 't') if first == Some(18) => {
                let (rows, cols) = screen.size();
                Some(format!("\x1b[8;{rows};{cols}t"))
            }
            // Key modifier options: none of the optional encodings are on.
            (Some(b'?'), _, 'm') => first.map(|mode| format!("\x1b[>{mode};0m")),
            // The kitty keyboard protocol, with no flags set.
            (Some(b'?'), _, 'u') => Some("\x1b[?0u".to_string()),
            // DECRQM: report the modes this screen actually tracks, and say the rest
            // are not recognized so the program picks its fallback without waiting.
            (Some(b'?'), Some(b'$'), 'p') => first.map(|mode| {
                let set = match mode {
                    1049 => screen.alternate_screen(),
                    2004 => screen.bracketed_paste(),
                    1 => screen.application_cursor(),
                    _ => return format!("\x1b[?{mode};0$y"),
                };
                format!("\x1b[?{mode};{}$y", if set { 1 } else { 2 })
            }),
            _ => None,
        };
        if let Some(reply) = reply {
            self.replies.push(reply);
        }
    }
}

/// What joins the parts of an emoji built out of several, and what vt100 ends a cell on.
const ZERO_WIDTH_JOINER: char = '\u{200D}';

/// One of the two letters a flag is spelled with, alone in a cell.
fn is_regional(text: &str) -> bool {
    let mut chars = text.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some('\u{1F1E6}'..='\u{1F1FF}'), None)
    )
}

pub struct Pane {
    pub thread_id: String,
    pub terminal_id: String,
    pub label: String,
    /// Set once the stream reports the shell exited, so the pane can say so.
    pub exited: Option<String>,
    /// A command being started in place of the shell: the pane shows a notice instead of
    /// the shell's prompt until the command is actually running.
    pub starting: Option<String>,
    parser: vt100::Parser<Answers>,
    size: (u16, u16),
    graphics: crate::kitty::Graphics,
    /// How many lines have scrolled off the top, which is what a placed image is
    /// anchored against.
    history: usize,
    /// Whether a full-screen program has the screen, tracked because its grid keeps no
    /// history of its own and images cannot be anchored across the swap.
    alternate: bool,
}

impl Pane {
    pub fn new(
        thread_id: String,
        terminal_id: String,
        label: String,
        cols: u16,
        rows: u16,
    ) -> Self {
        Self {
            thread_id,
            exited: None,
            starting: None,
            graphics: crate::kitty::Graphics::new(&terminal_id),
            terminal_id,
            label,
            parser: vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK, Answers::default()),
            size: (cols, rows),
            history: 0,
            alternate: false,
        }
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// What is at a cell, with whatever vt100 cut it away from put back, and how many
    /// columns those pieces were spread over.
    ///
    /// vt100 ends a cell at a zero-width joiner and starts the next one with what the
    /// joiner was joining to, and it gives each of the two letters a flag is spelled
    /// with a cell of its own. Drawn a column apart the pieces overlap, because the
    /// first of them is two columns wide and the second lands on its right half: that
    /// is why a trans flag came out as the symbol it is joined to, and a Swedish one as
    /// the letter E.
    pub fn cluster_at(&self, row: u16, col: u16) -> (String, u16) {
        let screen = self.screen();
        let Some(cell) = screen.cell(row, col) else {
            return (String::new(), 1);
        };
        let columns = |cell: &vt100::Cell| if cell.is_wide() { 2 } else { 1 };
        let mut text = cell.contents().to_string();
        let mut span = columns(cell);
        // A cluster can be cut more than once: a family is a person per cell.
        while text.ends_with(ZERO_WIDTH_JOINER) {
            let Some(next) = screen.cell(row, col + span) else {
                break;
            };
            let joined = next.contents();
            if joined.is_empty() {
                break;
            }
            text.push_str(joined);
            span += columns(next);
        }
        // A flag is two letters with no joiner between them, so it is paired by what
        // the letters are. Two of them make one flag and the next pair makes the next.
        if is_regional(&text)
            && let Some(next) = screen.cell(row, col + span)
            && is_regional(next.contents())
        {
            text.push_str(next.contents());
            span += columns(next);
        }
        (text, span)
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// Parse output, and hand back whatever the program asked the terminal to answer.
    ///
    /// Graphics commands never reach the parser: it has no hook for one, so they are
    /// taken out of the stream first and answered here.
    #[must_use]
    pub fn feed(&mut self, data: &str) -> Vec<String> {
        use crate::kitty::Piece;

        let mut replies = Vec::new();
        for piece in self.graphics.split(data) {
            match piece {
                Piece::Text(text) => {
                    self.parser.process(text.as_bytes());
                    self.follow();
                }
                Piece::Erase => self.graphics.clear(),
                Piece::Command(command) => {
                    let (row, column) = self.parser.screen().cursor_position();
                    let spot = crate::kitty::Spot {
                        line: self.history as i64 + i64::from(row),
                        column,
                    };
                    let outcome = self.graphics.take(command, spot, self.size);
                    if let Some(reply) = outcome.reply {
                        replies.push(reply);
                    }
                    // Displaying an image leaves the cursor past it, which at the foot of
                    // the screen is a scroll like any other.
                    if let Some(motion) = outcome.motion {
                        self.parser.process(motion.as_bytes());
                        self.follow();
                    }
                }
            }
        }
        // An image is anchored to a line of the history. Where there is no history to
        // anchor to — a full-screen program's grid keeps none, and a pane's own runs out
        // eventually — a picture lives until the next thing the program prints.
        if self.alternate != self.parser.screen().alternate_screen() {
            self.alternate = !self.alternate;
            self.graphics.clear();
        }
        if !self.alternate && self.history >= SCROLLBACK {
            self.graphics.keep_only_the_newest();
        }
        replies.extend(std::mem::take(&mut self.parser.callbacks_mut().replies));
        replies
    }

    /// Note how far the text has scrolled, so the images over it move with it.
    fn follow(&mut self) {
        if self.parser.screen().alternate_screen() {
            return;
        }
        let screen = self.parser.screen_mut();
        // The parser will say how long its history is, if asked the only way it can be:
        // scrolling further back than there is and seeing where that landed.
        let looking = screen.scrollback();
        screen.set_scrollback(usize::MAX);
        self.history = screen.scrollback();
        screen.set_scrollback(looking);
    }

    /// The images the program in the pane has placed, and how far the text under them
    /// has scrolled.
    pub fn placements(&self) -> &[crate::kitty::Placement] {
        self.graphics.placements()
    }

    /// The line of the pane's history showing at the top of the screen.
    pub fn top_line(&self) -> i64 {
        self.history as i64 - self.scrollback() as i64
    }

    /// Replace the screen with a replayed scrollback, as after attach or restart.
    pub fn reset(&mut self, history: &str) {
        let (cols, rows) = self.size;
        self.parser = vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK, Answers::default());
        self.exited = None;
        self.graphics.clear();
        self.history = 0;
        self.alternate = false;
        let _ = self.feed(history);
    }

    /// Returns true when the size changed and the server needs telling.
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        if self.size == (cols, rows) || cols == 0 || rows == 0 {
            return false;
        }
        self.size = (cols, rows);
        self.parser.screen_mut().set_size(rows, cols);
        // The text the images were drawn over has moved around them.
        self.graphics.clear();
        self.follow();
        true
    }

    /// Move through the scrollback; the pty never sees this.
    pub fn scroll(&mut self, delta: isize) {
        let current = self.parser.screen().scrollback() as isize;
        let next = (current + delta).max(0) as usize;
        self.parser.screen_mut().set_scrollback(next);
    }

    pub fn scrollback(&self) -> usize {
        self.parser.screen().scrollback()
    }

    /// A full-screen program has taken over, so whatever the shell printed is hidden.
    pub fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    /// Whether the program running in the pane wants the mouse, and in which encoding.
    pub fn mouse_protocol(&self) -> (MouseProtocolMode, MouseProtocolEncoding) {
        let screen = self.parser.screen();
        (
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
        )
    }
}

/// Encode a key the way a terminal emulator would, for `terminal.write`.
///
/// Returns `None` for keys with no byte sequence. Cursor and function keys follow
/// the application mode the screen is in, which is what full-screen programs expect.
pub fn encode_key(key: &KeyEvent, app_cursor: bool) -> Option<String> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // xterm's modifier parameter: 1 + shift + 2*alt + 4*ctrl.
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);

    let base = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let byte = match c {
                    ' ' | '@' => Some(0u8),
                    '[' => Some(27),
                    '\\' => Some(28),
                    ']' => Some(29),
                    '^' => Some(30),
                    '_' | '/' => Some(31),
                    '?' => Some(127),
                    // Terminals without the kitty protocol report Ctrl-\ through Ctrl-_
                    // as Ctrl-4 through Ctrl-7; both spellings reach the same bytes.
                    '4'..='7' => Some(0x1c + (c as u8 - b'4')),
                    c if c.is_ascii_alphabetic() => Some(c.to_ascii_lowercase() as u8 - b'a' + 1),
                    _ => None,
                };
                match byte {
                    Some(byte) => (byte as char).to_string(),
                    None => c.to_string(),
                }
            } else {
                c.to_string()
            }
        }
        KeyCode::Enter => "\r".into(),
        KeyCode::Tab => "\t".into(),
        KeyCode::BackTab => "\x1b[Z".into(),
        KeyCode::Backspace => "\x7f".into(),
        KeyCode::Esc => "\x1b".into(),
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Right
        | KeyCode::Left
        | KeyCode::Home
        | KeyCode::End => {
            let final_byte = match key.code {
                KeyCode::Up => 'A',
                KeyCode::Down => 'B',
                KeyCode::Right => 'C',
                KeyCode::Left => 'D',
                KeyCode::Home => 'H',
                _ => 'F',
            };
            if modifier > 1 {
                format!("\x1b[1;{modifier}{final_byte}")
            } else if app_cursor {
                format!("\x1bO{final_byte}")
            } else {
                format!("\x1b[{final_byte}")
            }
        }
        KeyCode::Insert => tilde(2, modifier),
        KeyCode::Delete => tilde(3, modifier),
        KeyCode::PageUp => tilde(5, modifier),
        KeyCode::PageDown => tilde(6, modifier),
        KeyCode::F(n @ 1..=4) => {
            let final_byte = (b'P' + n - 1) as char;
            if modifier > 1 {
                format!("\x1b[1;{modifier}{final_byte}")
            } else {
                format!("\x1bO{final_byte}")
            }
        }
        KeyCode::F(n @ 5..=12) => {
            let code = match n {
                5 => 15,
                6..=10 => 17 + u32::from(n) - 6,
                11 => 23,
                _ => 24,
            };
            tilde(code, modifier)
        }
        _ => return None,
    };

    // Alt is the escape prefix for anything that is not already an escape sequence.
    if alt && !base.starts_with('\x1b') {
        return Some(format!("\x1b{base}"));
    }
    Some(base)
}

/// Encode a mouse event for the program in the pane, at cell `col`/`row` within it.
///
/// Returns `None` when the program has not asked for the mouse, or has not asked for
/// this kind of event, in which case the wheel is free to drive our own scrollback.
pub fn encode_mouse(
    event: &MouseEvent,
    col: u16,
    row: u16,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
) -> Option<String> {
    if mode == MouseProtocolMode::None {
        return None;
    }
    let button = |button: &MouseButton| match button {
        MouseButton::Left => 0u8,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    // Wheel events are buttons 64 and up; motion adds 32 to whichever button is held.
    let (mut code, release) = match event.kind {
        MouseEventKind::Down(ref b) => (button(b), false),
        MouseEventKind::Up(ref b) if mode != MouseProtocolMode::Press => (button(b), true),
        MouseEventKind::Drag(ref b)
            if matches!(
                mode,
                MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
            ) =>
        {
            (button(b) + 32, false)
        }
        MouseEventKind::Moved if mode == MouseProtocolMode::AnyMotion => (3 + 32, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
        _ => return None,
    };
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }

    match encoding {
        MouseProtocolEncoding::Sgr => Some(format!(
            "\x1b[<{code};{};{}{}",
            col + 1,
            row + 1,
            if release { 'm' } else { 'M' }
        )),
        // The original encoding sends one byte per field, offset by 32, and reports
        // every release as button 3. It cannot address past column 223.
        _ => {
            if col >= 223 || row >= 223 {
                return None;
            }
            let code = if release { 3 + (code & !3) } else { code };
            Some(format!(
                "\x1b[M{}{}{}",
                (32 + code) as char,
                (33 + col as u8) as char,
                (33 + row as u8) as char
            ))
        }
    }
}

fn tilde(code: u32, modifier: u8) -> String {
    if modifier > 1 {
        format!("\x1b[{code};{modifier}~")
    } else {
        format!("\x1b[{code}~")
    }
}

/// Encode pasted text for `terminal.write`.
///
/// Newlines become carriage returns, which is what Return sends and so what a shell
/// reads as the end of a line. The other control bytes are dropped: nothing in a paste
/// is meant as an escape sequence, and one that ended the paste early would hand the
/// rest of the text to the program as keys nobody typed.
///
/// A program that has turned bracketed paste on gets the text between the markers, so
/// that it can tell a paste from typing and hold a multi-line one back until it is read.
pub fn encode_paste(text: &str, bracketed: bool) -> String {
    let mut out = String::with_capacity(text.len() + 12);
    if bracketed {
        out.push_str("\x1b[200~");
    }
    let mut after_cr = false;
    for ch in text.chars() {
        match ch {
            '\r' => out.push('\r'),
            // A CRLF is one line ending, not two.
            '\n' if !after_cr => out.push('\r'),
            '\n' => {}
            '\t' => out.push('\t'),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {}
            c => out.push(c),
        }
        after_cr = ch == '\r';
    }
    if bracketed {
        out.push_str("\x1b[201~");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// vt100 cuts an emoji built out of several at every joiner, and gives each letter
    /// of a flag its own cell. Drawn a cell apart the pieces land on top of each other
    /// — the trans flag came out as the symbol alone — so they are put back together.
    #[test]
    fn an_emoji_cut_across_cells_is_put_back_together() {
        let whole = |text: &str| {
            let mut pane = Pane::new("t".into(), "x".into(), "s".into(), 20, 3);
            let _ = pane.feed(text);
            pane.cluster_at(0, 0)
        };

        let trans = "\u{1F3F3}\u{FE0F}\u{200D}\u{26A7}\u{FE0F}";
        assert_eq!(whole(trans), (trans.to_string(), 2));
        let rainbow = "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}";
        assert_eq!(whole(rainbow), (rainbow.to_string(), 3));
        // Cut twice, and a person is two columns apiece.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(whole(family), (family.to_string(), 6));
        assert_eq!(
            whole("\u{1F1F8}\u{1F1EA}"),
            ("\u{1F1F8}\u{1F1EA}".into(), 2)
        );

        // Two flags are two flags, rather than one four-letter run.
        let mut pane = Pane::new("t".into(), "x".into(), "s".into(), 20, 3);
        let _ = pane.feed("\u{1F1F8}\u{1F1EA}\u{1F1F3}\u{1F1F4}");
        assert_eq!(pane.cluster_at(0, 0), ("\u{1F1F8}\u{1F1EA}".into(), 2));
        assert_eq!(pane.cluster_at(0, 2), ("\u{1F1F3}\u{1F1F4}".into(), 2));

        // What vt100 already keeps whole is left alone.
        assert_eq!(whole("\u{1F680}"), ("\u{1F680}".into(), 2));
        assert_eq!(whole("1\u{FE0F}\u{20E3}"), ("1\u{FE0F}\u{20E3}".into(), 1));
        assert_eq!(whole("e\u{301}"), ("e\u{301}".into(), 1));
        assert_eq!(whole("a"), ("a".into(), 1));
    }

    /// A program that has not asked for bracketed paste gets the text plain, because
    /// the markers would be typed into it as characters.
    #[test]
    fn a_paste_is_bracketed_only_for_a_program_that_asked() {
        assert_eq!(encode_paste("ls -l", false), "ls -l");
        assert_eq!(encode_paste("ls -l", true), "\x1b[200~ls -l\x1b[201~");
    }

    #[test]
    fn a_pasted_line_ending_is_one_carriage_return() {
        assert_eq!(encode_paste("one\ntwo", false), "one\rtwo");
        assert_eq!(encode_paste("one\r\ntwo", false), "one\rtwo");
        assert_eq!(encode_paste("one\rtwo", false), "one\rtwo");
    }

    /// Control bytes in the text are not keys anybody pressed, and an end marker hidden
    /// in a paste would hand the rest of it to the program as typing.
    #[test]
    fn a_paste_carries_no_escape_sequences() {
        assert_eq!(
            encode_paste("safe\x1b[201~rm -rf /", true),
            "\x1b[200~safe[201~rm -rf /\x1b[201~"
        );
        assert_eq!(encode_paste("a\x07b\x7f", false), "ab");
        assert_eq!(encode_paste("a\tb", false), "a\tb", "a tab is a tab");
    }

    #[test]
    fn control_letters_become_control_bytes() {
        let encoded = encode_key(&key(KeyCode::Char('c'), KeyModifiers::CONTROL), false);
        assert_eq!(encoded.as_deref(), Some("\u{3}"));
        let encoded = encode_key(&key(KeyCode::Char('A'), KeyModifiers::CONTROL), false);
        assert_eq!(encoded.as_deref(), Some("\u{1}"));
    }

    #[test]
    fn legacy_control_spellings_reach_the_same_bytes() {
        let bracket = encode_key(&key(KeyCode::Char(']'), KeyModifiers::CONTROL), false);
        let legacy = encode_key(&key(KeyCode::Char('5'), KeyModifiers::CONTROL), false);
        assert_eq!(bracket.as_deref(), Some("\u{1d}"));
        assert_eq!(legacy, bracket);
    }

    #[test]
    fn alt_prefixes_escape() {
        let encoded = encode_key(&key(KeyCode::Char('b'), KeyModifiers::ALT), false);
        assert_eq!(encoded.as_deref(), Some("\u{1b}b"));
    }

    #[test]
    fn arrows_follow_application_mode() {
        let plain = encode_key(&key(KeyCode::Up, KeyModifiers::NONE), false);
        assert_eq!(plain.as_deref(), Some("\x1b[A"));
        let app = encode_key(&key(KeyCode::Up, KeyModifiers::NONE), true);
        assert_eq!(app.as_deref(), Some("\x1bOA"));
        let shifted = encode_key(&key(KeyCode::Up, KeyModifiers::SHIFT), true);
        assert_eq!(shifted.as_deref(), Some("\x1b[1;2A"));
    }

    #[test]
    fn function_keys_use_their_two_families() {
        assert_eq!(
            encode_key(&key(KeyCode::F(1), KeyModifiers::NONE), false).as_deref(),
            Some("\x1bOP")
        );
        assert_eq!(
            encode_key(&key(KeyCode::F(5), KeyModifiers::NONE), false).as_deref(),
            Some("\x1b[15~")
        );
        assert_eq!(
            encode_key(&key(KeyCode::F(12), KeyModifiers::NONE), false).as_deref(),
            Some("\x1b[24~")
        );
    }

    #[test]
    fn capability_queries_are_answered() {
        let mut pane = Pane::new("t".into(), "term-1".into(), "Terminal".into(), 80, 24);
        // Primary device attributes, the text area size, and a mode query.
        let replies = pane.feed("\x1b[c\x1b[18t\x1b[?2026$p");
        assert_eq!(
            replies,
            vec!["\x1b[?62;22c", "\x1b[8;24;80t", "\x1b[?2026;0$y"]
        );
        // Plain output asks nothing.
        assert!(pane.feed("hello").is_empty());
    }

    fn mouse(kind: MouseEventKind, modifiers: KeyModifiers) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers,
        }
    }

    #[test]
    fn the_mouse_is_left_alone_until_a_program_asks_for_it() {
        let event = mouse(MouseEventKind::ScrollUp, KeyModifiers::NONE);
        assert_eq!(
            encode_mouse(
                &event,
                3,
                4,
                MouseProtocolMode::None,
                MouseProtocolEncoding::Sgr
            ),
            None
        );
        assert_eq!(
            encode_mouse(
                &event,
                3,
                4,
                MouseProtocolMode::Press,
                MouseProtocolEncoding::Sgr
            )
            .as_deref(),
            Some("\x1b[<64;4;5M")
        );
    }

    #[test]
    fn presses_releases_and_drags_follow_the_mode() {
        let down = mouse(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE);
        let up = mouse(MouseEventKind::Up(MouseButton::Left), KeyModifiers::NONE);
        let drag = mouse(MouseEventKind::Drag(MouseButton::Left), KeyModifiers::NONE);
        let sgr = MouseProtocolEncoding::Sgr;
        // X10 mode reports presses only.
        assert!(encode_mouse(&up, 0, 0, MouseProtocolMode::Press, sgr).is_none());
        assert!(encode_mouse(&drag, 0, 0, MouseProtocolMode::PressRelease, sgr).is_none());
        assert_eq!(
            encode_mouse(&up, 0, 0, MouseProtocolMode::PressRelease, sgr).as_deref(),
            Some("\x1b[<0;1;1m")
        );
        assert_eq!(
            encode_mouse(&drag, 1, 1, MouseProtocolMode::ButtonMotion, sgr).as_deref(),
            Some("\x1b[<32;2;2M")
        );
        // The original encoding offsets every field by 32, releases as button 3.
        assert_eq!(
            encode_mouse(
                &down,
                0,
                0,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Default
            )
            .as_deref(),
            Some("\x1b[M\x20\x21\x21")
        );
        assert_eq!(
            encode_mouse(
                &up,
                0,
                0,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Default
            )
            .as_deref(),
            Some("\x1b[M\x23\x21\x21")
        );
    }

    fn showing(width: u16, height: u16) -> String {
        format!(
            "\x1b_Ga=T,f=100,c={width},r={height},C=1;{}\x1b\\",
            crate::picture::test_png(40, 40)
        )
    }

    /// An image is drawn over cells that scroll, and it has to go with them.
    #[test]
    fn a_picture_follows_the_text_it_was_drawn_over() {
        crate::picture::draw_in_halfblocks();
        let mut pane = Pane::new("t".into(), "term-1".into(), "Terminal".into(), 20, 6);
        // Fill the screen, so that anything more scrolls it.
        let _ = pane.feed("one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix");
        let _ = pane.feed(&format!("\r\n{}", showing(4, 2)));
        let [placement] = pane.placements() else {
            panic!("the picture is on the screen");
        };
        let line = placement.line;
        assert_eq!(line - pane.top_line(), 5, "on the row the cursor was on");

        let _ = pane.feed("\r\nseven\r\neight");
        let [placement] = pane.placements() else {
            panic!("the picture is still on the screen");
        };
        assert_eq!(
            placement.line, line,
            "the picture is on the line it was put on"
        );
        assert_eq!(
            placement.line - pane.top_line(),
            3,
            "which has moved up the screen"
        );
    }

    #[test]
    fn a_picture_goes_with_the_screen_it_was_drawn_over() {
        crate::picture::draw_in_halfblocks();
        let mut pane = Pane::new("t".into(), "term-1".into(), "Terminal".into(), 20, 6);
        let _ = pane.feed(&showing(4, 2));
        assert_eq!(pane.placements().len(), 1);
        let _ = pane.feed("\x1b[2J");
        assert!(pane.placements().is_empty(), "the screen was wiped");

        // And a full-screen program takes the screen away entirely.
        let _ = pane.feed(&showing(4, 2));
        assert_eq!(pane.placements().len(), 1);
        let _ = pane.feed("\x1b[?1049h");
        assert!(
            pane.placements().is_empty(),
            "something else has the screen"
        );
    }

    #[test]
    fn output_lands_on_the_screen() {
        let mut pane = Pane::new("t".into(), "term-1".into(), "Terminal".into(), 20, 4);
        let _ = pane.feed("hello\r\nworld");
        assert_eq!(pane.screen().contents().trim_end(), "hello\nworld");
        assert!(pane.resize(30, 6));
        assert!(!pane.resize(30, 6));
    }
}

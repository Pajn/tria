//! The attached terminal: a vt100 screen fed by the server's output stream, and
//! the encoder that turns key events back into the bytes a pty expects.
//!
//! The server owns the pty. This side only parses what it sends and writes what
//! the user types, so there is no process handling here.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// How many lines of scrollback the parser keeps beyond the visible screen.
const SCROLLBACK: usize = 5_000;

pub struct Pane {
    pub thread_id: String,
    pub terminal_id: String,
    pub label: String,
    /// Set once the stream reports the shell exited, so the pane can say so.
    pub exited: Option<String>,
    /// A command being started in place of the shell: the pane shows a notice instead of
    /// the shell's prompt until the command is actually running.
    pub starting: Option<String>,
    parser: vt100::Parser,
    size: (u16, u16),
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
            terminal_id,
            label,
            exited: None,
            starting: None,
            parser: vt100::Parser::new(rows, cols, SCROLLBACK),
            size: (cols, rows),
        }
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    pub fn feed(&mut self, data: &str) {
        self.parser.process(data.as_bytes());
    }

    /// Replace the screen with a replayed scrollback, as after attach or restart.
    pub fn reset(&mut self, history: &str) {
        let (cols, rows) = self.size;
        self.parser = vt100::Parser::new(rows, cols, SCROLLBACK);
        self.exited = None;
        self.feed(history);
    }

    /// Returns true when the size changed and the server needs telling.
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        if self.size == (cols, rows) || cols == 0 || rows == 0 {
            return false;
        }
        self.size = (cols, rows);
        self.parser.screen_mut().set_size(rows, cols);
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

fn tilde(code: u32, modifier: u8) -> String {
    if modifier > 1 {
        format!("\x1b[{code};{modifier}~")
    } else {
        format!("\x1b[{code}~")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
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
    fn output_lands_on_the_screen() {
        let mut pane = Pane::new("t".into(), "term-1".into(), "Terminal".into(), 20, 4);
        pane.feed("hello\r\nworld");
        assert_eq!(pane.screen().contents().trim_end(), "hello\nworld");
        assert!(pane.resize(30, 6));
        assert!(!pane.resize(30, 6));
    }
}

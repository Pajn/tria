//! Telling somebody who is not looking that a thread wants them.
//!
//! The notification is the terminal's to make, not tria's: OSC 9 is the sequence a
//! terminal turns into whatever this desktop calls a notification. That keeps this to
//! one write, with nothing to install and nothing to know about the machine — and it
//! works the same against a server on another one, since what is being announced is
//! happening here, on the screen.

use std::io::Write;

/// When a thread that has stopped working is worth saying so out loud.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum When {
    /// Only while the terminal does not have the focus, which is the whole point of
    /// them: a notification for something already on the screen in front of you is an
    /// interruption to tell you what you are looking at.
    #[default]
    Unfocused,
    Always,
    Never,
}

impl When {
    /// The setting as written, and the reason one was not taken. An unknown value is
    /// refused rather than guessed at: the three differ by name alone, and a typo would
    /// otherwise be notifications quietly behaving as some other setting.
    pub fn parse(value: Option<&str>) -> (Self, Option<String>) {
        match value.map(str::trim) {
            None | Some("") | Some("unfocused") => (Self::Unfocused, None),
            Some("always") => (Self::Always, None),
            Some("never" | "off" | "none") => (Self::Never, None),
            Some(other) => (
                Self::default(),
                Some(format!(
                    "notify {other:?} is none of unfocused, always or never"
                )),
            ),
        }
    }
}

/// Whether the terminal has the focus, as far as it has said.
///
/// Terminals report this when it changes and not otherwise, and a good many do not
/// report it at all — tmux does not pass it through without `focus-events on`. So
/// "has not said" is a state of its own rather than a guess at one of the other two,
/// and it is the one that errs towards speaking up: a notification nobody needed is a
/// thing you can see and turn off, and one that never came is the feature not working.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Focus {
    #[default]
    Unheard,
    Focused,
    Unfocused,
}

impl When {
    pub fn wants(self, focus: Focus) -> bool {
        match self {
            Self::Never => false,
            Self::Always => true,
            Self::Unfocused => focus != Focus::Focused,
        }
    }
}

/// Hand `text` to the terminal as a notification.
pub fn send(text: &str) {
    // A control character in here would end the sequence early and put the rest of the
    // message on the screen, so the message is text or it is nothing. Each becomes a
    // space rather than nothing, so that a title written over two lines does not come
    // back as one word, and the runs that makes are collapsed.
    let text: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if recorded(&text) {
        return;
    }
    let mut out = std::io::stdout();
    let _ = out.write_all(sequence(&text, std::env::var_os("TMUX").is_some()).as_bytes());
    let _ = out.flush();
}

/// The bytes that say it. OSC 9 is the notification; inside tmux it travels through the
/// passthrough, because tmux answers a sequence it knows and swallows one it does not.
fn sequence(text: &str, tmux: bool) -> String {
    let osc = format!("\x1b]9;{text}\x07");
    match tmux {
        // Every escape inside a passthrough is doubled. The pane also has to be allowed
        // to pass things through at all, which is `allow-passthrough`.
        true => format!("\x1bPtmux;{}\x1b\\", osc.replace('\x1b', "\x1b\x1b")),
        false => osc,
    }
}

#[cfg(test)]
thread_local! {
    /// What was sent, for tests, which have no terminal to send it to and no business
    /// writing escape sequences over the test runner's output.
    static SENT: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn recorded(text: &str) -> bool {
    SENT.with(|sent| sent.borrow_mut().push(text.to_string()));
    true
}

#[cfg(not(test))]
fn recorded(_text: &str) -> bool {
    false
}

/// What has been sent since this was last asked, for tests.
#[cfg(test)]
pub fn sent() -> Vec<String> {
    SENT.with(|sent| std::mem::take(&mut *sent.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default is the one that needs the terminal's help, and terminals vary in
    /// whether they give it. Where one says nothing, the choice is between a
    /// notification nobody needed and no notification at all, and only one of those is
    /// something somebody can notice and put right.
    #[test]
    fn a_terminal_that_never_mentions_focus_still_gets_told() {
        assert!(When::Unfocused.wants(Focus::Unfocused));
        assert!(When::Unfocused.wants(Focus::Unheard));
        assert!(!When::Unfocused.wants(Focus::Focused));

        for focus in [Focus::Focused, Focus::Unfocused, Focus::Unheard] {
            assert!(When::Always.wants(focus));
            assert!(!When::Never.wants(focus));
        }
    }

    /// tmux keeps to itself a sequence it does not know, so this one goes round it.
    #[test]
    fn inside_tmux_the_sequence_travels_through_the_passthrough() {
        assert_eq!(sequence("done", false), "\x1b]9;done\x07");
        assert_eq!(
            sequence("done", true),
            "\x1bPtmux;\x1b\x1b]9;done\x07\x1b\\",
            "the escapes inside are doubled"
        );
    }

    /// An escape in the text would end the sequence early and spill the rest of it onto
    /// the screen, and a thread can be titled anything at all.
    #[test]
    fn a_title_cannot_close_the_sequence_it_travels_in() {
        send("a \x1b]9;thread\x07 title\nover two lines");
        assert_eq!(sent(), vec!["a ]9;thread title over two lines"]);
    }

    #[test]
    fn the_setting_is_taken_by_name_or_refused_out_loud() {
        assert_eq!(When::parse(None), (When::Unfocused, None));
        assert_eq!(When::parse(Some("always")), (When::Always, None));
        assert_eq!(When::parse(Some("never")), (When::Never, None));
        assert_eq!(When::parse(Some(" off ")), (When::Never, None));
        let (when, refused) = When::parse(Some("yes"));
        assert_eq!(when, When::Unfocused, "the default stands");
        assert!(refused.is_some_and(|said| said.contains("yes")));
    }
}

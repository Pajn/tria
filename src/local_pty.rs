//! Client-owned terminals for program bindings. No interactive shell startup is involved.
use std::io::{Read, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc;

use crate::{app::AppEvent, term::Pane};

pub type Key = (String, String);

pub enum Output {
    Data(String),
    Exited(Result<(), String>),
}

pub struct Event {
    pub key: Key,
    pub generation: uuid::Uuid,
    pub output: Output,
}

pub struct Session {
    pub generation: uuid::Uuid,
    pub screen: Option<Pane>,
    pub summary: crate::model::TerminalSummary,
    finished: Arc<AtomicBool>,
    pid: Option<u32>,
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

impl Session {
    pub fn spawn(
        key: Key,
        command: &str,
        cwd: &str,
        cols: u16,
        rows: u16,
        events: mpsc::UnboundedSender<AppEvent>,
    ) -> Result<Self> {
        let pair = native_pty_system().openpty(size(cols, rows))?;
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        // A plain executable needs no shell at all. Configured command strings retain
        // shell syntax through a non-interactive POSIX shell, with no login profiles.
        let mut cmd = if command
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-/.".contains(c))
            && !command.contains(' ')
        {
            CommandBuilder::new(command)
        } else {
            let mut cmd = CommandBuilder::new("/bin/sh");
            cmd.args(["-c", command]);
            cmd
        };
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        let mut child = pair.slave.spawn_command(cmd).context("starting program")?;
        let killer = child.clone_killer();
        let pid = child.process_id();
        let finished = Arc::new(AtomicBool::new(false));
        let done = finished.clone();
        let summary = crate::model::TerminalSummary {
            thread_id: key.0.clone(),
            terminal_id: key.1.clone(),
            cwd: cwd.into(),
            worktree_path: None,
            status: "running".into(),
            pid,
            exit_code: None,
            exit_signal: None,
            has_running_subprocess: true,
            label: command.into(),
            updated_at: String::new(),
        };
        drop(pair.slave);
        let generation = uuid::Uuid::new_v4();
        std::thread::spawn(move || {
            let mut bytes = [0; 8192];
            let mut pending = Vec::new();
            loop {
                let count = match reader.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    // Unix ptys can report EIO instead of EOF after the slave closes.
                    Err(_) => break,
                };
                pending.extend_from_slice(&bytes[..count]);
                let data = decode(&mut pending, false);
                if !data.is_empty()
                    && events
                        .send(AppEvent::LocalTerminal(Event {
                            key: key.clone(),
                            generation,
                            output: Output::Data(data),
                        }))
                        .is_err()
                {
                    let _ = child.kill();
                    break;
                }
            }
            let data = decode(&mut pending, true);
            if !data.is_empty() {
                let _ = events.send(AppEvent::LocalTerminal(Event {
                    key: key.clone(),
                    generation,
                    output: Output::Data(data),
                }));
            }
            // Send exit after the reader drained the last output, so a short command's
            // output cannot land in the next popup opened on the same binding.
            let result = child.wait().map(|_| ()).map_err(|err| err.to_string());
            done.store(true, Ordering::Release);
            let _ = events.send(AppEvent::LocalTerminal(Event {
                key,
                generation,
                output: Output::Exited(result),
            }));
        });
        Ok(Self {
            generation,
            screen: None,
            summary,
            finished,
            pid,
            master: pair.master,
            writer: Mutex::new(writer),
            killer,
        })
    }

    pub fn write(&self, data: &str) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(data.as_bytes())?;
        writer.flush()?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master.resize(size(cols, rows))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::Acquire) {
            let _ = self.killer.kill();
            // The PTY child leads its own session/process group. Stop descendants too
            // when tria closes a popup or exits, including a shell pipeline.
            #[cfg(unix)]
            if let Some(pid) = self.pid {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        }
    }
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        cols,
        rows,
        pixel_width: 0,
        pixel_height: 0,
    }
}

// PTY reads may split a UTF-8 character. Retain only an incomplete suffix; invalid
// bytes use the replacement character just like server terminal text does.
fn decode(pending: &mut Vec<u8>, eof: bool) -> String {
    let mut text = String::new();
    let mut consumed = 0;
    while consumed < pending.len() {
        match std::str::from_utf8(&pending[consumed..]) {
            Ok(valid) => {
                text.push_str(valid);
                consumed = pending.len();
            }
            Err(err) => {
                let valid_end = consumed + err.valid_up_to();
                text.push_str(std::str::from_utf8(&pending[consumed..valid_end]).unwrap());
                consumed = valid_end;
                match err.error_len() {
                    Some(len) => {
                        text.push('\u{fffd}');
                        consumed += len;
                    }
                    None if eof => {
                        text.push('\u{fffd}');
                        consumed = pending.len();
                    }
                    None => break,
                }
            }
        }
    }
    pending.drain(..consumed);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn terminal_has_a_tty_directory_size_and_input() {
        let (events, mut received) = mpsc::unbounded_channel();
        let cwd = std::env::temp_dir();
        let session = Session::spawn(
            ("thread".into(), "program".into()),
            "stty -echo; test -t 0 && printf 'TTY\\n'; pwd; stty size; read line; printf 'INPUT:%s\\n' \"$line\"",
            cwd.to_str().unwrap(), 93, 17, events,
        ).unwrap();
        let mut text = String::new();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !text.contains("17 93") {
                match received.recv().await.unwrap() {
                    AppEvent::LocalTerminal(Event {
                        output: Output::Data(data),
                        ..
                    }) => text.push_str(&data),
                    _ => panic!("program exited before reading input"),
                }
            }
            session.resize(81, 22).unwrap();
            assert_eq!(session.master.get_size().unwrap().cols, 81);
            session.write("hello\r").unwrap();
            loop {
                match received.recv().await.unwrap() {
                    AppEvent::LocalTerminal(Event {
                        output: Output::Data(data),
                        ..
                    }) => text.push_str(&data),
                    AppEvent::LocalTerminal(Event {
                        output: Output::Exited(result),
                        ..
                    }) => {
                        result.unwrap();
                        break;
                    }
                    _ => panic!("unexpected event"),
                }
            }
        })
        .await
        .unwrap();
        assert!(text.contains("TTY"), "{text}");
        assert!(
            text.contains(cwd.to_str().unwrap().trim_end_matches('/')),
            "{text}"
        );
        assert!(text.contains("INPUT:hello"), "{text}");
    }

    #[tokio::test]
    async fn dropping_a_session_stops_its_process() {
        let (events, mut received) = mpsc::unbounded_channel();
        let session = Session::spawn(
            ("thread".into(), "program".into()),
            "sleep 60",
            std::env::temp_dir().to_str().unwrap(),
            80,
            24,
            events,
        )
        .unwrap();
        drop(session);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let AppEvent::LocalTerminal(Event {
                    output: Output::Exited(_),
                    ..
                }) = received.recv().await.unwrap()
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn utf8_reads_preserve_split_characters_and_replace_invalid_bytes() {
        let mut pending = vec![b'a', 0xf0, 0x9f];
        assert_eq!(decode(&mut pending, false), "a");
        pending.extend_from_slice(&[0x98, 0x80, 0xff, b'b', 0xe2]);
        assert_eq!(decode(&mut pending, false), "😀�b");
        assert_eq!(decode(&mut pending, true), "�");
        assert!(pending.is_empty());
    }
}

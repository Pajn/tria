//! Adding a project: choose its machine, browse there, then connect and open a draft.
use crate::{composer::Composer, ssh};
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph},
};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Local,
    Ssh(ssh::Target),
}
impl Target {
    pub fn label(&self) -> String {
        match self {
            Self::Local => "Local".into(),
            Self::Ssh(target) => target.label(),
        }
    }
}

pub struct Connection {
    pub origin: String,
    pub token: String,
    pub target: Option<Target>,
    pub local_disk: bool,
    pub _tunnel: Option<ssh::Tunnel>,
}

impl Connection {
    pub async fn prepare(target: Target) -> Result<Self> {
        let (origin, token, tunnel) = match &target {
            Target::Ssh(ssh) => {
                let (origin, token, tunnel) = ssh.connect().await?;
                (origin, token, Some(tunnel))
            }
            Target::Local => {
                let cfg = crate::config::Config::load()?;
                let known = cfg
                    .url
                    .clone()
                    .filter(|url| crate::server::is_local(url))
                    .or_else(|| crate::discovery::local_origin().ok());
                let (origin, _) = crate::server::ensure_quiet(known, &cfg.server_command()).await?;
                let old = cfg
                    .token
                    .filter(|_| cfg.url.as_deref().is_some_and(|url| url == origin));
                let token = if let Some(token) = old
                    && crate::auth::websocket_ticket(&origin, &token).await.is_ok()
                {
                    token
                } else {
                    let output = tokio::process::Command::new("sh").args(["-lc", "if command -v t3 >/dev/null 2>&1; then t3 auth session issue --label tria --ttl 30d --token-only; else npx --yes t3@latest auth session issue --label tria --ttl 30d --token-only; fi"]).kill_on_drop(true).output().await.context("issuing local T3 session")?;
                    anyhow::ensure!(
                        output.status.success(),
                        "Could not authorize local T3: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                    let token = String::from_utf8(output.stdout)?.trim().to_string();
                    anyhow::ensure!(!token.is_empty(), "No local T3 token issued");
                    crate::auth::websocket_ticket(&origin, &token).await?;
                    token
                };
                (origin, token, None)
            }
        };
        Ok(Self {
            local_disk: target == Target::Local,
            origin,
            token,
            target: Some(target),
            _tunnel: tunnel,
        })
    }

    pub fn remember(&self) -> Result<()> {
        let mut cfg = crate::config::Config::load()?;
        match self.target.as_ref().context("Connection has no profile")? {
            Target::Local => {
                cfg.active_ssh = None;
                cfg.url = Some(self.origin.clone());
                cfg.token = Some(self.token.clone());
            }
            Target::Ssh(ssh) => {
                cfg.active_ssh = Some(ssh.clone());
                if !cfg.ssh_connections.contains(ssh) {
                    cfg.ssh_connections.push(ssh.clone());
                }
            }
        }
        cfg.save()
    }
}

#[derive(Debug)]
pub struct Directory {
    pub path: String,
    pub entries: Vec<String>,
}
impl Directory {
    pub async fn read(target: &Target, path: &str) -> Result<Self> {
        match target {
            Target::Local => {
                let path = path.to_string();
                tokio::task::spawn_blocking(move || {
                    let root = crate::workspace::root(Some(&path))?;
                    let root = std::fs::canonicalize(root)?;
                    let mut entries = vec!["..".to_string()];
                    for entry in std::fs::read_dir(&root)? {
                        let entry = entry?;
                        if entry.path().is_dir()
                            && let Some(name) = entry.file_name().to_str()
                        {
                            entries.push(name.to_string());
                        }
                    }
                    entries[1..].sort();
                    Ok(Self {
                        path: root
                            .to_str()
                            .context("Directory path must be UTF-8")?
                            .to_string(),
                        entries,
                    })
                })
                .await?
            }
            Target::Ssh(ssh) => {
                let script = directory_script(path);
                let bytes = ssh.script(&script, Duration::from_secs(30)).await?;
                let mut fields = ssh::framed(&bytes, b"TRIA_DIR\0")?;
                let path = fields.remove(0);
                anyhow::ensure!(path.starts_with('/'), "Remote directory must be absolute");
                fields.sort();
                fields.insert(0, "..".into());
                Ok(Self {
                    path,
                    entries: fields,
                })
            }
        }
    }
}

fn directory_script(path: &str) -> String {
    format!(
        "set -eu\ndir={}\ncase \"$dir\" in ''|'~') dir=\"$HOME\";; '~/'*) dir=\"$HOME/${{dir#\\~/}}\";; esac\ncd -P \"$dir\"\nprintf 'TRIA_DIR\\000%s\\000' \"$PWD\"\nfor entry in .[^.]* ..?* *; do\n if [ -d \"$entry\" ]; then printf '%s\\000' \"$entry\"; fi\ndone\n",
        ssh::quote(path)
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Machine,
    Ssh,
    Browse,
}
pub enum Action {
    Read(Target, String),
    Open(Target, String),
    Close,
}

pub struct Wizard {
    pub id: String,
    pub stage: Stage,
    pub machines: Vec<Target>,
    pub selected: usize,
    pub target: Option<Target>,
    pub fields: [Composer; 4],
    pub field: usize,
    pub path: Composer,
    pub path_focus: bool,
    pub directory: Option<Directory>,
    pub busy: Option<String>,
    pub error: Option<String>,
    pub task: Option<tokio::task::JoinHandle<()>>,
}
impl Drop for Wizard {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
impl Wizard {
    pub fn new(saved: Vec<ssh::Target>) -> Self {
        Self {
            id: crate::commands::new_id(),
            stage: Stage::Machine,
            machines: std::iter::once(Target::Local)
                .chain(saved.into_iter().map(Target::Ssh))
                .collect(),
            selected: 0,
            target: None,
            fields: std::array::from_fn(|_| Composer::new()),
            field: 0,
            path: Composer::new(),
            path_focus: false,
            directory: None,
            busy: None,
            error: None,
            task: None,
        }
    }
    fn browse(&mut self, target: Target) -> Action {
        let path = if target == Target::Local {
            std::env::current_dir()
                .unwrap_or_else(|_| "/".into())
                .display()
                .to_string()
        } else {
            "~".into()
        };
        self.target = Some(target.clone());
        self.stage = Stage::Browse;
        self.selected = 0;
        self.path_focus = false;
        Action::Read(target, path)
    }
    pub fn loaded(&mut self, result: Result<Directory, String>) {
        self.busy = None;
        self.task = None;
        match result {
            Ok(directory) => {
                self.path.set_text(&directory.path);
                self.directory = Some(directory);
                self.selected = 0;
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
    }
    pub fn key(&mut self, key: KeyEvent) -> Option<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc {
            if self.busy.is_some() {
                return Some(Action::Close);
            }
            match self.stage {
                Stage::Machine => return Some(Action::Close),
                Stage::Ssh => {
                    self.stage = Stage::Machine;
                    self.selected = 0;
                }
                Stage::Browse => {
                    self.stage = Stage::Machine;
                    self.selected = 0;
                    self.directory = None;
                }
            }
            self.error = None;
            return None;
        }
        if self.busy.is_some() {
            return None;
        }
        match self.stage {
            Stage::Machine => match key.code {
                KeyCode::Up | KeyCode::BackTab => self.selected = self.selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Tab => {
                    self.selected = (self.selected + 1).min(self.machines.len())
                }
                KeyCode::Enter => {
                    if let Some(target) = self.machines.get(self.selected).cloned() {
                        return Some(self.browse(target));
                    }
                    self.stage = Stage::Ssh;
                }
                KeyCode::Char('l') => return Some(self.browse(Target::Local)),
                KeyCode::Char('r') => self.stage = Stage::Ssh,
                _ => {}
            },
            Stage::Ssh => match key.code {
                KeyCode::Tab | KeyCode::Down => self.field = (self.field + 1) % 4,
                KeyCode::BackTab | KeyCode::Up => self.field = (self.field + 3) % 4,
                KeyCode::Enter => {
                    let port = self.fields[2].text();
                    let target = ssh::Target {
                        host: self.fields[0].text().trim().into(),
                        user: self.fields[1].text().trim().into(),
                        port: if port.trim().is_empty() {
                            None
                        } else {
                            match port.trim().parse() {
                                Ok(port) => Some(port),
                                Err(_) => {
                                    self.error =
                                        Some("SSH port must be between 1 and 65535".into());
                                    return None;
                                }
                            }
                        },
                        identity: self.fields[3].text().trim().into(),
                    };
                    match target.validate() {
                        Ok(()) => return Some(self.browse(Target::Ssh(target))),
                        Err(err) => self.error = Some(err.to_string()),
                    }
                }
                _ => {
                    crate::app::edit_key(&mut self.fields[self.field], key);
                }
            },
            Stage::Browse => {
                let target = self.target.clone()?;
                if (ctrl && key.code == KeyCode::Char('o')) || key.code == KeyCode::F(2) {
                    return self
                        .directory
                        .as_ref()
                        .map(|directory| Action::Open(target, directory.path.clone()));
                }
                match key.code {
                    KeyCode::Tab | KeyCode::BackTab => self.path_focus = !self.path_focus,
                    KeyCode::Enter if self.path_focus => {
                        let typed = self.path.text();
                        let path = if typed.starts_with('/') || typed.starts_with('~') {
                            typed
                        } else if let Some(directory) = &self.directory {
                            format!("{}/{typed}", directory.path)
                        } else {
                            typed
                        };
                        return Some(Action::Read(target, path));
                    }
                    _ if self.path_focus => crate::app::edit_key(&mut self.path, key),
                    KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                    KeyCode::Down => {
                        self.selected = (self.selected + 1).min(
                            self.directory
                                .as_ref()
                                .map_or(0, |d| d.entries.len().saturating_sub(1)),
                        )
                    }
                    KeyCode::Enter => {
                        let dir = self.directory.as_ref()?;
                        let entry = dir.entries.get(self.selected)?;
                        return Some(Action::Read(target, format!("{}/{entry}", dir.path)));
                    }
                    KeyCode::Backspace => {
                        let dir = self.directory.as_ref()?;
                        return Some(Action::Read(target, format!("{}/..", dir.path)));
                    }
                    _ => {}
                }
            }
        }
        None
    }
    pub fn paste(&mut self, text: &str) {
        if self.busy.is_none() {
            let text = text.replace(['\n', '\r'], "");
            match self.stage {
                Stage::Ssh => self.fields[self.field].insert_str(&text),
                Stage::Browse if self.path_focus => self.path.insert_str(&text),
                _ => {}
            }
        }
    }
}

pub fn draw(frame: &mut Frame, wizard: &Wizard, area: Rect) {
    let width = area.width.saturating_sub(4).min(90);
    let height = area.height.saturating_sub(2).min(24);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let title = match wizard.stage {
        Stage::Machine => " Add project · choose machine ".into(),
        Stage::Ssh => " Add remote project · SSH configuration ".into(),
        Stage::Browse => format!(
            " Add project · {} ",
            wizard
                .target
                .as_ref()
                .map(Target::label)
                .unwrap_or_default()
        ),
    };
    let block = Block::bordered()
        .title(title)
        .border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let mut cursor = None;
    let [content, status, hint] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(2),
        Constraint::Length(2),
    ])
    .areas(inner);
    match wizard.stage {
        Stage::Machine => {
            let mut rows: Vec<_> = wizard
                .machines
                .iter()
                .map(|target| {
                    ListItem::new(match target {
                        Target::Local => "Local · browse this machine".to_string(),
                        Target::Ssh(_) => format!("Remote · {}", target.label()),
                    })
                })
                .collect();
            rows.push(ListItem::new("Remote · configure SSH…"));
            frame.render_stateful_widget(
                List::new(rows)
                    .highlight_style(Style::default().bg(Color::Indexed(238)))
                    .highlight_symbol("› "),
                content,
                &mut ListState::default().with_selected(Some(wizard.selected)),
            );
        }
        Stage::Ssh => {
            let labels = [
                "Host / SSH config alias",
                "User (optional)",
                "Port (optional)",
                "Identity file (optional)",
            ];
            let mut lines = vec![
                Line::from("Uses ~/.ssh/config and your SSH agent or key."),
                Line::from(""),
            ];
            for (i, label) in labels.iter().enumerate() {
                let style = if i == wizard.field {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                };
                let prefix = format!("{} {label}: ", if i == wizard.field { "›" } else { " " });
                let prefix_width = prefix.chars().count() as u16;
                let (visible, column) = wizard.fields[i].line_window(
                    content.width.saturating_sub(prefix_width).saturating_sub(1) as usize,
                );
                lines.push(Line::styled(format!("{prefix}{visible}"), style));
                if i == wizard.field && content.height > 2 + i as u16 * 2 {
                    cursor = Some((
                        content.x + prefix_width + column as u16,
                        content.y + 2 + i as u16 * 2,
                    ));
                }
                lines.push(Line::from(""));
            }
            frame.render_widget(Paragraph::new(lines), content);
        }
        Stage::Browse => {
            let [path, list] =
                Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]).areas(content);
            let prefix = if wizard.path_focus { "› " } else { "Path: " };
            let (visible, column) = wizard.path.line_window(
                path.width
                    .saturating_sub(prefix.len() as u16)
                    .saturating_sub(1) as usize,
            );
            frame.render_widget(
                Paragraph::new(format!("{prefix}{visible}")).style(Style::default().fg(
                    if wizard.path_focus {
                        Color::Yellow
                    } else {
                        Color::Cyan
                    },
                )),
                path,
            );
            if wizard.path_focus && path.height > 0 {
                cursor = Some((path.x + 2 + column as u16, path.y));
            }
            let rows = wizard
                .directory
                .as_ref()
                .map(|d| {
                    d.entries
                        .iter()
                        .map(|entry| ListItem::new(format!("{entry}/")))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            frame.render_stateful_widget(
                List::new(rows)
                    .highlight_style(Style::default().bg(Color::Indexed(238)))
                    .highlight_symbol("› "),
                list,
                &mut ListState::default().with_selected(if wizard.path_focus {
                    None
                } else {
                    Some(wizard.selected)
                }),
            );
        }
    }
    let message = wizard
        .busy
        .as_deref()
        .or(wizard.error.as_deref())
        .unwrap_or("");
    frame.render_widget(
        Paragraph::new(message)
            .style(Style::default().fg(if wizard.error.is_some() {
                Color::Red
            } else {
                Color::Yellow
            }))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        status,
    );
    frame.render_widget(
        Paragraph::new(match wizard.stage {
            Stage::Machine => "↑↓ choose · Enter continue · Esc close",
            Stage::Ssh => "Tab field · Enter browse · Esc back",
            Stage::Browse => {
                "Enter browse · Tab path · Backspace parent\n^O / F2 use this directory · Esc back"
            }
        })
        .style(Style::default().fg(Color::DarkGray)),
        hint,
    );
    if let Some(cursor) = cursor
        && wizard.busy.is_none()
        && cursor.0 < area.right()
        && cursor.1 < area.bottom()
    {
        frame.set_cursor_position(cursor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn remote_listing_keeps_shell_characters_and_newlines_in_paths_literal() {
        use tokio::io::AsyncWriteExt;
        let root = std::env::temp_dir().join(format!("tria-browser-{}", crate::commands::new_id()));
        let weird = root.join("quote ' dollar $(false)\nspace");
        std::fs::create_dir_all(weird.join("child ' $(false)\nfolder")).unwrap();
        std::fs::write(weird.join("file"), "not a directory").unwrap();
        let mut child = tokio::process::Command::new("sh")
            .arg("-s")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(directory_script(weird.to_str().unwrap()).as_bytes())
            .await
            .unwrap();
        let output = child.wait_with_output().await.unwrap();
        assert!(output.status.success());
        let fields = ssh::framed(&output.stdout, b"TRIA_DIR\0").unwrap();
        assert!(fields[0].ends_with("quote ' dollar $(false)\nspace"));
        assert_eq!(&fields[1..], ["child ' $(false)\nfolder"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn canceling_the_browser_aborts_its_pending_operation() {
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let aborted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        struct Mark(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Mark {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let mut wizard = Wizard::new(vec![]);
        wizard.task = Some(tokio::spawn({
            let started = started.clone();
            let aborted = aborted.clone();
            async move {
                let _mark = Mark(aborted);
                started.notify_one();
                std::future::pending::<()>().await;
            }
        }));
        started.notified().await;
        drop(wizard);
        tokio::task::yield_now().await;
        assert!(aborted.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn project_dialog_renders_choices_fields_and_browser_controls() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut wizard = Wizard::new(vec![]);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut screens = vec![];
        for stage in [Stage::Machine, Stage::Ssh, Stage::Browse] {
            wizard.stage = stage;
            wizard.target = Some(Target::Local);
            wizard.loaded(Ok(Directory {
                path: "/projects".into(),
                entries: vec!["..".into(), "tria".into()],
            }));
            terminal
                .draw(|frame| draw(frame, &wizard, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let screen = (0..24)
                .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n");
            match stage {
                Stage::Machine => {
                    assert!(screen.contains("Local · browse this machine"));
                    assert!(screen.contains("Remote · configure SSH"));
                }
                Stage::Ssh => {
                    assert!(screen.contains("Host / SSH config alias"));
                    assert!(screen.contains("Identity file"));
                }
                Stage::Browse => {
                    assert!(screen.contains("/projects"));
                    assert!(screen.contains("F2 use this directory"));
                    assert!(screen.contains("Esc back"));
                }
            }
            screens.push(screen);
        }
        std::fs::write(
            std::env::temp_dir().join("tria-project-dialog.txt"),
            screens.join("\n\n"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn local_browser_lists_directories_and_rejects_files() {
        let dir = Directory::read(&Target::Local, ".").await.unwrap();
        assert_eq!(dir.entries[0], "..");
        assert!(dir.entries.contains(&"src".into()));
        assert!(Directory::read(&Target::Local, "Cargo.toml").await.is_err());
    }
    #[test]
    fn opening_uses_the_remote_directory_without_local_resolution() {
        let mut wizard = Wizard::new(vec![]);
        wizard.target = Some(Target::Ssh(ssh::Target {
            host: "server".into(),
            ..Default::default()
        }));
        wizard.stage = Stage::Browse;
        wizard.loaded(Ok(Directory {
            path: "/remote/only".into(),
            entries: vec!["..".into()],
        }));
        assert!(
            matches!(wizard.key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)), Some(Action::Open(Target::Ssh(_), path)) if path == "/remote/only")
        );
    }
}

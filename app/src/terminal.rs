//! Terminals in the reference app: a shell on the connected host, a shell
//! on any SSH server, and application launches on the host. The library
//! does everything (the command stream, the SSH client, the screen model);
//! this keeps the open terminals and draws one in a window of its own.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Color32, FontId, Key as EguiKey};
use windowcast_client::{Client, ClientError, ClientSession, SshSession};
use windowcast_protocol::command::TerminalSize;
use windowcast_terminal::{HostKeyPolicy, Key, SshAuth, SshError, SshTarget, Terminal};

/// An open terminal. The SSH login (if any) lives as long as the screen.
pub struct TerminalEntry {
    pub id: u64,
    pub title: String,
    pub terminal: Terminal,
    _login: Option<SshSession>,
}

/// What the user typed to log in to an SSH server.
#[derive(Clone, Default)]
pub struct SshRequest {
    pub host: String,
    pub port: String,
    pub user: String,
    pub password: String,
    /// Path of a private key file; used instead of the password when set.
    pub key_file: String,
    pub passphrase: String,
}

/// Where the SSH form stands, for the client window to show.
#[derive(Clone, Default)]
pub struct SshProgress {
    pub busy: bool,
    pub error: Option<String>,
    /// A server not pinned yet, with the key it presented: the user
    /// decides whether to trust it.
    pub untrusted: Option<(String, String)>,
    pub launched: Option<String>,
}

#[derive(Default)]
pub struct Terminals {
    open: Mutex<Vec<Arc<TerminalEntry>>>,
    progress: Mutex<SshProgress>,
    next: AtomicU64,
}

const START_SIZE: TerminalSize = TerminalSize {
    cols: 100,
    rows: 30,
};

impl Terminals {
    pub fn open(&self) -> Vec<Arc<TerminalEntry>> {
        self.open.lock().expect("terminals").clone()
    }

    pub fn progress(&self) -> SshProgress {
        self.progress.lock().expect("progress").clone()
    }

    pub fn close(&self, id: u64) {
        self.open.lock().expect("terminals").retain(|t| t.id != id);
    }

    fn add(&self, title: String, terminal: Terminal, login: Option<SshSession>) {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.open
            .lock()
            .expect("terminals")
            .push(Arc::new(TerminalEntry {
                id,
                title,
                terminal,
                _login: login,
            }));
    }

    fn set_progress(&self, update: impl FnOnce(&mut SshProgress)) {
        update(&mut self.progress.lock().expect("progress"));
    }

    /// A shell on the connected host (blocking).
    pub fn open_host(&self, session: &ClientSession, host: &str) -> Result<(), ClientError> {
        let terminal = session.open_terminal(START_SIZE)?;
        self.add(format!("shell on {host}"), terminal, None);
        Ok(())
    }

    /// Starts an application on the connected host (blocking). The words
    /// of `command_line` are the program and its arguments.
    pub fn launch(&self, session: &ClientSession, command_line: &str) {
        let argv: Vec<String> = command_line.split_whitespace().map(str::to_owned).collect();
        let result = if argv.is_empty() {
            Err("type a program to start".to_owned())
        } else {
            session
                .launch(argv)
                .map(|pid| match pid {
                    Some(pid) => format!("started (process {pid}); its windows are in the list"),
                    None => "started; its windows are in the list".to_owned(),
                })
                .map_err(|e| e.to_string())
        };
        self.set_progress(|p| match result {
            Ok(done) => (p.error, p.launched) = (None, Some(done)),
            Err(e) => (p.error, p.launched) = (Some(e), None),
        });
    }

    /// Logs in to an SSH server and opens a shell (blocking). A server not
    /// pinned yet is not trusted unless `trust` is the fingerprint it
    /// presents; its fingerprint is then left in the progress for the user
    /// to confirm.
    pub fn open_ssh(&self, client: &Client, request: &SshRequest, trust: Option<String>) {
        self.set_progress(|p| {
            *p = SshProgress {
                busy: true,
                ..Default::default()
            }
        });
        let result = self.try_ssh(client, request, trust);
        self.set_progress(|p| {
            p.busy = false;
            match result {
                Ok(()) => *p = SshProgress::default(),
                Err(SshFailure::Untrusted(server, fingerprint)) => {
                    p.untrusted = Some((server, fingerprint))
                }
                Err(SshFailure::Other(e)) => p.error = Some(e),
            }
        });
    }

    fn try_ssh(
        &self,
        client: &Client,
        request: &SshRequest,
        trust: Option<String>,
    ) -> Result<(), SshFailure> {
        let port = if request.port.trim().is_empty() {
            22
        } else {
            request
                .port
                .trim()
                .parse()
                .map_err(|_| SshFailure::Other("the port is not a number".into()))?
        };
        let target = SshTarget {
            host: request.host.trim().to_owned(),
            port,
            user: request.user.trim().to_owned(),
        };
        if target.host.is_empty() || target.user.is_empty() {
            return Err(SshFailure::Other(
                "a server and a user name are needed".into(),
            ));
        }
        let auth = if request.key_file.trim().is_empty() {
            SshAuth::Password(request.password.clone())
        } else {
            let pem = std::fs::read_to_string(request.key_file.trim())
                .map_err(|e| SshFailure::Other(format!("cannot read the key file: {e}")))?;
            SshAuth::Key {
                pem,
                passphrase: (!request.passphrase.is_empty()).then(|| request.passphrase.clone()),
            }
        };
        let policy = match trust {
            Some(fingerprint) => HostKeyPolicy::Fingerprint(fingerprint),
            None => HostKeyPolicy::Pinned,
        };
        let login = match client.ssh_connect(&target, &auth, policy) {
            Ok(login) => login,
            Err(ClientError::Ssh(SshError::HostKeyRefused {
                server,
                fingerprint,
            })) => return Err(SshFailure::Untrusted(server, fingerprint)),
            Err(e) => return Err(SshFailure::Other(e.to_string())),
        };
        let terminal = login
            .open_terminal(START_SIZE)
            .map_err(|e| SshFailure::Other(e.to_string()))?;
        self.add(
            format!("{}@{}", target.user, target.host),
            terminal,
            Some(login),
        );
        Ok(())
    }
}

enum SshFailure {
    Untrusted(String, String),
    Other(String),
}

const DEFAULT_FG: Color32 = Color32::from_rgb(0xdd, 0xdd, 0xdd);
const DEFAULT_BG: Color32 = Color32::from_rgb(0x10, 0x10, 0x10);

fn rgb(value: u32) -> Color32 {
    Color32::from_rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

/// Draws a terminal's screen in `ui` and feeds it the keyboard.
pub fn terminal_ui(ui: &mut egui::Ui, entry: &TerminalEntry) {
    let terminal = &entry.terminal;
    let font = FontId::monospace(15.0);
    let cell = ui
        .painter()
        .layout_no_wrap("M".to_owned(), font.clone(), DEFAULT_FG)
        .size();

    let available = ui.available_size();
    let size = TerminalSize {
        cols: (available.x / cell.x).floor().max(1.0) as u16,
        rows: (available.y / cell.y).floor().max(1.0) as u16,
    };
    let snapshot = terminal.snapshot();
    if (snapshot.cols, snapshot.rows) != (size.cols, size.rows) && terminal.ended().is_none() {
        terminal.resize(size);
    }

    let (rect, response) = ui.allocate_exact_size(available, egui::Sense::click());
    response.request_focus();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, DEFAULT_BG);
    for (row, line) in snapshot.lines.iter().enumerate() {
        let y = rect.top() + row as f32 * cell.y;
        let mut x = rect.left();
        for run in line {
            let width = run.text.chars().count() as f32 * cell.x;
            let (mut fg, mut bg) = (
                run.fg.map_or(DEFAULT_FG, rgb),
                run.bg.map_or(DEFAULT_BG, rgb),
            );
            if run.inverse {
                std::mem::swap(&mut fg, &mut bg);
            }
            if bg != DEFAULT_BG {
                painter.rect_filled(
                    egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(width, cell.y)),
                    0.0,
                    bg,
                );
            }
            if !run.text.trim().is_empty() {
                painter.text(
                    egui::pos2(x, y),
                    egui::Align2::LEFT_TOP,
                    &run.text,
                    font.clone(),
                    fg,
                );
            }
            x += width;
        }
    }
    if let Some((row, col)) = snapshot.cursor {
        painter.rect_stroke(
            egui::Rect::from_min_size(
                egui::pos2(
                    rect.left() + f32::from(col) * cell.x,
                    rect.top() + f32::from(row) * cell.y,
                ),
                cell,
            ),
            0.0,
            egui::Stroke::new(1.5_f32, DEFAULT_FG),
            egui::StrokeKind::Inside,
        );
    }
    if let Some(code) = terminal.ended() {
        let text = match code {
            Some(code) => format!("[the shell ended, exit code {code}]"),
            None => "[the shell ended]".to_owned(),
        };
        painter.text(
            rect.left_bottom() + egui::vec2(4.0, -4.0),
            egui::Align2::LEFT_BOTTOM,
            text,
            font,
            Color32::YELLOW,
        );
    }

    send_keyboard(ui.ctx(), terminal);
    for text in terminal.take_clipboard() {
        ui.ctx().copy_text(text);
    }
}

fn send_keyboard(ctx: &egui::Context, terminal: &Terminal) {
    let events = ctx.input(|i| i.events.clone());
    for event in events {
        match event {
            egui::Event::Text(text) => terminal.send_text(&text),
            egui::Event::Paste(text) => terminal.paste(&text),
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                if modifiers.ctrl && !modifiers.alt {
                    // Ctrl+V is the paste event; every other Ctrl+letter
                    // is the control character.
                    if key != EguiKey::V {
                        if let Some(c) = letter(key) {
                            terminal.send_control(c);
                        }
                    }
                } else if let Some(key) = named(key) {
                    terminal.send_key(key);
                }
            }
            _ => {}
        }
    }
}

fn letter(key: EguiKey) -> Option<char> {
    let name = key.name();
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => Some(c),
        _ => None,
    }
}

fn named(key: EguiKey) -> Option<Key> {
    Some(match key {
        EguiKey::Enter => Key::Enter,
        EguiKey::Backspace => Key::Backspace,
        EguiKey::Tab => Key::Tab,
        EguiKey::Escape => Key::Escape,
        EguiKey::ArrowUp => Key::Up,
        EguiKey::ArrowDown => Key::Down,
        EguiKey::ArrowLeft => Key::Left,
        EguiKey::ArrowRight => Key::Right,
        EguiKey::Home => Key::Home,
        EguiKey::End => Key::End,
        EguiKey::PageUp => Key::PageUp,
        EguiKey::PageDown => Key::PageDown,
        EguiKey::Insert => Key::Insert,
        EguiKey::Delete => Key::Delete,
        EguiKey::F1 => Key::F(1),
        EguiKey::F2 => Key::F(2),
        EguiKey::F3 => Key::F(3),
        EguiKey::F4 => Key::F(4),
        EguiKey::F5 => Key::F(5),
        EguiKey::F6 => Key::F(6),
        EguiKey::F7 => Key::F(7),
        EguiKey::F8 => Key::F(8),
        EguiKey::F9 => Key::F(9),
        EguiKey::F10 => Key::F(10),
        EguiKey::F11 => Key::F(11),
        EguiKey::F12 => Key::F(12),
        _ => return None,
    })
}

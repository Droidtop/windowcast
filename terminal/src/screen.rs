//! The screen model: a shell's bytes (escape sequences and all) turned into
//! rows of coloured cells by a VT100 emulator, and keys turned into the
//! bytes a shell expects. It lives in the client library so every viewer
//! (the egui app, the Android viewer) draws the same screen from one
//! [`Snapshot`] and has no emulator of its own.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Serialize;
use windowcast_protocol::command::TerminalSize;

use crate::channel::{Channel, ChannelEvent};

/// Lines of history kept above the screen.
const SCROLLBACK: usize = 1000;

#[derive(Default)]
struct Callbacks {
    /// Text programs set on the clipboard (OSC 52), not yet taken.
    clipboard: Vec<String>,
}

impl vt100::Callbacks for Callbacks {
    /// OSC 52 ; selection ; base64 data. A request to read the clipboard
    /// (`paste_from_clipboard`) is left unanswered: a remote program never
    /// gets the user's clipboard.
    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, _selection: &[u8], data: &[u8]) {
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
            if let Ok(text) = String::from_utf8(bytes) {
                self.clipboard.push(text);
            }
        }
    }
}

/// A run of cells with the same look.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Run {
    pub text: String,
    /// `0xRRGGBB`, or `None` for the viewer's default colour.
    pub fg: Option<u32>,
    pub bg: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// Foreground and background swap (the viewer knows its defaults).
    pub inverse: bool,
}

/// What a viewer draws: every row as runs, the cursor, and the modes that
/// change what the viewer sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<Vec<Run>>,
    /// Row and column of the cursor, `None` while a program hides it.
    pub cursor: Option<(u16, u16)>,
    pub alternate_screen: bool,
    /// How far the view is scrolled back from the live screen, in lines.
    pub scrollback: usize,
    /// Bumped on every change, so a viewer redraws only on news.
    pub version: u64,
}

fn palette(index: u8) -> u32 {
    const BASE: [u32; 16] = [
        0x000000, 0xcd0000, 0x00cd00, 0xcdcd00, 0x0000ee, 0xcd00cd, 0x00cdcd, 0xe5e5e5, 0x7f7f7f,
        0xff0000, 0x00ff00, 0xffff00, 0x5c5cff, 0xff00ff, 0x00ffff, 0xffffff,
    ];
    match index {
        0..=15 => BASE[index as usize],
        16..=231 => {
            let n = u32::from(index) - 16;
            let level = |v: u32| if v == 0 { 0 } else { 55 + v * 40 };
            (level(n / 36) << 16) | (level(n / 6 % 6) << 8) | level(n % 6)
        }
        _ => {
            let v = 8 + (u32::from(index) - 232) * 10;
            (v << 16) | (v << 8) | v
        }
    }
}

fn color(color: vt100::Color) -> Option<u32> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(palette(i)),
        vt100::Color::Rgb(r, g, b) => Some(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b)),
    }
}

/// A key a viewer has no text for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Enter,
    Backspace,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// F1 to F12.
    F(u8),
}

impl Key {
    /// The key by name (`"Enter"`, `"PageUp"`, `"F5"`), as the C interface
    /// and the viewers spell it.
    pub fn from_name(name: &str) -> Option<Key> {
        Some(match name {
            "Enter" => Key::Enter,
            "Backspace" => Key::Backspace,
            "Tab" => Key::Tab,
            "Escape" => Key::Escape,
            "Up" => Key::Up,
            "Down" => Key::Down,
            "Left" => Key::Left,
            "Right" => Key::Right,
            "Home" => Key::Home,
            "End" => Key::End,
            "PageUp" => Key::PageUp,
            "PageDown" => Key::PageDown,
            "Insert" => Key::Insert,
            "Delete" => Key::Delete,
            _ => {
                let n: u8 = name.strip_prefix('F')?.parse().ok()?;
                if !(1..=12).contains(&n) {
                    return None;
                }
                Key::F(n)
            }
        })
    }

    /// The bytes a terminal sends for the key. `application_cursor` is the
    /// screen's mode (programs like vi and less switch it on).
    pub fn bytes(self, application_cursor: bool) -> Vec<u8> {
        let arrow = |c: char| {
            let intro = if application_cursor { "\x1bO" } else { "\x1b[" };
            format!("{intro}{c}").into_bytes()
        };
        match self {
            Key::Enter => b"\r".to_vec(),
            Key::Backspace => vec![0x7f],
            Key::Tab => b"\t".to_vec(),
            Key::Escape => vec![0x1b],
            Key::Up => arrow('A'),
            Key::Down => arrow('B'),
            Key::Right => arrow('C'),
            Key::Left => arrow('D'),
            Key::Home => arrow('H'),
            Key::End => arrow('F'),
            Key::PageUp => b"\x1b[5~".to_vec(),
            Key::PageDown => b"\x1b[6~".to_vec(),
            Key::Insert => b"\x1b[2~".to_vec(),
            Key::Delete => b"\x1b[3~".to_vec(),
            Key::F(n) => match n {
                1..=4 => format!("\x1bO{}", (b'P' + n - 1) as char).into_bytes(),
                5 => b"\x1b[15~".to_vec(),
                6..=8 => format!("\x1b[{}~", 17 + n - 6).into_bytes(),
                9 | 10 => format!("\x1b[{}~", 20 + n - 9).into_bytes(),
                _ => format!("\x1b[{}~", 23 + n - 11).into_bytes(),
            },
        }
    }
}

/// The byte a terminal sends for Ctrl and a letter or one of `@[\]^_`;
/// `None` for anything else.
pub fn control_byte(c: char) -> Option<u8> {
    match c.to_ascii_uppercase() {
        c @ '@'..='_' => Some(c as u8 - b'@'),
        _ => None,
    }
}

struct Shared {
    parser: Mutex<vt100::Parser<Callbacks>>,
    version: Mutex<u64>,
    changed: Condvar,
    ended: Mutex<Option<Option<i32>>>,
}

/// A channel and the screen it draws: the one object a viewer holds.
pub struct Terminal {
    channel: Arc<Channel>,
    shared: Arc<Shared>,
}

impl Terminal {
    /// Starts reading `channel` into a screen of `size`.
    pub fn new(channel: Channel, size: TerminalSize) -> Terminal {
        let size = size.clamped();
        let channel = Arc::new(channel);
        let shared = Arc::new(Shared {
            parser: Mutex::new(vt100::Parser::new_with_callbacks(
                size.rows,
                size.cols,
                SCROLLBACK,
                Callbacks::default(),
            )),
            version: Mutex::new(1),
            changed: Condvar::new(),
            ended: Mutex::new(None),
        });
        let (reader, state) = (Arc::clone(&channel), Arc::clone(&shared));
        std::thread::spawn(move || loop {
            let Some(event) = reader.next_event(Duration::from_millis(500)) else {
                continue;
            };
            match event {
                ChannelEvent::Data(bytes) => {
                    state.parser.lock().expect("screen").process(&bytes);
                }
                ChannelEvent::Exited(code) => {
                    state.ended.lock().expect("ended").get_or_insert(code);
                }
                ChannelEvent::Closed => {
                    state.ended.lock().expect("ended").get_or_insert(None);
                    *state.version.lock().expect("version") += 1;
                    state.changed.notify_all();
                    return;
                }
            }
            *state.version.lock().expect("version") += 1;
            state.changed.notify_all();
        });
        Terminal { channel, shared }
    }

    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    /// Types bytes (already encoded) into the terminal.
    pub fn send(&self, bytes: &[u8]) {
        self.channel.write(bytes);
    }

    /// Types text, as the keyboard would.
    pub fn send_text(&self, text: &str) {
        self.channel.write(text.as_bytes());
    }

    /// Types a key not made of text.
    pub fn send_key(&self, key: Key) {
        let application_cursor = self
            .shared
            .parser
            .lock()
            .expect("screen")
            .screen()
            .application_cursor();
        self.channel.write(&key.bytes(application_cursor));
    }

    /// Types Ctrl and a letter (or one of `@[\]^_`).
    pub fn send_control(&self, c: char) {
        if let Some(byte) = control_byte(c) {
            self.channel.write(&[byte]);
        }
    }

    /// Pastes text, in the bracketed form when the program asked for it so
    /// it can tell a paste from typing.
    pub fn paste(&self, text: &str) {
        let bracketed = self
            .shared
            .parser
            .lock()
            .expect("screen")
            .screen()
            .bracketed_paste();
        if bracketed {
            self.channel.write(b"\x1b[200~");
            self.channel.write(text.as_bytes());
            self.channel.write(b"\x1b[201~");
        } else {
            self.channel.write(text.as_bytes());
        }
    }

    /// The window showing the terminal changed size.
    pub fn resize(&self, size: TerminalSize) {
        let size = size.clamped();
        {
            let mut parser = self.shared.parser.lock().expect("screen");
            parser.screen_mut().set_size(size.rows, size.cols);
        }
        self.channel.resize(size);
        *self.shared.version.lock().expect("version") += 1;
        self.shared.changed.notify_all();
    }

    /// Scrolls the view back by `lines` from the live screen (0 returns to
    /// it).
    pub fn scroll_to(&self, lines: usize) {
        self.shared
            .parser
            .lock()
            .expect("screen")
            .screen_mut()
            .set_scrollback(lines);
        *self.shared.version.lock().expect("version") += 1;
        self.shared.changed.notify_all();
    }

    /// Waits until the screen has changed since `seen` (a
    /// [`Snapshot::version`]), up to `timeout`. Returns whether it did.
    pub fn wait_change(&self, seen: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut version = self.shared.version.lock().expect("version");
        while *version == seen {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (guard, result) = self
                .shared
                .changed
                .wait_timeout(version, left)
                .expect("version");
            version = guard;
            if result.timed_out() && *version == seen {
                return false;
            }
        }
        true
    }

    /// The text a program set on the clipboard since the last call.
    pub fn take_clipboard(&self) -> Vec<String> {
        std::mem::take(
            &mut self
                .shared
                .parser
                .lock()
                .expect("screen")
                .callbacks_mut()
                .clipboard,
        )
    }

    /// `Some` once the command ended (with its exit code when there is
    /// one) or the connection went.
    pub fn ended(&self) -> Option<Option<i32>> {
        *self.shared.ended.lock().expect("ended")
    }

    pub fn snapshot(&self) -> Snapshot {
        let version = *self.shared.version.lock().expect("version");
        let parser = self.shared.parser.lock().expect("screen");
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        let mut lines = Vec::with_capacity(rows as usize);
        for row in 0..rows {
            let mut runs: Vec<Run> = Vec::new();
            for col in 0..cols {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let text = if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                };
                let look = (
                    color(cell.fgcolor()),
                    color(cell.bgcolor()),
                    cell.bold(),
                    cell.italic(),
                    cell.underline(),
                    cell.inverse(),
                );
                match runs.last_mut() {
                    Some(last)
                        if (
                            last.fg,
                            last.bg,
                            last.bold,
                            last.italic,
                            last.underline,
                            last.inverse,
                        ) == look =>
                    {
                        last.text.push_str(text)
                    }
                    _ => runs.push(Run {
                        text: text.to_owned(),
                        fg: look.0,
                        bg: look.1,
                        bold: look.2,
                        italic: look.3,
                        underline: look.4,
                        inverse: look.5,
                    }),
                }
            }
            lines.push(runs);
        }
        Snapshot {
            cols,
            rows,
            lines,
            cursor: (!screen.hide_cursor() && screen.scrollback() == 0)
                .then(|| screen.cursor_position()),
            alternate_screen: screen.alternate_screen(),
            scrollback: screen.scrollback(),
            version,
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.channel.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal() -> (Terminal, crate::channel::ChannelEnd) {
        let (channel, end) = Channel::pair(None);
        (
            Terminal::new(channel, TerminalSize { cols: 20, rows: 4 }),
            end,
        )
    }

    fn feed(end: &crate::channel::ChannelEnd, t: &Terminal, bytes: &[u8]) {
        let seen = t.snapshot().version;
        end.events.send(ChannelEvent::Data(bytes.to_vec())).unwrap();
        assert!(t.wait_change(seen, Duration::from_secs(5)));
    }

    fn text(snapshot: &Snapshot, row: usize) -> String {
        snapshot.lines[row]
            .iter()
            .map(|r| r.text.as_str())
            .collect()
    }

    #[test]
    fn colours_and_the_cursor_come_out_as_runs() {
        let (t, end) = terminal();
        feed(&end, &t, b"hi \x1b[1;31mred\x1b[0m!\r\nnext");
        let s = t.snapshot();
        assert_eq!(text(&s, 0).trim_end(), "hi red!");
        let red = s.lines[0].iter().find(|r| r.text == "red").unwrap();
        assert_eq!((red.fg, red.bold), (Some(0xcd0000), true));
        assert_eq!(s.cursor, Some((1, 4)));
        assert_eq!((s.cols, s.rows), (20, 4));
    }

    #[test]
    fn a_program_sets_the_clipboard_with_osc_52() {
        let (t, end) = terminal();
        // "copied" in base64
        feed(&end, &t, b"\x1b]52;c;Y29waWVk\x07");
        assert_eq!(t.take_clipboard(), ["copied"]);
        assert!(t.take_clipboard().is_empty());
        // A request to read the clipboard is never answered.
        feed(&end, &t, b"\x1b]52;c;?\x07x");
        assert!(t.take_clipboard().is_empty());
    }

    #[test]
    fn a_paste_is_bracketed_only_when_the_program_asked() {
        let (t, end) = terminal();
        let mut end = end;
        feed(&end, &t, b"");
        t.paste("one");
        feed(&end, &t, b"\x1b[?2004h");
        t.paste("two");
        let mut sent = Vec::new();
        while let Ok(command) = end.commands.try_recv() {
            if let crate::channel::Command::Data(bytes) = command {
                sent.extend(bytes);
            }
        }
        assert_eq!(sent, b"one\x1b[200~two\x1b[201~");
    }

    #[test]
    fn keys_follow_the_cursor_mode() {
        assert_eq!(Key::Up.bytes(false), b"\x1b[A");
        assert_eq!(Key::Up.bytes(true), b"\x1bOA");
        assert_eq!(Key::F(1).bytes(false), b"\x1bOP");
        assert_eq!(Key::F(5).bytes(false), b"\x1b[15~");
        assert_eq!(Key::F(12).bytes(false), b"\x1b[24~");
        assert_eq!(Key::from_name("PageDown"), Some(Key::PageDown));
        assert_eq!(Key::from_name("F13"), None);
        assert_eq!(control_byte('c'), Some(3));
        assert_eq!(control_byte('1'), None);
    }

    #[test]
    fn the_exit_is_remembered() {
        let (t, end) = terminal();
        end.events.send(ChannelEvent::Exited(Some(3))).unwrap();
        end.events.send(ChannelEvent::Closed).unwrap();
        let mut seen = 0;
        while t.ended().is_none() {
            assert!(t.wait_change(seen, Duration::from_secs(5)));
            seen = t.snapshot().version;
        }
        assert_eq!(t.ended(), Some(Some(3)));
    }
}

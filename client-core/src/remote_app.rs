//! RemoteApp launches, client side (docs/BACKENDS.md, "RemoteApp"): a
//! launch the rules give RDP asks the host for a RemoteApp; the client
//! logs in to the host's Remote Desktop itself and runs the program there,
//! and each window Windows reports joins this session's window list under
//! an id of its own ([`REMOTE_APP_WINDOW`] set), its pictures cut from the
//! RemoteApp session's desktop and its input sent to it.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use windowcast_protocol::{
    selection, BackendKind, ContentHint, HandoffTarget, InputEvent, WindowId, WindowInfo,
};
use windowcast_rdp::client::{ClientConfig, RdpStream, RgbaPicture};
use windowcast_rdp::remoteapp::{ListedWindow, Rect, RemoteApp};

use crate::{ClientError, ClientSession, Event, PicturePoll, Shared};

/// Set in the id of every RemoteApp window; the host's own ids never have
/// it.
pub const REMOTE_APP_WINDOW: u64 = 1 << 63;

/// The desktop a RemoteApp's windows live on.
const DESKTOP: (u16, u16) = (1920, 1080);

/// How long Windows has to log the user in and start the program.
const START_WITHIN: Duration = Duration::from_secs(90);

/// A program's windows close: the RemoteApp ends once none came back
/// within this.
const GONE_AFTER: Duration = Duration::from_secs(3);

/// One RemoteApp connection.
pub(crate) struct Conn {
    key: u32,
    /// The program's file name, the windows' `app_id`.
    app_id: String,
    stream: Arc<RdpStream>,
    listed: Vec<ListedWindow>,
}

fn window_id(key: u32, window: u32) -> WindowId {
    WindowId(REMOTE_APP_WINDOW | (u64::from(key) << 32) | u64::from(window))
}

/// The connection key and Windows' window id of a RemoteApp window.
pub(crate) fn split(window: WindowId) -> Option<(u32, u32)> {
    (window.0 & REMOTE_APP_WINDOW != 0).then_some((
        ((window.0 & !REMOTE_APP_WINDOW) >> 32) as u32,
        window.0 as u32,
    ))
}

/// A program's file name, as a window's `app_id` would give it.
fn app_id(program: &str) -> String {
    program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_owned()
}

/// Windows quotes an argument with spaces or quotes in it.
fn quote(argument: &str) -> String {
    if !argument.is_empty() && !argument.contains([' ', '\t', '"']) {
        return argument.to_owned();
    }
    format!("\"{}\"", argument.replace('"', "\\\""))
}

/// The RemoteApp windows, as window list entries.
pub(crate) fn infos(shared: &Shared) -> Vec<WindowInfo> {
    let conns = shared.remote_apps.lock().expect("remote apps");
    conns
        .iter()
        .flat_map(|conn| {
            conn.listed.iter().map(|w| {
                // An untitled window is named after its owner, or its program.
                let owner_title = w
                    .owner
                    .and_then(|o| conn.listed.iter().find(|l| l.id == o))
                    .map(|o| o.title.as_str())
                    .filter(|t| !t.is_empty());
                let title = match (w.title.is_empty(), owner_title) {
                    (false, _) => w.title.clone(),
                    (true, Some(owner)) => format!("{owner} popup"),
                    (true, None) => conn.app_id.clone(),
                };
                WindowInfo {
                    id: window_id(conn.key, w.id),
                    title,
                    app_id: conn.app_id.clone(),
                    width: w.rect.width,
                    height: w.rect.height,
                    focused: false,
                    owner: w.owner.map(|o| window_id(conn.key, o)),
                    kind: w.kind,
                    position: Some((w.rect.x, w.rect.y)),
                    content: ContentHint::Text,
                }
            })
        })
        .collect()
}

/// The connection and rectangle of a RemoteApp window that is listed now.
pub(crate) fn find(shared: &Shared, window: WindowId) -> Option<(Arc<RdpStream>, u32, Rect)> {
    let (key, id) = split(window)?;
    let conns = shared.remote_apps.lock().expect("remote apps");
    let conn = conns.iter().find(|c| c.key == key)?;
    let rect = conn.listed.iter().find(|w| w.id == id)?.rect;
    Some((Arc::clone(&conn.stream), id, rect))
}

impl ClientSession {
    /// Whether a launch of `argv` asks for a RemoteApp: the rules give the
    /// program RDP (as they would its window), this client shows pictures,
    /// and the host is on this network.
    pub(crate) fn wants_remote_app(&self, argv: &[String]) -> bool {
        let Some(program) = argv.first() else {
            return false;
        };
        if self.shared.host_ip.is_none() || !self.pictures.load(Ordering::SeqCst) {
            return false;
        }
        let app_id = app_id(program);
        let window = WindowInfo {
            id: WindowId(0),
            title: String::new(),
            content: selection::classify(&app_id, ""),
            app_id,
            width: 0,
            height: 0,
            focused: false,
            owner: None,
            kind: windowcast_protocol::WindowKind::Normal,
            position: None,
        };
        let rules = self.rules.lock().expect("rules");
        selection::choose_backend(&window, &rules) == BackendKind::Rdp
    }

    /// Logs in to the host's Remote Desktop at `target` and runs `argv`
    /// there, returning once Windows started it (or failed to).
    pub(crate) fn start_remote_app(
        &self,
        argv: &[String],
        target: HandoffTarget,
        sign_in_password: bool,
        typed: Option<&str>,
    ) -> Result<(), ClientError> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| ClientError::Refused("no application".into()))?;
        let ip = match (
            target.address.parse::<std::net::IpAddr>(),
            self.shared.host_ip,
        ) {
            (Ok(ip), _) => ip,
            (Err(_), Some(ip)) if target.address.is_empty() => ip,
            _ => {
                return Err(ClientError::Refused(
                    "the host's Remote Desktop is not reachable from here".into(),
                ))
            }
        };
        let kept = sign_in_password
            .then(|| {
                self.shared
                    .sign_in_password
                    .lock()
                    .expect("password")
                    .clone()
            })
            .flatten();
        let (password, ask_again) = if !target.password.is_empty() {
            (target.password.clone(), false)
        } else if let Some(typed) = typed {
            (typed.to_owned(), false)
        } else if let Some(kept) = kept {
            (kept, true)
        } else {
            return Err(ClientError::PasswordNeeded(target.username));
        };
        let (domain, user) = match target.username.split_once('\\') {
            Some((domain, user)) => (Some(domain.to_owned()), user.to_owned()),
            None => (None, target.username.clone()),
        };
        let stream = windowcast_rdp::client::connect(&ClientConfig {
            address: std::net::SocketAddr::new(ip, target.port),
            server_name: ip.to_string(),
            username: user,
            password,
            domain,
            size: DESKTOP,
            pinned: target.certificate_sha256,
            remote_app: Some(RemoteApp {
                program: program.clone(),
                arguments: args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" "),
                working_dir: String::new(),
            }),
        })
        .map_err(|e| {
            // The password signed in with is not this Windows user's: ask.
            if ask_again {
                ClientError::PasswordNeeded(target.username.clone())
            } else {
                ClientError::Refused(format!("Remote Desktop: {e}"))
            }
        })?;
        let stream = Arc::new(stream);
        let started = Instant::now();
        while started.elapsed() < START_WITHIN {
            match stream.exec_result() {
                Some(0) => break,
                Some(code) => {
                    return Err(ClientError::Refused(format!(
                        "Windows could not start {program} as a RemoteApp (error {code}); \
                         it may need to be on the host's RemoteApp list"
                    )))
                }
                None if stream.ended.load(Ordering::SeqCst) => {
                    return Err(ClientError::Refused(
                        "Remote Desktop ended the session before the program started".into(),
                    ))
                }
                None => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        let key = self.shared.remote_keys.fetch_add(1, Ordering::SeqCst) & 0x7fff_ffff;
        self.shared
            .remote_apps
            .lock()
            .expect("remote apps")
            .push(Conn {
                key,
                app_id: app_id(program),
                stream: Arc::clone(&stream),
                listed: Vec::new(),
            });
        let shared = Arc::downgrade(&self.shared);
        let host_windows = Arc::clone(&self.windows);
        std::thread::spawn(move || watch(shared, host_windows, key, stream));
        Ok(())
    }

    /// Starts showing a RemoteApp window: nothing to ask the host.
    pub(crate) fn start_remote_window(&self, window: WindowId) {
        let event = if find(&self.shared, window).is_some() {
            self.shared
                .remote_viewing
                .lock()
                .expect("viewing")
                .insert(window, 0);
            Event::StreamStarted {
                window: window.0,
                backend: BackendKind::Rdp,
                codec: None,
            }
        } else {
            Event::StreamRefused {
                window: window.0,
                reason: "that RemoteApp window is gone".into(),
            }
        };
        self.shared.emit(event);
    }

    pub(crate) fn stop_remote_window(&self, window: WindowId) {
        if self
            .shared
            .remote_viewing
            .lock()
            .expect("viewing")
            .remove(&window)
            .is_some()
        {
            self.shared.emit(Event::StreamStopped { window: window.0 });
        }
    }

    pub(crate) fn remote_input(&self, window: WindowId, event: InputEvent) -> bool {
        match find(&self.shared, window) {
            Some((stream, id, _)) => stream.input_to(id, event),
            None => false,
        }
    }

    pub(crate) fn remote_latest_picture(&self, window: WindowId) -> Option<RgbaPicture> {
        let (stream, _, rect) = find(&self.shared, window)?;
        Some(stream.latest_picture()?.cut(rect))
    }

    pub(crate) fn remote_next_picture(&self, window: WindowId, timeout: Duration) -> PicturePoll {
        let Some((stream, _, rect)) = find(&self.shared, window) else {
            return PicturePoll::Ended;
        };
        let seen = self
            .shared
            .remote_viewing
            .lock()
            .expect("viewing")
            .get(&window)
            .copied()
            .unwrap_or(0);
        match stream.picture_after(seen, timeout) {
            Some((number, desktop)) => {
                self.shared
                    .remote_viewing
                    .lock()
                    .expect("viewing")
                    .insert(window, number);
                PicturePoll::Picture(desktop.cut(rect))
            }
            None if stream.ended.load(Ordering::SeqCst) => PicturePoll::Ended,
            None => PicturePoll::Timeout,
        }
    }
}

/// Follows one RemoteApp: its windows into the window list, and its end
/// (the session ended, or the program's last window closed).
fn watch(
    shared: Weak<Shared>,
    host_windows: Arc<Mutex<Vec<WindowInfo>>>,
    key: u32,
    stream: Arc<RdpStream>,
) {
    let mut number = 0;
    let mut had_windows = false;
    let mut empty_since: Option<Instant> = None;
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let (now, listed) = stream.windows();
        let changed = now != number;
        if changed {
            number = now;
            had_windows |= !listed.is_empty();
            empty_since = (had_windows && listed.is_empty()).then(Instant::now);
            if let Some(conn) = shared
                .remote_apps
                .lock()
                .expect("remote apps")
                .iter_mut()
                .find(|c| c.key == key)
            {
                conn.listed = listed;
            }
        }
        let ended = stream.ended.load(Ordering::SeqCst)
            || empty_since.is_some_and(|since| since.elapsed() > GONE_AFTER);
        if ended {
            stream.stop();
            shared
                .remote_apps
                .lock()
                .expect("remote apps")
                .retain(|c| c.key != key);
        }
        if changed || ended {
            // Windows being watched that are gone now.
            let gone: Vec<WindowId> = shared
                .remote_viewing
                .lock()
                .expect("viewing")
                .keys()
                .copied()
                .filter(|w| split(*w).is_some_and(|(k, _)| k == key))
                .filter(|w| find(&shared, *w).is_none())
                .collect();
            for window in gone {
                shared
                    .remote_viewing
                    .lock()
                    .expect("viewing")
                    .remove(&window);
                shared.emit(Event::StreamStopped { window: window.0 });
            }
            let mut windows = host_windows.lock().expect("windows").clone();
            windows.extend(infos(&shared));
            shared.emit(Event::Windows { windows });
        }
        if ended {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_app_window_ids_split_back() {
        let id = window_id(5, 0x0001_0106);
        assert!(id.0 & REMOTE_APP_WINDOW != 0);
        assert_eq!(split(id), Some((5, 0x0001_0106)));
        assert_eq!(split(WindowId(0x0012_3456)), None, "a host's own window");
    }

    #[test]
    fn programs_and_arguments_are_named_as_windows_does() {
        assert_eq!(app_id(r"C:\Windows\System32\notepad.exe"), "notepad.exe");
        assert_eq!(app_id("notepad"), "notepad");
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote(r"C:\My Files\a.txt"), "\"C:\\My Files\\a.txt\"");
        assert_eq!(quote(""), "\"\"");
    }
}

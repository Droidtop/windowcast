//! RemoteApps run by the host itself (docs/BACKENDS.md, "RemoteApp"): the
//! host logs in to its own Remote Desktop as the RemoteApp's client and
//! runs the program there, and each window Windows reports joins the
//! host's window list under an id of its own ([`REMOTE_APP_WINDOW`] set).
//! Its pictures are cut from the RemoteApp session's desktop and its input
//! goes to it, so a RemoteApp window streams on any carrier and switches
//! like any other window.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use windowcast_host::audio::{AudioSource, MicrophoneSink};
use windowcast_host::gamepad::GamepadSink;
use windowcast_host::video::EncodedPictures;
use windowcast_host::{
    CursorState, FrameSource, Handoff, Picture, PictureSource, PrivateRefusal, WindowSource,
};
use windowcast_protocol::{
    BackendKind, ContentHint, CursorImage, HostState, InputEvent, VideoCodec, WindowId, WindowInfo,
};

use crate::client::{self, ClientConfig, RdpStream};
use crate::remoteapp::{ListedWindow, Rect, RemoteApp};

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

/// How often a RemoteApp window that does not change is sent again.
const REFRESH: Duration = Duration::from_secs(1);

/// Where and as whom a RemoteApp logs in.
pub struct Login {
    pub address: SocketAddr,
    pub username: String,
    pub domain: Option<String>,
    pub password: String,
    /// Remote Desktop's certificate, read by connecting to it.
    pub certificate: Option<[u8; 32]>,
}

/// Why a RemoteApp did not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// Remote Desktop refused the login (a wrong password, most likely).
    Login(String),
    /// Anything else.
    Other(String),
}

/// One RemoteApp connection.
struct Conn {
    key: u32,
    /// The program's file name, the windows' `app_id`.
    app_id: String,
    stream: Arc<RdpStream>,
    listed: Vec<ListedWindow>,
}

/// The host's running RemoteApps.
#[derive(Default)]
pub struct RemoteAppWindows {
    conns: Mutex<Vec<Conn>>,
    keys: AtomicU32,
}

fn window_id(key: u32, window: u32) -> WindowId {
    WindowId(REMOTE_APP_WINDOW | (u64::from(key) << 32) | u64::from(window))
}

/// The connection key and Windows' window id of a RemoteApp window.
pub fn split(window: WindowId) -> Option<(u32, u32)> {
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

impl RemoteAppWindows {
    /// Logs in with `login` and runs `argv` as a RemoteApp, returning once
    /// Windows started it (or failed to).
    pub fn start(self: &Arc<Self>, argv: &[String], login: Login) -> Result<(), StartError> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| StartError::Other("no application".into()))?;
        let stream = client::connect(&ClientConfig {
            address: login.address,
            server_name: login.address.ip().to_string(),
            username: login.username,
            password: login.password,
            domain: login.domain,
            size: DESKTOP,
            pinned: login.certificate,
            remote_app: Some(RemoteApp {
                program: program.clone(),
                arguments: args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" "),
                working_dir: String::new(),
            }),
        })
        .map_err(|e| StartError::Login(format!("Remote Desktop: {e}")))?;
        let stream = Arc::new(stream);
        let started = Instant::now();
        loop {
            match stream.exec_result() {
                Some(0) => break,
                Some(code) => {
                    stream.stop();
                    return Err(StartError::Other(format!(
                        "Windows could not start {program} as a RemoteApp (error {code}); \
                         it may need to be on the host's RemoteApp list"
                    )));
                }
                None if stream.ended.load(Ordering::SeqCst) => {
                    return Err(StartError::Other(
                        "Remote Desktop ended the session before the program started".into(),
                    ))
                }
                None if started.elapsed() > START_WITHIN => {
                    stream.stop();
                    return Err(StartError::Other(format!(
                        "{program} did not start as a RemoteApp within {} s",
                        START_WITHIN.as_secs()
                    )));
                }
                None => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        let key = self.keys.fetch_add(1, Ordering::SeqCst) & 0x7fff_ffff;
        self.conns.lock().expect("remote apps").push(Conn {
            key,
            app_id: app_id(program),
            stream: Arc::clone(&stream),
            listed: Vec::new(),
        });
        let apps = Arc::downgrade(self);
        std::thread::spawn(move || watch(apps, key, stream));
        Ok(())
    }

    /// The RemoteApp windows, as window list entries.
    pub fn infos(&self) -> Vec<WindowInfo> {
        let conns = self.conns.lock().expect("remote apps");
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

    /// The connection and rectangle of a RemoteApp window listed now.
    fn find(&self, window: WindowId) -> Option<(Arc<RdpStream>, u32, Rect)> {
        let (key, id) = split(window)?;
        let conns = self.conns.lock().expect("remote apps");
        let conn = conns.iter().find(|c| c.key == key)?;
        let rect = conn.listed.iter().find(|w| w.id == id)?.rect;
        Some((Arc::clone(&conn.stream), id, rect))
    }
}

impl Drop for RemoteAppWindows {
    fn drop(&mut self) {
        for conn in self.conns.lock().expect("remote apps").iter() {
            conn.stream.stop();
        }
    }
}

/// Follows one RemoteApp: its windows into the list, and its end (the
/// session ended, or the program's last window closed).
fn watch(apps: Weak<RemoteAppWindows>, key: u32, stream: Arc<RdpStream>) {
    let mut number = 0;
    let mut had_windows = false;
    let mut empty_since: Option<Instant> = None;
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let Some(apps) = apps.upgrade() else {
            stream.stop();
            return;
        };
        let (now, listed) = stream.windows();
        if now != number {
            number = now;
            had_windows |= !listed.is_empty();
            empty_since = (had_windows && listed.is_empty()).then(Instant::now);
            if let Some(conn) = apps
                .conns
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
            apps.conns
                .lock()
                .expect("remote apps")
                .retain(|c| c.key != key);
            return;
        }
    }
}

/// A RemoteApp window's pictures: cut from its session's desktop, when it
/// changes and once a second regardless.
struct RemotePictures {
    apps: Weak<RemoteAppWindows>,
    window: WindowId,
    seen: u64,
}

impl PictureSource for RemotePictures {
    fn next_picture(&mut self) -> Option<Picture> {
        let apps = self.apps.upgrade()?;
        let (stream, _, rect) = apps.find(self.window)?;
        drop(apps);
        let desktop = match stream.picture_after(self.seen, REFRESH) {
            Some((number, desktop)) => {
                self.seen = number;
                desktop
            }
            None if stream.ended.load(Ordering::SeqCst) => return None,
            None => stream.latest_picture()?,
        };
        let cut = desktop.cut(rect);
        // RGBA to the BGRA hosts hand out.
        let mut data = cut.data;
        for pixel in data.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        Some(Picture {
            width: cut.width,
            height: cut.height,
            stride: cut.width as usize * 4,
            data,
        })
    }
}

/// Another source plus the host's RemoteApp windows: listed with its own,
/// carried like them (encoded with OpenH264 on video carriers, as pictures
/// on RDP), and taking input.
pub struct WithRemoteApps {
    inner: Arc<dyn WindowSource>,
    apps: Arc<RemoteAppWindows>,
}

impl WithRemoteApps {
    pub fn new(inner: Arc<dyn WindowSource>, apps: Arc<RemoteAppWindows>) -> Self {
        WithRemoteApps { inner, apps }
    }

    fn pictures(&self, window: WindowId) -> Result<RemotePictures, String> {
        self.apps
            .find(window)
            .ok_or_else(|| "that RemoteApp window is gone".to_owned())?;
        Ok(RemotePictures {
            apps: Arc::downgrade(&self.apps),
            window,
            seen: 0,
        })
    }

    fn encoded(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        if codec != VideoCodec::H264 {
            return Err("RemoteApp windows stream as H.264".into());
        }
        Ok(Box::new(EncodedPictures::new(Box::new(
            self.pictures(window)?,
        ))))
    }
}

/// The window an input event is for: the one it names, or the focus.
fn input_window(event: &InputEvent, focus: Option<WindowId>) -> Option<WindowId> {
    match event {
        InputEvent::PointerMove { window, .. }
        | InputEvent::PointerButton { window, .. }
        | InputEvent::PointerScroll { window, .. }
        | InputEvent::Touch { window, .. } => Some(*window),
        _ => focus,
    }
}

impl WindowSource for WithRemoteApps {
    fn list_windows(&self) -> Vec<WindowInfo> {
        let mut windows = self.inner.list_windows();
        windows.extend(self.apps.infos());
        windows
    }
    fn encoders(&self) -> Vec<VideoCodec> {
        self.inner.encoders()
    }
    fn backends(&self) -> Vec<BackendKind> {
        self.inner.backends()
    }
    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        if split(window).is_some() {
            return self.encoded(window, codec);
        }
        self.inner.open(window, codec)
    }
    fn open_desktop(
        &self,
        window: WindowId,
        codec: VideoCodec,
    ) -> Result<Box<dyn FrameSource>, String> {
        if split(window).is_some() {
            // Its own picture is all a RemoteApp window has.
            return self.encoded(window, codec);
        }
        self.inner.open_desktop(window, codec)
    }
    fn handoff(&self, kind: BackendKind, window: WindowId) -> Option<Result<Handoff, String>> {
        self.inner.handoff(kind, window)
    }
    fn open_pictures(&self, window: WindowId) -> Option<Result<Box<dyn PictureSource>, String>> {
        if split(window).is_some() {
            return Some(self.pictures(window).map(|p| Box::new(p) as _));
        }
        self.inner.open_pictures(window)
    }
    fn open_audio(&self, window: WindowId) -> Option<Result<Box<dyn AudioSource>, String>> {
        if split(window).is_some() {
            return None;
        }
        self.inner.open_audio(window)
    }
    fn microphone(&self) -> Option<Result<Box<dyn MicrophoneSink>, String>> {
        self.inner.microphone()
    }
    fn input(&self, event: &InputEvent, focus: Option<WindowId>) {
        match input_window(event, focus).filter(|w| split(*w).is_some()) {
            Some(window) => {
                if let Some((stream, id, _)) = self.apps.find(window) {
                    stream.input_to(id, event.clone());
                }
            }
            None => self.inner.input(event, focus),
        }
    }
    fn gamepads(&self) -> Option<Result<Box<dyn GamepadSink>, String>> {
        self.inner.gamepads()
    }
    fn clipboard(&self) -> Option<(u64, String)> {
        self.inner.clipboard()
    }
    fn set_clipboard(&self, text: &str) {
        self.inner.set_clipboard(text)
    }
    fn host_state(&self) -> HostState {
        self.inner.host_state()
    }
    fn set_private_session(
        &self,
        start: bool,
        password: Option<&str>,
    ) -> Result<(), PrivateRefusal> {
        self.inner.set_private_session(start, password)
    }
    fn cursor(&self) -> Option<CursorState> {
        self.inner.cursor()
    }
    fn cursor_image(&self, shape: u64) -> Option<CursorImage> {
        self.inner.cursor_image(shape)
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

    #[test]
    fn keys_go_to_the_focus_and_pointers_to_their_window() {
        let remote = window_id(1, 2);
        let key = InputEvent::Key {
            keycode: 30,
            pressed: true,
        };
        assert_eq!(input_window(&key, Some(remote)), Some(remote));
        let click = InputEvent::PointerMove {
            window: WindowId(7),
            x: 0.5,
            y: 0.5,
        };
        assert_eq!(input_window(&click, Some(remote)), Some(WindowId(7)));
    }
}

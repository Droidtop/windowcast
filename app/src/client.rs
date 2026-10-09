//! The client role: client-core's Rust API, one session at a time. Each
//! streamed window opens a native stream window of its own (on Windows,
//! client-windows: hardware decoding and a flip-model swap chain); the
//! client window shows the controls and each stream's statistics.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windowcast_client::{Client, ClientSession, Event};
use windowcast_client_windows::{Placement, Shared, StreamStats};
use windowcast_protocol::selection::{self, BackendRule, WindowMatch};
use windowcast_protocol::{BackendKind, VideoCodec, WindowId, WindowInfo};

use crate::config::{SavedHost, Store};

/// How many recent session events the client window shows.
const LOG_LINES: usize = 30;

struct Stream {
    requested: Option<BackendKind>,
    backend: Option<BackendKind>,
    codec: Option<VideoCodec>,
    refused: Option<String>,
    shared: Arc<Shared>,
    #[cfg(windows)]
    window: Option<windowcast_client_windows::StreamWindow>,
}

impl Stream {
    fn new(requested: Option<BackendKind>) -> Self {
        Stream {
            requested,
            backend: None,
            codec: None,
            refused: None,
            shared: Arc::default(),
            #[cfg(windows)]
            window: None,
        }
    }

    fn close_window(&mut self) {
        #[cfg(windows)]
        if let Some(mut window) = self.window.take() {
            window.close();
        }
    }
}

#[derive(Default)]
struct State {
    session: Option<Arc<ClientSession>>,
    /// Bumped per connection, so the event thread of an old session stops.
    generation: u64,
    address: Option<String>,
    connecting: bool,
    error: Option<String>,
    windows: Vec<WindowInfo>,
    streams: HashMap<u64, Stream>,
    log: VecDeque<String>,
    clipboard: Option<String>,
}

/// One of the host's windows, as the client window lists it.
#[derive(Clone)]
pub struct ClientWindow {
    pub info: WindowInfo,
    pub default_backend: BackendKind,
    /// The user's rule for this app, if any.
    pub rule: Option<BackendKind>,
    pub streaming: bool,
    pub refused: Option<String>,
}

#[derive(Clone)]
pub struct ClientStream {
    pub title: String,
    pub requested: Option<BackendKind>,
    pub backend: Option<BackendKind>,
    pub codec: Option<VideoCodec>,
    pub stats: StreamStats,
}

/// Everything the client window shows.
#[derive(Clone, Default)]
pub struct ClientSnapshot {
    pub identity: String,
    pub address: Option<String>,
    pub host_id: Option<String>,
    pub paired: bool,
    pub rtt_ms: Option<f64>,
    pub connecting: bool,
    pub error: Option<String>,
    pub windows: Vec<ClientWindow>,
    pub streams: Vec<ClientStream>,
    pub log: Vec<String>,
    pub clipboard: Option<String>,
}

/// Stream window settings for this run only (command-line options), over
/// the saved ones.
#[derive(Clone, Default)]
pub struct Overrides {
    pub fullscreen: Option<bool>,
    pub display: Option<usize>,
    pub codec: Option<String>,
}

pub struct ClientRole {
    client: Client,
    store: Arc<Store>,
    state: Mutex<State>,
    overrides: Mutex<Overrides>,
}

impl ClientRole {
    pub fn start(store: Arc<Store>, data_dir: &std::path::Path) -> Result<Arc<Self>, String> {
        let client = Client::new(&data_dir.join("client")).map_err(|e| e.to_string())?;
        Ok(Arc::new(ClientRole {
            client,
            store,
            state: Mutex::new(State::default()),
            overrides: Mutex::default(),
        }))
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn set_overrides(&self, overrides: Overrides) {
        *self.overrides.lock().expect("overrides") = overrides;
    }

    /// The saved client settings with this run's overrides applied.
    fn settings(&self) -> crate::config::ClientSettings {
        let mut settings = self.store.get().client;
        let overrides = self.overrides.lock().expect("overrides").clone();
        if let Some(fullscreen) = overrides.fullscreen {
            settings.fullscreen = fullscreen;
        }
        if let Some(display) = overrides.display {
            settings.display = display;
        }
        if let Some(codec) = overrides.codec {
            settings.codec = codec;
        }
        settings
    }

    fn session(&self) -> Result<Arc<ClientSession>, String> {
        self.state
            .lock()
            .expect("state")
            .session
            .clone()
            .ok_or_else(|| "not connected to a host".to_owned())
    }

    fn log(&self, line: String) {
        let mut state = self.state.lock().expect("state");
        state.log.push_back(line);
        while state.log.len() > LOG_LINES {
            state.log.pop_front();
        }
    }

    /// Connects on a thread of its own: pairs with `pin` if given, else
    /// resumes with the pinned identity. Replaces any current session.
    pub fn connect_in_background(self: &Arc<Self>, address: String, pin: Option<String>) {
        let role = Arc::clone(self);
        std::thread::spawn(move || {
            let _ = role.connect(&address, pin.as_deref());
        });
    }

    /// Connects (blocking).
    pub fn connect(self: &Arc<Self>, address: &str, pin: Option<&str>) -> Result<(), String> {
        self.disconnect();
        self.state.lock().expect("state").connecting = true;
        let result = self.client.connect(address, pin);
        let mut state = self.state.lock().expect("state");
        state.connecting = false;
        let session = match result {
            Ok(session) => Arc::new(session),
            Err(e) => {
                let message = format!("could not connect to {address}: {e}");
                state.error = Some(message.clone());
                return Err(message);
            }
        };
        session.set_rules(self.store.get().client.rules);
        state.generation += 1;
        state.session = Some(Arc::clone(&session));
        state.address = Some(address.to_owned());
        state.error = None;
        state.windows.clear();
        let generation = state.generation;
        drop(state);

        let host_id = session.host_id().to_owned();
        self.log(format!(
            "{} {address} (host {})",
            if session.paired() {
                "paired with"
            } else {
                "resumed with"
            },
            &host_id[..12.min(host_id.len())]
        ));
        self.store.update(|config| {
            let saved = &mut config.client.saved;
            saved.retain(|host| host.host_id != host_id);
            saved.insert(
                0,
                SavedHost {
                    address: address.to_owned(),
                    host_id: host_id.clone(),
                },
            );
        });
        let _ = session.request_windows();
        let role = Arc::clone(self);
        std::thread::spawn(move || role.pump_events(session, generation));
        Ok(())
    }

    pub fn disconnect(&self) {
        let (session, mut streams) = {
            let mut state = self.state.lock().expect("state");
            state.generation += 1;
            state.address = None;
            (state.session.take(), std::mem::take(&mut state.streams))
        };
        for stream in streams.values_mut() {
            stream.close_window();
        }
        if let Some(session) = session {
            session.close();
            self.log("disconnected".into());
        }
    }

    pub fn forget(&self, host_id: &str) -> Result<(), String> {
        self.client.forget(host_id).map_err(|e| e.to_string())?;
        self.store
            .update(|config| config.client.saved.retain(|host| host.host_id != host_id));
        Ok(())
    }

    /// The session's events and round-trip pings, until it closes or is
    /// replaced; also stops streams whose window the user closed.
    fn pump_events(self: Arc<Self>, session: Arc<ClientSession>, generation: u64) {
        let mut last_ping = Instant::now() - Duration::from_secs(5);
        loop {
            if self.state.lock().expect("state").generation != generation {
                return;
            }
            if last_ping.elapsed() >= Duration::from_secs(1) {
                let _ = session.ping();
                last_ping = Instant::now();
            }
            self.stop_closed_windows(&session);
            let Some(event) = session.next_event(Duration::from_millis(250)) else {
                continue;
            };
            let mut state = self.state.lock().expect("state");
            if state.generation != generation {
                return;
            }
            match event {
                Event::Windows { windows } => state.windows = windows,
                Event::StreamStarted {
                    window,
                    backend,
                    codec,
                } => {
                    let title = state
                        .windows
                        .iter()
                        .find(|info| info.id.0 == window)
                        .map_or_else(|| format!("window {window}"), |info| info.title.clone());
                    let stream = state
                        .streams
                        .entry(window)
                        .or_insert_with(|| Stream::new(None));
                    stream.backend = Some(backend);
                    stream.codec = codec;
                    stream.refused = None;
                    let line = format!(
                        "{title}: streaming over {backend:?}{}",
                        codec.map(|c| format!(" in {c:?}")).unwrap_or_default()
                    );
                    if let Some(codec) = codec {
                        self.open_window(&session, stream, window, codec, title);
                    }
                    drop(state);
                    self.log(line);
                }
                Event::StreamRefused { window, reason } => {
                    if let Some(stream) = state.streams.get_mut(&window) {
                        stream.refused = Some(reason.clone());
                    }
                    drop(state);
                    self.log(format!("window {window}: refused: {reason}"));
                }
                Event::StreamStopped { window } => {
                    let stream = state.streams.remove(&window);
                    drop(state);
                    if let Some(mut stream) = stream {
                        stream.close_window();
                    }
                    self.log(format!("window {window}: stream stopped"));
                }
                Event::WindowResized { .. } | Event::WindowFocused { .. } => {}
                Event::Clipboard { text } => state.clipboard = Some(text),
                Event::Closed => {
                    state.session = None;
                    let mut streams = std::mem::take(&mut state.streams);
                    state.error = Some("the host closed the session".into());
                    drop(state);
                    for stream in streams.values_mut() {
                        stream.close_window();
                    }
                    self.log("session closed".into());
                    return;
                }
            }
        }
    }

    #[cfg(windows)]
    fn open_window(
        &self,
        session: &Arc<ClientSession>,
        stream: &mut Stream,
        window: u64,
        codec: VideoCodec,
        title: String,
    ) {
        let config = self.settings();
        stream.close_window();
        stream.shared = Arc::default();
        stream
            .shared
            .send_input
            .store(config.send_input, Ordering::SeqCst);
        let placement = Placement {
            fullscreen: config.fullscreen,
            display: (config.display > 0).then_some(config.display as u32),
        };
        stream.window = Some(windowcast_client_windows::open(
            Arc::clone(session),
            WindowId(window),
            codec,
            title,
            placement,
            Arc::clone(&stream.shared),
        ));
    }

    #[cfg(not(windows))]
    fn open_window(
        &self,
        _: &Arc<ClientSession>,
        stream: &mut Stream,
        _: u64,
        _: VideoCodec,
        _: String,
    ) {
        let _ = (Placement::default(), Ordering::SeqCst);
        let mut stats = stream.shared.stats.lock().expect("stats");
        stats.error = Some("this platform has no native stream window yet".into());
        stats.closed = true;
    }

    /// A stream whose window was closed (by the user, or an error) is
    /// stopped on the host.
    fn stop_closed_windows(&self, session: &ClientSession) {
        let closed: Vec<u64> = {
            let state = self.state.lock().expect("state");
            state
                .streams
                .iter()
                .filter(|(_, stream)| {
                    stream.backend.is_some() && stream.shared.stats.lock().expect("stats").closed
                })
                .map(|(window, _)| *window)
                .collect()
        };
        for window in closed {
            let stream = self.state.lock().expect("state").streams.remove(&window);
            if let Some(mut stream) = stream {
                if let Some(error) = stream.shared.stats.lock().expect("stats").error.clone() {
                    self.log(format!("window {window}: {error}"));
                }
                stream.close_window();
            }
            let _ = session.stop_window(WindowId(window));
        }
    }

    pub fn refresh(&self) -> Result<(), String> {
        self.session()?.request_windows().map_err(|e| e.to_string())
    }

    /// The codecs to ask for: the user's choice, or every codec this PC
    /// decodes, best first.
    pub fn codecs(&self) -> Vec<VideoCodec> {
        let decodable = windowcast_client_windows::decodable();
        match self.settings().codec.as_str() {
            "H264" => vec![VideoCodec::H264],
            "H265" => vec![VideoCodec::H265],
            "Av1" => vec![VideoCodec::Av1],
            _ if decodable.is_empty() => vec![VideoCodec::H264],
            _ => decodable,
        }
    }

    /// Streams `window`. The backend comes from the user's rules, then the
    /// defaults.
    pub fn start_stream(&self, window: u64) -> Result<(), String> {
        let session = self.session()?;
        {
            let mut state = self.state.lock().expect("state");
            let rules = self.store.get().client.rules;
            let requested = state
                .windows
                .iter()
                .find(|info| info.id.0 == window)
                .map(|info| selection::choose_backend(info, &rules));
            if let Some(mut old) = state.streams.insert(window, Stream::new(requested)) {
                old.close_window();
            }
        }
        session
            .start_window(WindowId(window), &self.codecs())
            .map_err(|e| e.to_string())
    }

    pub fn stop_stream(&self, window: u64) -> Result<(), String> {
        let stream = self.state.lock().expect("state").streams.remove(&window);
        if let Some(mut stream) = stream {
            stream.close_window();
        }
        self.session()?
            .stop_window(WindowId(window))
            .map_err(|e| e.to_string())
    }

    /// Sets (or with `None` clears) the user's backend rule for one app.
    pub fn set_rule(&self, app_id: &str, backend: Option<BackendKind>) {
        self.store.update(|config| {
            let rules = &mut config.client.rules;
            rules.retain(|rule| {
                !rule
                    .when
                    .app_id
                    .as_deref()
                    .is_some_and(|id| id.eq_ignore_ascii_case(app_id))
            });
            if let Some(backend) = backend {
                rules.push(BackendRule {
                    when: WindowMatch {
                        app_id: Some(app_id.to_owned()),
                        ..Default::default()
                    },
                    backend,
                });
            }
        });
        if let Ok(session) = self.session() {
            session.set_rules(self.store.get().client.rules);
        }
    }

    pub fn set_send_input(&self, on: bool) {
        self.store.update(|config| config.client.send_input = on);
        for stream in self.state.lock().expect("state").streams.values() {
            stream.shared.send_input.store(on, Ordering::SeqCst);
        }
    }

    pub fn set_clipboard(&self, text: &str) -> Result<(), String> {
        self.session()?
            .set_clipboard(text)
            .map_err(|e| e.to_string())
    }

    pub fn snapshot(&self) -> ClientSnapshot {
        let rules = self.store.get().client.rules;
        let state = self.state.lock().expect("state");
        let session = state.session.as_ref();
        let windows = state
            .windows
            .iter()
            .map(|info| {
                let stream = state.streams.get(&info.id.0);
                ClientWindow {
                    default_backend: selection::choose_backend(info, &[]),
                    rule: rules
                        .iter()
                        .find(|rule| rule.when.app_id.is_some() && rule.when.matches(info))
                        .map(|rule| rule.backend),
                    streaming: stream.is_some_and(|s| s.refused.is_none()),
                    refused: stream.and_then(|s| s.refused.clone()),
                    info: info.clone(),
                }
            })
            .collect();
        let streams = state
            .streams
            .iter()
            .map(|(window, stream)| ClientStream {
                title: state
                    .windows
                    .iter()
                    .find(|info| info.id.0 == *window)
                    .map_or_else(|| format!("window {window}"), |info| info.title.clone()),
                requested: stream.requested,
                backend: stream.backend,
                codec: stream.codec,
                stats: stream.shared.stats.lock().expect("stats").clone(),
            })
            .collect();
        ClientSnapshot {
            identity: self.client.peer_id(),
            address: state.address.clone(),
            host_id: session.map(|s| s.host_id().to_owned()),
            paired: session.is_some_and(|s| s.paired()),
            rtt_ms: session
                .and_then(|s| s.round_trip())
                .map(|d| d.as_secs_f64() * 1000.0),
            connecting: state.connecting,
            error: state.error.clone(),
            windows,
            streams,
            log: state.log.iter().cloned().collect(),
            clipboard: state.clipboard.clone(),
        }
    }

    /// Whether a stream is live, for a command-line stream that ends when
    /// its window closes.
    pub fn streaming(&self) -> bool {
        self.state
            .lock()
            .expect("state")
            .streams
            .values()
            .any(|stream| stream.refused.is_none())
    }

    /// Waits up to `timeout` for the host's window list and returns it.
    pub fn wait_for_windows(&self, timeout: Duration) -> Vec<WindowInfo> {
        let start = Instant::now();
        loop {
            let windows = self.state.lock().expect("state").windows.clone();
            if !windows.is_empty() || start.elapsed() >= timeout {
                return windows;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

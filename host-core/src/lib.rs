//! The host side of the windowcast library: everything a host agent does
//! that is not specific to its operating system. An agent implements
//! [`WindowSource`] (list windows, capture and encode one) and calls
//! [`run`]; this crate does the rest: listening, pairing by PIN and the
//! PIN lockout, the trusted-client list, answering window lists, choosing
//! each stream's backend and codec, attaching window tracks and feeding
//! them frames, and keyframe requests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use windowcast_identity::{Identity, PeerId, TrustStore};
use windowcast_protocol::selection;
use windowcast_protocol::{
    BackendKind, ControlMessage, InputEvent, StreamBackend, StreamOptions, StreamTarget,
    VideoCodec, WindowId, WindowInfo,
};
use windowcast_transport::{accept, HostCredential, Session, TransportError, WindowTrack};

/// The default signaling address.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:47100";

/// Failed pairing attempts before the PIN is withdrawn, and how long the
/// host waits before it shows a new one. SPAKE2 allows one PIN guess per
/// connection; this caps the rate too, so a LAN peer cannot walk the PIN
/// space (three guesses a minute at most).
const MAX_PAIRING_FAILURES: u32 = 3;
const PAIRING_LOCKOUT: Duration = Duration::from_secs(60);

/// One encoded access unit from a [`FrameSource`].
pub struct EncodedFrame {
    /// Annex-B for H.264/H.265, low-overhead OBUs for AV1.
    pub data: Vec<u8>,
    /// How long the frame is shown.
    pub duration: Duration,
    /// Set on the first frame at a new picture size (and on the first
    /// frame of all); the client is told before the frame is sent.
    pub size: Option<(u32, u32)>,
}

/// What an agent provides: its windows, and a capture-plus-encoder per
/// streamed window.
pub trait WindowSource: Send + Sync + 'static {
    /// The windows open right now. Each carries its content hint
    /// ([`selection::classify`]).
    fn list_windows(&self) -> Vec<WindowInfo>;

    /// Codecs this host can encode, most preferred first.
    fn encoders(&self) -> Vec<VideoCodec>;

    /// Backends this host can serve. Native is always available.
    fn backends(&self) -> Vec<BackendKind> {
        vec![BackendKind::Native]
    }

    /// Starts capturing and encoding `window` in `codec`. The error text is
    /// passed to the client as the refusal reason.
    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String>;

    /// Delivers one input event. `focus` is the session's input focus: the
    /// last streamed window an event named, where keys, text and gamepads
    /// go. Called on the session's input thread, in order.
    fn input(&self, _event: &InputEvent, _focus: Option<WindowId>) {}

    /// The clipboard's text and a counter that changes whenever the
    /// clipboard does; `None` when this host does not share its clipboard.
    fn clipboard(&self) -> Option<(u64, String)> {
        None
    }

    /// Replaces the clipboard's text with the client's.
    fn set_clipboard(&self, _text: &str) {}
}

/// A running capture and encoder for one window. Runs on its own thread.
pub trait FrameSource: Send {
    /// Blocks until the next frame is encoded, paced by the source. With
    /// `keyframe` set, that frame must be a keyframe with its parameter
    /// sets. `None` ends the stream (the window closed).
    fn next_frame(&mut self, keyframe: bool) -> Option<EncodedFrame>;
}

pub struct HostConfig {
    /// Signaling address, `ADDR:PORT`.
    pub listen: String,
    /// Whether a PIN is shown for new clients to pair.
    pub pairing: bool,
    /// Where the host's identity and trusted-client list live.
    pub data_dir: PathBuf,
}

/// The host's pairing PIN, withdrawn after repeated failures and re-issued
/// after the lockout. Shown on standard output, the one place a host shows
/// it today; a tray app (#116) will read it from here.
struct Pairing {
    pin: Mutex<Option<String>>,
    failures: AtomicU32,
}

impl Pairing {
    fn new(open: bool) -> Arc<Self> {
        let pairing = Arc::new(Pairing {
            pin: Mutex::new(None),
            failures: AtomicU32::new(0),
        });
        if open {
            let pin = windowcast_pairing::generate_pin();
            show_pin(&pin);
            *pairing.pin.try_lock().expect("new pairing state") = Some(pin);
        } else {
            println!("pairing closed; only clients paired earlier can connect");
        }
        pairing
    }

    async fn current(&self) -> Option<String> {
        self.pin.lock().await.clone()
    }

    async fn paired(&self) {
        *self.pin.lock().await = None;
        self.failures.store(0, Ordering::SeqCst);
    }

    /// Counts a failed attempt; the third withdraws the PIN, and a new one
    /// is shown after [`PAIRING_LOCKOUT`].
    async fn failed(self: &Arc<Self>) {
        let mut pin = self.pin.lock().await;
        if pin.is_none() {
            return;
        }
        let failures = self.failures.fetch_add(1, Ordering::SeqCst) + 1;
        if failures < MAX_PAIRING_FAILURES {
            return;
        }
        *pin = None;
        eprintln!(
            "pairing paused after {failures} failed attempts; a new PIN follows in {} s",
            PAIRING_LOCKOUT.as_secs()
        );
        let pairing = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(PAIRING_LOCKOUT).await;
            let new_pin = windowcast_pairing::generate_pin();
            show_pin(&new_pin);
            *pairing.pin.lock().await = Some(new_pin);
            pairing.failures.store(0, Ordering::SeqCst);
        });
    }
}

fn show_pin(pin: &str) {
    println!("pairing PIN (enter this on the client): {pin}");
}

struct Host {
    identity: Identity,
    trust: Mutex<TrustStore>,
    trust_path: PathBuf,
    pairing: Arc<Pairing>,
    source: Arc<dyn WindowSource>,
}

/// Runs a host on `config.listen` until the listener fails.
pub async fn run(config: HostConfig, source: Arc<dyn WindowSource>) -> std::io::Result<()> {
    let listener = TcpListener::bind(&config.listen).await?;
    println!("listening on {}", config.listen);
    serve(listener, config, source).await
}

/// Runs a host on an already bound listener (`config.listen` is unused).
pub async fn serve(
    listener: TcpListener,
    config: HostConfig,
    source: Arc<dyn WindowSource>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    let identity = Identity::load_or_generate(&config.data_dir.join("agent-identity.key"))
        .map_err(std::io::Error::other)?;
    let trust_path = config.data_dir.join("agent-trusted-clients");
    let trust = TrustStore::load(&trust_path).map_err(std::io::Error::other)?;
    println!("host identity: {}", identity.peer_id());

    let host = Arc::new(Host {
        identity,
        trust: Mutex::new(trust),
        trust_path,
        pairing: Pairing::new(config.pairing),
        source,
    });

    loop {
        let (stream, address) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                eprintln!("accept failed: {e}");
                continue;
            }
        };
        let host = Arc::clone(&host);
        tokio::spawn(async move {
            match host.serve(stream).await {
                Ok(()) | Err(TransportError::Closed) => println!("{address}: session ended"),
                Err(TransportError::AuthenticationFailed) => {
                    eprintln!("{address}: authentication failed");
                    host.pairing.failed().await;
                }
                Err(e) => eprintln!("{address}: {e}"),
            }
        });
    }
}

/// One stream being fed: dropping it stops its frame thread.
struct Stream {
    stop: Arc<AtomicBool>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Host {
    async fn serve(
        self: &Arc<Self>,
        stream: TcpStream,
        address: String,
    ) -> Result<(), TransportError> {
        // A host that only listens on loopback opens nothing else either.
        let local = stream
            .local_addr()
            .is_ok_and(|local| local.ip().is_loopback());
        let session = if local {
            Session::local_only().await?
        } else {
            Session::new().await?
        };
        let control = &self.control;
        let trusted = control.trust.lock().await.clone();
        let pin = control.pairing.current().await;
        let established = accept(
            stream,
            session,
            &control.identity,
            HostCredential {
                pin: pin.as_deref(),
                trusted: &trusted,
            },
        )
        .await?;

        if established.paired {
            self.pairing.paired().await;
            self.pin_client(established.peer).await;
            println!("paired with {}; pairing is now closed", established.peer);
        } else {
            println!("{} connected", established.peer);
        }

        let session = Arc::new(established.session);
        let mut streams = std::collections::HashMap::<WindowId, Stream>::new();
        let input = self.input_thread();
        let mut focus: Option<WindowId> = None;
        let clipboard = Arc::new(std::sync::Mutex::new(None::<String>));
        let clipboard_task = tokio::spawn(share_clipboard(
            Arc::clone(&self.source),
            Arc::clone(&session),
            Arc::clone(&clipboard),
        ));
        let _clipboard_task = AbortOnDrop(clipboard_task);
        loop {
            match session.recv_control().await? {
                ControlMessage::Input(event) => {
                    // Only windows this session streams take input; the
                    // client cannot reach any other window on the host.
                    match input_window(&event) {
                        Some(window) if streams.contains_key(&window) => focus = Some(window),
                        Some(_) => continue,
                        None if focus.is_none_or(|f| !streams.contains_key(&f)) => continue,
                        None => {}
                    }
                    let _ = input.send((event, focus));
                }
                ControlMessage::Clipboard(text) => {
                    *clipboard.lock().expect("clipboard") = Some(text.clone());
                    let source = Arc::clone(&self.source);
                    tokio::task::spawn_blocking(move || source.set_clipboard(&text));
                }
                ControlMessage::ListWindowsRequest => {
                    let source = Arc::clone(&self.source);
                    let windows = tokio::task::spawn_blocking(move || source.list_windows())
                        .await
                        .unwrap_or_default();
                    session
                        .send_control(&ControlMessage::ListWindowsResponse(windows))
                        .await?;
                }
                ControlMessage::StreamStartRequest {
                    target: StreamTarget::Window(window),
                    options,
                } => {
                    let response = match self.start(&session, window, &options).await {
                        Ok((stream, track)) => {
                            streams.insert(window, stream);
                            ControlMessage::StreamStartResponse {
                                target: StreamTarget::Window(window),
                                accepted: true,
                                backend: StreamBackend::Native {
                                    codec: track.codec(),
                                },
                                track_id: Some(track.track_id()),
                                handoff: None,
                                reason: None,
                            }
                        }
                        Err(reason) => refusal(StreamTarget::Window(window), reason),
                    };
                    session.send_control(&response).await?;
                }
                ControlMessage::StreamStartRequest { target, .. } => {
                    session
                        .send_control(&refusal(target, "not supported by this host".into()))
                        .await?;
                }
                ControlMessage::StreamStopRequest(StreamTarget::Window(window)) => {
                    streams.remove(&window);
                    session.detach_window(window).await?;
                }
                ControlMessage::Ping => session.send_control(&ControlMessage::Pong).await?,
                other => tracing::debug!("ignoring {other:?}"),
            }
        }
    }

    /// A thread that delivers this session's input in order, so a slow
    /// injection never holds up the control channel.
    fn input_thread(&self) -> std::sync::mpsc::Sender<(InputEvent, Option<WindowId>)> {
        let (tx, rx) = std::sync::mpsc::channel::<(InputEvent, Option<WindowId>)>();
        let source = Arc::clone(&self.source);
        std::thread::spawn(move || {
            for (event, focus) in rx {
                source.input(&event, focus);
            }
        });
        tx
    }

    async fn pin_client(&self, peer: PeerId) {
        let mut trust = self.trust.lock().await;
        trust.pin(peer);
        if let Err(e) = trust.save(&self.trust_path) {
            eprintln!("could not save the trusted-client list: {e}");
        }
    }

    /// Picks the backend and codec, attaches the window's track and starts
    /// its frame thread.
    async fn start(
        self: &Arc<Self>,
        session: &Arc<Session>,
        window: WindowId,
        options: &StreamOptions,
    ) -> Result<(Stream, WindowTrack), String> {
        // Every backend but native is a seam for now (docs/BACKENDS.md);
        // whatever the client asked for, this host serves native.
        let backend = selection::serve(options.backend, &self.source.backends());
        if backend != BackendKind::Native {
            return Err(format!("{backend:?} is not built yet"));
        }
        let encoders = self.source.encoders();
        let codec = options
            .codecs
            .iter()
            .copied()
            .find(|codec| encoders.contains(codec))
            .ok_or_else(|| "no codec both sides support".to_owned())?;

        let source = Arc::clone(&self.source);
        let frames = tokio::task::spawn_blocking(move || source.open(window, codec))
            .await
            .map_err(|e| e.to_string())??;
        let track = session
            .attach_window(window, codec)
            .await
            .map_err(|e| e.to_string())?;

        let stop = Arc::new(AtomicBool::new(false));
        feed(
            Arc::clone(session),
            track.clone(),
            frames,
            Arc::clone(&stop),
        );
        Ok((Stream { stop }, track))
    }
}

/// The window an input event names, if it names one.
fn input_window(event: &InputEvent) -> Option<WindowId> {
    match event {
        InputEvent::PointerMove { window, .. }
        | InputEvent::PointerButton { window, .. }
        | InputEvent::PointerScroll { window, .. }
        | InputEvent::Touch { window, .. } => Some(*window),
        _ => None,
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// How often the host's clipboard is checked for a change.
const CLIPBOARD_POLL: Duration = Duration::from_millis(500);

/// Sends the host's clipboard text to the client whenever it changes,
/// except when the change is the client's own text coming back.
async fn share_clipboard(
    source: Arc<dyn WindowSource>,
    session: Arc<Session>,
    last: Arc<std::sync::Mutex<Option<String>>>,
) {
    let mut seen = None;
    loop {
        let source_now = Arc::clone(&source);
        let Ok(current) = tokio::task::spawn_blocking(move || source_now.clipboard()).await else {
            return;
        };
        let Some((counter, text)) = current else {
            return;
        };
        if seen.is_some() && seen != Some(counter) {
            let echo = last.lock().expect("clipboard").as_deref() == Some(text.as_str());
            if !echo {
                *last.lock().expect("clipboard") = Some(text.clone());
                if session
                    .send_control(&ControlMessage::Clipboard(text))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
        seen = Some(counter);
        tokio::time::sleep(CLIPBOARD_POLL).await;
    }
}

fn refusal(target: StreamTarget, reason: String) -> ControlMessage {
    ControlMessage::StreamStartResponse {
        target,
        accepted: false,
        backend: StreamBackend::Native {
            codec: VideoCodec::H264,
        },
        track_id: None,
        handoff: None,
        reason: Some(reason),
    }
}

/// Moves frames from the encoder thread onto the track until the stream is
/// stopped, the window closes, or the session ends. Keyframe requests from
/// the client reach the encoder through `want_keyframe`.
fn feed(
    session: Arc<Session>,
    track: WindowTrack,
    mut frames: Box<dyn FrameSource>,
    stop: Arc<AtomicBool>,
) {
    // The first frame is always a keyframe.
    let want_keyframe = Arc::new(AtomicBool::new(true));
    let requests = track.clone();
    let flag = Arc::clone(&want_keyframe);
    let watcher = tokio::spawn(async move {
        loop {
            requests.keyframe_requested().await;
            flag.store(true, Ordering::SeqCst);
        }
    });

    let runtime = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let keyframe = want_keyframe.swap(false, Ordering::SeqCst);
            let Some(frame) = frames.next_frame(keyframe) else {
                break;
            };
            if stop.load(Ordering::SeqCst) {
                break;
            }
            if let Some((width, height)) = frame.size {
                let resized = ControlMessage::WindowResized {
                    window: track.window(),
                    width,
                    height,
                };
                let _ = runtime.block_on(session.send_control(&resized));
            }
            let written =
                runtime.block_on(track.write_frame(Bytes::from(frame.data), frame.duration));
            if written.is_err() {
                break;
            }
        }
        watcher.abort();
        // A window that closed by itself is detached here; a stop request
        // was already detached by the session loop.
        if !stop.load(Ordering::SeqCst) {
            let window = track.window();
            let _ = runtime.block_on(session.detach_window(window));
        }
    });
}

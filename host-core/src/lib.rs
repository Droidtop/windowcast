//! The host side of the windowcast library: everything a host agent does
//! that is not specific to its operating system. An agent implements
//! [`WindowSource`] (list windows, capture and encode one) and calls
//! [`run`]; this crate does the rest: listening, pairing by PIN and the
//! PIN lockout, the trusted-client list, answering window lists, choosing
//! each stream's backend and codec, attaching window tracks and feeding
//! them frames, and keyframe requests. A host application watches and
//! steers a running host through [`HostControl`]: the PIN, the trusted
//! clients, who is connected and what each stream is doing.

pub mod audio;
pub mod gamepad;
pub mod quality;
pub mod remote;
pub mod video;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use windowcast_identity::{Identity, PeerId, TrustStore};
use windowcast_protocol::selection;
use windowcast_protocol::{
    BackendKind, ControlMessage, InputEvent, StreamBackend, StreamLimits, StreamOptions,
    StreamQuality, StreamTarget, VideoCodec, WindowId, WindowInfo,
};
use windowcast_transport::remote::RemotePeers;
use windowcast_transport::{
    accept, is_keyframe, AudioTrack, HostCredential, Session, TransportError, WindowTrack,
};

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

    /// The desktop backend: `window` cut out of a capture of the whole
    /// screen it is on, so whatever covers it shows too. Only asked of
    /// hosts that list [`BackendKind::Desktop`] in [`Self::backends`].
    fn open_desktop(
        &self,
        _window: WindowId,
        _codec: VideoCodec,
    ) -> Result<Box<dyn FrameSource>, String> {
        Err("this host does not capture whole screens".into())
    }

    /// The sound `window` makes (its application's), streamed beside its
    /// picture. `None` when this host captures no window audio.
    fn open_audio(&self, _window: WindowId) -> Option<Result<Box<dyn audio::AudioSource>, String>> {
        None
    }

    /// The host's virtual microphone, which a client's microphone plays
    /// into. `None` when this host has none.
    fn microphone(&self) -> Option<Result<Box<dyn audio::MicrophoneSink>, String>> {
        None
    }

    /// Delivers one input event. `focus` is the session's input focus: the
    /// last streamed window an event named, where keys and text go. Called
    /// on the session's input thread, in order. Gamepad events go to
    /// [`WindowSource::gamepads`] instead.
    fn input(&self, _event: &InputEvent, _focus: Option<WindowId>) {}

    /// Virtual gamepads for one session, made on its input thread when its
    /// first gamepad event arrives. `None` when this host has none.
    fn gamepads(&self) -> Option<Result<Box<dyn gamepad::GamepadSink>, String>> {
        None
    }

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

    /// The encoder in use, for people watching the host (e.g. "NVIDIA
    /// HEVC Encoder MFT"). Asked again whenever the picture size changes.
    fn describe(&self) -> String {
        String::new()
    }

    /// The frame rate the host's settings give this stream.
    fn frame_rate(&self) -> u32 {
        30
    }

    /// Holds the encoder to `quality` from the next picture on (adaptive
    /// quality, [`quality`]): below its own bitrate and frame rate, and
    /// the picture scaled down. A source that cannot adapt ignores it.
    fn set_quality(&mut self, _quality: quality::Quality) {}
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

    /// Shows a new PIN, replacing any current one.
    async fn reopen(&self) -> String {
        let pin = windowcast_pairing::generate_pin();
        show_pin(&pin);
        *self.pin.lock().await = Some(pin.clone());
        self.failures.store(0, Ordering::SeqCst);
        pin
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

/// A connected client, as a host application shows it.
#[derive(Debug, Clone)]
pub struct ClientStatus {
    pub peer: PeerId,
    pub address: String,
    pub since: Instant,
}

/// One live stream and its counters since it started. Rates are the
/// watcher's to work out: sample twice and divide by the time between.
#[derive(Debug, Clone)]
pub struct StreamStatus {
    pub peer: PeerId,
    pub window: WindowId,
    /// The backend the client asked for.
    pub requested: BackendKind,
    /// The backend serving it.
    pub backend: BackendKind,
    pub codec: VideoCodec,
    /// [`FrameSource::describe`].
    pub encoder: String,
    pub size: Option<(u32, u32)>,
    pub frames: u64,
    pub keyframes: u64,
    pub bytes: u64,
    /// Keyframes the client asked for (lost packets, a decoder behind).
    pub keyframe_requests: u64,
    /// Opus packets sent; `None` when the stream has no audio.
    pub audio_packets: Option<u64>,
    /// What adaptive quality holds the stream to, and the network it sees.
    pub quality: StreamQuality,
    pub since: Instant,
}

/// A running host, seen and steered by the application around it: its
/// identity, the pairing PIN, the clients it trusts, who is connected and
/// what each stream is doing. [`run`] and [`serve`] make one of their own;
/// an application opens one with [`HostControl::open`] and passes it to
/// [`serve_with`].
pub struct HostControl {
    identity: Identity,
    trust: Mutex<TrustStore>,
    trust_path: PathBuf,
    pairing: Arc<Pairing>,
    clients: std::sync::Mutex<HashMap<u64, ClientStatus>>,
    streams: std::sync::Mutex<HashMap<u64, StreamStatus>>,
    serial: AtomicU64,
    /// The clients' discovery IDs, for reaching them away from the LAN.
    remote_peers: RemotePeers,
    /// This host's own; `None` if its certificate could not be made.
    discovery_id: Option<String>,
}

impl HostControl {
    /// Loads (or creates) the host's identity and trusted-client list from
    /// `config.data_dir`, and opens pairing if `config.pairing` says so.
    pub fn open(config: &HostConfig) -> std::io::Result<Arc<Self>> {
        std::fs::create_dir_all(&config.data_dir)?;
        let identity = Identity::load_or_generate(&config.data_dir.join("agent-identity.key"))
            .map_err(std::io::Error::other)?;
        let trust_path = config.data_dir.join("agent-trusted-clients");
        let trust = TrustStore::load(&trust_path).map_err(std::io::Error::other)?;
        println!("host identity: {}", identity.peer_id());
        let discovery_id = windowcast_transport::remote::discovery_id(&identity).ok();
        Ok(Arc::new(HostControl {
            identity,
            trust: Mutex::new(trust),
            trust_path,
            pairing: Pairing::new(config.pairing),
            clients: Default::default(),
            streams: Default::default(),
            serial: AtomicU64::new(1),
            remote_peers: RemotePeers::load(&config.data_dir.join("remote-peers.json")),
            discovery_id,
        }))
    }

    pub fn peer_id(&self) -> PeerId {
        self.identity.peer_id()
    }

    /// Syncthing's global discovery as this host announces itself there
    /// (`servers` as Syncthing writes them), for [`remote::serve_remote`].
    pub fn directory(
        &self,
        servers: &[String],
    ) -> Result<Arc<dyn windowcast_transport::remote::Directory>, String> {
        Ok(Arc::new(windowcast_transport::remote::Syncthing::new(
            &self.identity,
            servers,
        )?))
    }

    /// The PIN a new client pairs with; `None` while pairing is closed.
    pub async fn pin(&self) -> Option<String> {
        self.pairing.current().await
    }

    /// Opens pairing with a new PIN (replacing any current one).
    pub async fn open_pairing(&self) -> String {
        self.pairing.reopen().await
    }

    /// Closes pairing: only clients paired earlier can connect.
    pub async fn close_pairing(&self) {
        self.pairing.paired().await;
    }

    /// The clients this host trusts.
    pub async fn trusted(&self) -> Vec<PeerId> {
        self.trust.lock().await.peers().copied().collect()
    }

    /// Stops trusting `peer`: it has to pair again. A session it has open
    /// now is not cut.
    pub async fn forget(&self, peer: &PeerId) -> std::io::Result<()> {
        let mut trust = self.trust.lock().await;
        trust.revoke(peer);
        trust.save(&self.trust_path).map_err(std::io::Error::other)
    }

    /// The clients connected now.
    pub fn clients(&self) -> Vec<ClientStatus> {
        self.clients
            .lock()
            .expect("clients")
            .values()
            .cloned()
            .collect()
    }

    /// The streams live now.
    pub fn streams(&self) -> Vec<StreamStatus> {
        self.streams
            .lock()
            .expect("streams")
            .values()
            .cloned()
            .collect()
    }

    fn next_serial(&self) -> u64 {
        self.serial.fetch_add(1, Ordering::SeqCst)
    }

    fn update_stream(&self, serial: u64, update: impl FnOnce(&mut StreamStatus)) {
        if let Some(status) = self.streams.lock().expect("streams").get_mut(&serial) {
            update(status);
        }
    }
}

/// Removes a status entry when its session or stream ends, however it ends.
struct Listed<'a> {
    map: &'a std::sync::Mutex<HashMap<u64, ClientStatus>>,
    serial: u64,
}

impl Drop for Listed<'_> {
    fn drop(&mut self) {
        self.map.lock().expect("clients").remove(&self.serial);
    }
}

struct Host {
    control: Arc<HostControl>,
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
    let control = HostControl::open(&config)?;
    serve_with(listener, control, source).await
}

/// Runs a host on an already bound listener, watched and steered through
/// `control`.
pub async fn serve_with(
    listener: TcpListener,
    control: Arc<HostControl>,
    source: Arc<dyn WindowSource>,
) -> std::io::Result<()> {
    let host = Arc::new(Host { control, source });
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
            match host.serve(stream, address.to_string()).await {
                Ok(()) | Err(TransportError::Closed) => println!("{address}: session ended"),
                Err(TransportError::AuthenticationFailed) => {
                    eprintln!("{address}: authentication failed");
                    host.control.pairing.failed().await;
                }
                Err(e) => eprintln!("{address}: {e}"),
            }
        });
    }
}

/// One stream being fed: dropping it stops its frame thread.
struct Stream {
    stop: Arc<AtomicBool>,
    /// The client's ceilings, which it may change while it watches.
    limits: Arc<std::sync::Mutex<StreamLimits>>,
}

/// The session's round trip, from the host's own pings.
#[derive(Default)]
struct RoundTrip {
    sent: std::sync::Mutex<Option<Instant>>,
    last: std::sync::Mutex<Option<Duration>>,
}

impl RoundTrip {
    fn answered(&self) {
        if let Some(sent) = self.sent.lock().expect("ping").take() {
            *self.last.lock().expect("round trip") = Some(sent.elapsed());
        }
    }

    fn last(&self) -> Option<Duration> {
        *self.last.lock().expect("round trip")
    }
}

/// Pings the client once a second for [`RoundTrip`]. A ping still
/// unanswered after a second is not resent: the round trip then reads as
/// at least that long, which is what congestion looks like. A client that
/// never answers (one from before hosts pinged) leaves it unknown.
async fn ping(session: Arc<Session>, rtt: Arc<RoundTrip>) {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let sent = *rtt.sent.lock().expect("ping");
        match sent {
            Some(_) if rtt.last().is_none() => {}
            Some(at) => {
                let waited = at.elapsed();
                let mut last = rtt.last.lock().expect("round trip");
                if last.is_none_or(|l| l < waited) {
                    *last = Some(waited);
                }
            }
            None => {
                *rtt.sent.lock().expect("ping") = Some(Instant::now());
                if session.send_control(&ControlMessage::Ping).await.is_err() {
                    return;
                }
            }
        }
    }
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
        self.serve_session(stream, address, session, true).await
    }

    /// Serves one client over a signaling stream (a LAN TCP socket, or a
    /// punched UDP stream from away), on `session`. Pairing by PIN only
    /// where `pairing` allows (on the LAN: a host found from away takes
    /// only clients it already trusts).
    async fn serve_session<S>(
        self: &Arc<Self>,
        stream: S,
        address: String,
        session: Session,
        pairing: bool,
    ) -> Result<(), TransportError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let control = &self.control;
        let trusted = control.trust.lock().await.clone();
        let pin = if pairing {
            control.pairing.current().await
        } else {
            None
        };
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

        let peer = established.peer;
        if established.paired {
            control.pairing.paired().await;
            self.pin_client(peer).await;
            println!("paired with {peer}; pairing is now closed");
        } else {
            println!("{peer} connected");
        }
        let serial = control.next_serial();
        control.clients.lock().expect("clients").insert(
            serial,
            ClientStatus {
                peer,
                address,
                since: Instant::now(),
            },
        );
        let _listed = Listed {
            map: &control.clients,
            serial,
        };

        let session = Arc::new(established.session);
        let mut streams = std::collections::HashMap::<WindowId, Stream>::new();
        let input = deliver_input(Arc::clone(&self.source));
        let mut focus: Option<WindowId> = None;
        let clipboard = Arc::new(std::sync::Mutex::new(None::<String>));
        let clipboard_task = tokio::spawn(share_clipboard(
            Arc::clone(&self.source),
            Arc::clone(&session),
            Arc::clone(&clipboard),
        ));
        let _clipboard_task = AbortOnDrop(clipboard_task);
        let _microphone_task = AbortOnDrop(tokio::spawn(receive_microphone(
            Arc::clone(&session),
            Arc::clone(&self.source),
        )));
        let rtt = Arc::new(RoundTrip::default());
        let _ping_task = AbortOnDrop(tokio::spawn(ping(Arc::clone(&session), Arc::clone(&rtt))));
        if let Some(discovery_id) = control.discovery_id.clone() {
            session
                .send_control(&ControlMessage::Rendezvous { discovery_id })
                .await?;
        }
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
                    let response = match self.start(&session, peer, window, &options, &rtt).await {
                        Ok((stream, track, backend)) => {
                            streams.insert(window, stream);
                            ControlMessage::StreamStartResponse {
                                target: StreamTarget::Window(window),
                                accepted: true,
                                backend,
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
                    session.detach_audio(window).await?;
                    session.detach_window(window).await?;
                }
                ControlMessage::Ping => session.send_control(&ControlMessage::Pong).await?,
                ControlMessage::Pong => rtt.answered(),
                ControlMessage::Rendezvous { discovery_id } => {
                    if let Err(e) = control.remote_peers.set(&peer, &discovery_id) {
                        eprintln!("could not keep {peer}'s discovery ID: {e}");
                    }
                }
                ControlMessage::StreamLimits { window, limits } => {
                    if let Some(stream) = streams.get(&window) {
                        *stream.limits.lock().expect("limits") = limits;
                    }
                }
                other => tracing::debug!("ignoring {other:?}"),
            }
        }
    }

    async fn pin_client(&self, peer: PeerId) {
        let mut trust = self.control.trust.lock().await;
        trust.pin(peer);
        if let Err(e) = trust.save(&self.control.trust_path) {
            eprintln!("could not save the trusted-client list: {e}");
        }
    }

    /// Picks the backend and codec, attaches the window's track and starts
    /// its frame thread.
    async fn start(
        self: &Arc<Self>,
        session: &Arc<Session>,
        peer: PeerId,
        window: WindowId,
        options: &StreamOptions,
        rtt: &Arc<RoundTrip>,
    ) -> Result<(Stream, WindowTrack, StreamBackend), String> {
        // Of the backends the client may ask for, the session-track ones
        // are served here; one this host does not offer falls back to
        // native (docs/BACKENDS.md).
        let kind = selection::serve(options.backend, &self.source.backends());
        let encoders = self.source.encoders();
        let codec = options
            .codecs
            .iter()
            .copied()
            .find(|codec| encoders.contains(codec))
            .ok_or_else(|| "no codec both sides support".to_owned())?;
        let backend = match kind {
            BackendKind::Native => StreamBackend::Native { codec },
            BackendKind::Desktop => StreamBackend::Desktop { codec },
            other => return Err(format!("{other:?} is not built yet")),
        };

        let source = Arc::clone(&self.source);
        let frames = tokio::task::spawn_blocking(move || match kind {
            BackendKind::Desktop => source.open_desktop(window, codec),
            _ => source.open(window, codec),
        })
        .await
        .map_err(|e| e.to_string())??;
        let track = session
            .attach_window(window, codec)
            .await
            .map_err(|e| e.to_string())?;

        let serial = self.control.next_serial();
        self.control.streams.lock().expect("streams").insert(
            serial,
            StreamStatus {
                peer,
                window,
                requested: options.backend,
                backend: kind,
                codec,
                encoder: String::new(),
                size: None,
                frames: 0,
                keyframes: 0,
                bytes: 0,
                keyframe_requests: 0,
                audio_packets: None,
                quality: StreamQuality::default(),
                since: Instant::now(),
            },
        );
        let stop = Arc::new(AtomicBool::new(false));
        let limits = Arc::new(std::sync::Mutex::new(options.limits));
        feed(
            Arc::clone(session),
            track.clone(),
            frames,
            Feedback {
                rtt: Arc::clone(rtt),
                limits: Arc::clone(&limits),
            },
            Arc::clone(&stop),
            Arc::clone(&self.control),
            serial,
        );
        self.start_audio(session, window, &stop, serial).await;
        Ok((Stream { stop, limits }, track, backend))
    }

    /// Streams the window's sound beside its picture, if the agent captures
    /// it. Audio that cannot start leaves the picture streaming.
    async fn start_audio(
        &self,
        session: &Arc<Session>,
        window: WindowId,
        stop: &Arc<AtomicBool>,
        serial: u64,
    ) {
        let source = Arc::clone(&self.source);
        let opened = tokio::task::spawn_blocking(move || source.open_audio(window))
            .await
            .ok()
            .flatten();
        let capture = match opened {
            None => return,
            Some(Err(e)) => {
                eprintln!("window {}: no audio: {e}", window.0);
                return;
            }
            Some(Ok(capture)) => capture,
        };
        let track = match session.attach_audio(window).await {
            Ok(track) => track,
            Err(e) => {
                eprintln!("window {}: no audio: {e}", window.0);
                return;
            }
        };
        self.control
            .update_stream(serial, |status| status.audio_packets = Some(0));
        feed_audio(
            track,
            capture,
            Arc::clone(stop),
            Arc::clone(&self.control),
            serial,
        );
    }
}

/// A thread that delivers one session's input to `source` in order, so a
/// slow injection never holds up the channel it came on; the session's
/// gamepads are plugged in on first use and unplugged when the sender is
/// dropped. Shared by windowcast sessions and GameStream ones.
pub fn deliver_input(
    source: Arc<dyn WindowSource>,
) -> std::sync::mpsc::Sender<(InputEvent, Option<WindowId>)> {
    let (tx, rx) = std::sync::mpsc::channel::<(InputEvent, Option<WindowId>)>();
    std::thread::spawn(move || {
        // The session's pads; dropped, and so unplugged, when the
        // session ends and its sender goes.
        let mut pads: Option<Box<dyn gamepad::GamepadSink>> = None;
        let mut no_pads = false;
        for (event, focus) in rx {
            match event {
                InputEvent::Gamepad { pad, state } if pad < gamepad::MAX_PADS => {
                    if pads.is_none() && !no_pads {
                        match source.gamepads() {
                            Some(Ok(sink)) => pads = Some(sink),
                            Some(Err(e)) => {
                                eprintln!("gamepads: {e}");
                                no_pads = true;
                            }
                            None => no_pads = true,
                        }
                    }
                    if let Some(pads) = pads.as_mut() {
                        pads.set(pad, &state);
                    }
                }
                InputEvent::GamepadGone { pad } => {
                    if let Some(pads) = pads.as_mut() {
                        pads.remove(pad);
                    }
                }
                InputEvent::Gamepad { .. } => {}
                event => source.input(&event, focus),
            }
        }
    });
    tx
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

/// What adaptive quality steers a stream by, besides its track's receiver
/// reports.
struct Feedback {
    rtt: Arc<RoundTrip>,
    limits: Arc<std::sync::Mutex<StreamLimits>>,
}

/// A stream's adaptive quality, run from its frame thread about once a
/// second: what went out since the last round, the client's loss and the
/// round trip in; the encoder's quality, the status and (when it changed,
/// or every few seconds) a report to the client out.
struct Adapting {
    controller: quality::Controller,
    feedback: Feedback,
    since: Instant,
    bytes: u64,
    applied: quality::Quality,
    /// The window's own size: the last announced size over the scale it
    /// was made at.
    window: (u32, u32),
    last_report: Option<Instant>,
    reported: StreamQuality,
    last_loss_at: Option<Instant>,
    loss: f32,
}

/// How often the client hears the quality even when nothing changed.
const QUALITY_REPORT_EVERY: Duration = Duration::from_secs(5);

impl Adapting {
    fn new(feedback: Feedback) -> Self {
        let limits = *feedback.limits.lock().expect("limits");
        Adapting {
            controller: quality::Controller::new(limits),
            feedback,
            since: Instant::now(),
            bytes: 0,
            applied: quality::Quality::default(),
            window: (0, 0),
            last_report: None,
            reported: StreamQuality::default(),
            last_loss_at: None,
            loss: 0.0,
        }
    }

    fn resized(&mut self, (width, height): (u32, u32)) {
        let scale = self.applied.scale;
        self.window = (
            (width as f32 / scale).round() as u32,
            (height as f32 / scale).round() as u32,
        );
    }

    /// Counts a frame; once a second, decides. Returns a report for the
    /// client when one is due.
    fn sent(
        &mut self,
        bytes: usize,
        track: &WindowTrack,
        frames: &mut dyn FrameSource,
    ) -> Option<StreamQuality> {
        self.bytes += bytes as u64;
        let now = Instant::now();
        let elapsed = now.duration_since(self.since);
        if elapsed < Duration::from_secs(1) {
            return None;
        }
        let sent_bps = (self.bytes as f64 * 8.0 / elapsed.as_secs_f64()) as u32;
        self.since = now;
        self.bytes = 0;
        // Only a report newer than the last one counts.
        let reception = track
            .reception()
            .filter(|r| self.last_loss_at.is_none_or(|at| r.at > at));
        if let Some(reception) = reception {
            self.last_loss_at = Some(reception.at);
            self.loss = reception.loss;
        }
        let limits = *self.feedback.limits.lock().expect("limits");
        self.controller.set_limits(limits);
        let host_fps = frames.frame_rate();
        let rtt = self.feedback.rtt.last();
        let quality = self.controller.observe(
            now,
            quality::Observation {
                sent_bps,
                window: self.window,
                host_fps,
                loss: reception.map(|r| r.loss),
                rtt,
            },
        );
        if quality != self.applied {
            frames.set_quality(quality);
            self.applied = quality;
        }
        let scale = quality.scale;
        let report = StreamQuality {
            target_kbps: quality.bitrate.map(|b| b / 1000),
            sent_kbps: sent_bps / 1000,
            fps: quality.fps.unwrap_or(host_fps),
            width: (self.window.0 as f32 * scale) as u32 & !1,
            height: (self.window.1 as f32 * scale) as u32 & !1,
            loss_percent: self.loss * 100.0,
            rtt_ms: rtt.map(|r| r.as_millis() as u32),
        };
        let changed = report.target_kbps != self.reported.target_kbps
            || report.fps != self.reported.fps
            || report.height != self.reported.height;
        let due = self
            .last_report
            .is_none_or(|at| now.duration_since(at) >= QUALITY_REPORT_EVERY);
        self.reported = report;
        (changed || due).then(|| {
            self.last_report = Some(now);
            report
        })
    }
}

/// Moves frames from the encoder thread onto the track until the stream is
/// stopped, the window closes, or the session ends. Keyframe requests from
/// the client reach the encoder through `want_keyframe`; adaptive quality
/// ([`Adapting`]) steers it from the same thread.
fn feed(
    session: Arc<Session>,
    track: WindowTrack,
    mut frames: Box<dyn FrameSource>,
    feedback: Feedback,
    stop: Arc<AtomicBool>,
    control: Arc<HostControl>,
    serial: u64,
) {
    // The first frame is always a keyframe.
    let want_keyframe = Arc::new(AtomicBool::new(true));
    let requests = track.clone();
    let flag = Arc::clone(&want_keyframe);
    let watched = Arc::clone(&control);
    let watcher = tokio::spawn(async move {
        loop {
            requests.keyframe_requested().await;
            flag.store(true, Ordering::SeqCst);
            watched.update_stream(serial, |status| status.keyframe_requests += 1);
        }
    });
    let codec = track.codec();

    let runtime = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let mut adapting = Adapting::new(feedback);
        while !stop.load(Ordering::SeqCst) {
            let keyframe = want_keyframe.swap(false, Ordering::SeqCst);
            let Some(frame) = frames.next_frame(keyframe) else {
                break;
            };
            if stop.load(Ordering::SeqCst) {
                break;
            }
            if let Some((width, height)) = frame.size {
                adapting.resized((width, height));
                let resized = ControlMessage::WindowResized {
                    window: track.window(),
                    width,
                    height,
                };
                let _ = runtime.block_on(session.send_control(&resized));
                let encoder = frames.describe();
                control.update_stream(serial, |status| {
                    status.size = Some((width, height));
                    status.encoder = encoder;
                });
            }
            let key = is_keyframe(codec, &frame.data);
            let bytes = frame.data.len() as u64;
            control.update_stream(serial, |status| {
                status.frames += 1;
                status.bytes += bytes;
                status.keyframes += u64::from(key);
            });
            let written =
                runtime.block_on(track.write_frame(Bytes::from(frame.data), frame.duration));
            if written.is_err() {
                break;
            }
            if let Some(quality) = adapting.sent(bytes as usize, &track, frames.as_mut()) {
                control.update_stream(serial, |status| status.quality = quality);
                let report = ControlMessage::StreamQuality {
                    window: track.window(),
                    quality,
                };
                let _ = runtime.block_on(session.send_control(&report));
            }
        }
        watcher.abort();
        control.streams.lock().expect("streams").remove(&serial);
        // A window that closed by itself is detached here; a stop request
        // was already detached by the session loop.
        if !stop.load(Ordering::SeqCst) {
            let window = track.window();
            let _ = runtime.block_on(session.detach_audio(window));
            let _ = runtime.block_on(session.detach_window(window));
        }
    });
}

/// Plays the client's microphone, when it sends one, into the agent's
/// virtual microphone: Opus decoded here, played on a thread of its own
/// (a sink may block).
async fn receive_microphone(session: Arc<Session>, source: Arc<dyn WindowSource>) {
    use windowcast_transport::RemoteTrack;
    while let Ok(track) = session.next_remote_track().await {
        let RemoteTrack::Microphone(mut remote) = track else {
            continue;
        };
        let source = Arc::clone(&source);
        let opened = tokio::task::spawn_blocking(move || source.microphone())
            .await
            .ok()
            .flatten();
        let mut sink = match opened {
            None => {
                eprintln!("a client sends its microphone; this host has no virtual microphone");
                continue;
            }
            Some(Err(e)) => {
                eprintln!("no virtual microphone: {e}");
                continue;
            }
            Some(Ok(sink)) => sink,
        };
        let Ok(mut decoder) = opus::Decoder::new(audio::RATE, opus::Channels::Stereo) else {
            continue;
        };
        let (samples_tx, samples_rx) = std::sync::mpsc::channel::<Vec<i16>>();
        std::thread::spawn(move || {
            for samples in samples_rx {
                sink.play(&samples);
            }
        });
        tokio::spawn(async move {
            let mut pcm = vec![0i16; 5760 * audio::CHANNELS];
            while let Ok(packet) = remote.next_packet().await {
                if let Ok(frames) = decoder.decode(&packet.data, &mut pcm, false) {
                    if samples_tx
                        .send(pcm[..frames * audio::CHANNELS].to_vec())
                        .is_err()
                    {
                        return;
                    }
                }
            }
        });
    }
}

/// Moves the window's sound from the agent's capture through Opus onto its
/// audio track until the stream stops or the capture ends.
fn feed_audio(
    track: AudioTrack,
    mut capture: Box<dyn audio::AudioSource>,
    stop: Arc<AtomicBool>,
    control: Arc<HostControl>,
    serial: u64,
) {
    let runtime = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let mut packets = match audio::OpusPackets::new() {
            Ok(packets) => packets,
            Err(e) => {
                eprintln!("window {}: no Opus encoder: {e}", track.window().0);
                return;
            }
        };
        while !stop.load(Ordering::SeqCst) {
            let Some(samples) = capture.next_samples() else {
                break;
            };
            let encoded = match packets.push(&samples) {
                Ok(encoded) => encoded,
                Err(e) => {
                    eprintln!("window {}: audio encoding failed: {e}", track.window().0);
                    break;
                }
            };
            let count = encoded.len() as u64;
            for packet in encoded {
                let written = runtime
                    .block_on(track.write_packet(Bytes::from(packet), Duration::from_millis(20)));
                if written.is_err() {
                    return;
                }
            }
            if count > 0 {
                control.update_stream(serial, |status| {
                    *status.audio_packets.get_or_insert(0) += count;
                });
            }
        }
    });
}

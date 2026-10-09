//! The client side of the windowcast library, and the one surface every
//! client implements against: a Rust API ([`Client`], [`ClientSession`])
//! and the C interface over it (`ffi`, declared in
//! `include/windowcast.h`). droidtop's Desktop mode (through JNI), the
//! Android library in `android/`, the reference CLI and any other client
//! use exactly this.
//!
//! The model is pull-based so no client has to accept calls from library
//! threads: a client connects, asks for the window list and for streams,
//! then polls two queues, session events ([`ClientSession::next_event`])
//! and each window's frames ([`ClientSession::next_frame`]), from its own
//! threads. Frames come out whole and ready for a hardware decoder.

pub mod ffi;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::runtime::Runtime;
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::selection::{self, BackendRule};
use windowcast_protocol::{
    BackendKind, ControlMessage, InputEvent, StreamLimits, StreamOptions, StreamQuality,
    StreamTarget, VideoCodec, WindowId, WindowInfo,
};
use windowcast_transport::remote::{
    self, Answer, Directory, RemoteConfig, RemotePeers, Syncthing, CONNECT_WITHIN,
};
use windowcast_transport::{
    connect, AudioPacket, AudioTrack, ClientCredential, RemoteAudio, RemoteTrack, RemoteWindow,
    Session, TransportError, WindowFrame,
};

/// Frames queued per window before the client is considered behind. When
/// it is, frames are dropped up to the next keyframe (and one is asked
/// for), so a slow decoder skips ahead instead of falling further behind.
const FRAME_QUEUE: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{0}")]
    Transport(#[from] TransportError),
    #[error("cannot reach {0}: {1}")]
    Unreachable(String, std::io::Error),
    #[error("identity or trust store: {0}")]
    Identity(#[from] windowcast_identity::IdentityError),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    /// The host cannot be looked for away from the LAN: not paired, or no
    /// session on the LAN told this client its discovery ID yet.
    #[error("{0} cannot be reached away from the LAN yet: connect on the LAN once first")]
    NotReachableAway(String),
    /// Discovery has no address for the host, or none answered.
    #[error("{0} was not found away from the LAN ({1})")]
    NotFound(String, String),
}

/// Where a session keeps the host's discovery ID, and this client's own.
struct Rendezvous {
    peers: Arc<RemotePeers>,
    own: Option<String>,
}

/// One client identity and its trusted hosts; connects to any number of
/// hosts.
pub struct Client {
    runtime: Arc<Runtime>,
    identity: Arc<Identity>,
    trust: Mutex<TrustStore>,
    trust_path: PathBuf,
    /// The hosts' discovery IDs, learned on sessions, for reaching them
    /// away from the LAN.
    remote_hosts: Arc<RemotePeers>,
}

impl Client {
    /// Loads (or creates) the client's identity and trusted-host list in
    /// `data_dir`, app-private storage on Android.
    pub fn new(data_dir: &Path) -> Result<Self, ClientError> {
        std::fs::create_dir_all(data_dir)?;
        let identity = Identity::load_or_generate(&data_dir.join("client-identity.key"))?;
        let trust_path = data_dir.join("client-trusted-hosts");
        let trust = TrustStore::load(&trust_path)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        Ok(Client {
            runtime: Arc::new(runtime),
            identity: Arc::new(identity),
            trust: Mutex::new(trust),
            trust_path,
            remote_hosts: Arc::new(RemotePeers::load(&data_dir.join("remote-hosts.json"))),
        })
    }

    /// This client's identity, as a host shows it.
    pub fn peer_id(&self) -> String {
        self.identity.peer_id().to_hex()
    }

    /// Connects to a host agent at `address` (`HOST:PORT`): pairs with
    /// `pin` the first time, otherwise resumes with the pinned identity.
    /// Blocks until the session is up.
    pub fn connect(&self, address: &str, pin: Option<&str>) -> Result<ClientSession, ClientError> {
        let trusted = self.trust.lock().expect("trust store").clone();
        let identity = Arc::clone(&self.identity);
        let established = self.runtime.block_on(async {
            let stream = tokio::net::TcpStream::connect(address)
                .await
                .map_err(|e| ClientError::Unreachable(address.to_owned(), e))?;
            // A host on this device is reached over loopback alone.
            let local = stream.peer_addr().is_ok_and(|peer| peer.ip().is_loopback());
            let session = if local {
                Session::local_only().await?
            } else {
                Session::new().await?
            };
            let credential = match pin {
                Some(pin) => ClientCredential::Pin(pin),
                None => ClientCredential::Pinned(&trusted),
            };
            Ok::<_, ClientError>(connect(stream, session, &identity, credential).await?)
        })?;

        if established.paired {
            let mut trust = self.trust.lock().expect("trust store");
            trust.pin(established.peer);
            trust.save(&self.trust_path)?;
        }
        Ok(self.started(established))
    }

    fn started(&self, established: windowcast_transport::Established) -> ClientSession {
        ClientSession::start(
            Arc::clone(&self.runtime),
            established.session,
            established.peer,
            established.paired,
            Rendezvous {
                peers: Arc::clone(&self.remote_hosts),
                own: windowcast_transport::remote::discovery_id(&self.identity).ok(),
            },
        )
    }

    /// Whether `host_id` (hex) can be looked for away from the LAN: it is
    /// paired and told this client its discovery ID.
    pub fn reachable_away(&self, host_id: &str) -> bool {
        windowcast_identity::PeerId::from_hex(host_id).is_ok_and(|peer| {
            self.trust.lock().expect("trust store").is_pinned(&peer)
                && self.remote_hosts.get(&peer).is_some()
        })
    }

    /// Connects to a paired host away from the LAN through Syncthing's
    /// global discovery and the STUN servers `config` names: announces
    /// this client's address, looks the host up, and punches through both
    /// NATs to it (`windowcast_transport::remote`). Blocks, up to a minute
    /// and a half while the host's next lookup finds this client.
    pub fn connect_away(
        &self,
        host_id: &str,
        config: &RemoteConfig,
    ) -> Result<ClientSession, ClientError> {
        let directory = Syncthing::new(&self.identity, &config.servers)
            .map_err(|e| ClientError::NotFound(host_id.to_owned(), e))?;
        self.connect_away_with(host_id, config, &directory, CONNECT_WITHIN)
    }

    /// [`Self::connect_away`] with another directory and time limit.
    pub fn connect_away_with(
        &self,
        host_id: &str,
        config: &RemoteConfig,
        directory: &dyn Directory,
        within: Duration,
    ) -> Result<ClientSession, ClientError> {
        let peer = windowcast_identity::PeerId::from_hex(host_id)?;
        let trusted = self.trust.lock().expect("trust store").clone();
        let host_discovery = self
            .remote_hosts
            .get(&peer)
            .filter(|_| trusted.is_pinned(&peer))
            .ok_or_else(|| ClientError::NotReachableAway(host_id.to_owned()))?;
        let identity = Arc::clone(&self.identity);
        let established = self.runtime.block_on(async {
            let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
            let rendezvous = windowcast_transport::punched::Rendezvous::new(socket, false);
            let config_copy = config.clone();
            let stun = tokio::task::spawn_blocking(move || config_copy.stun_addresses())
                .await
                .unwrap_or_default();
            let mapped = remote::mapped_address(&rendezvous, &stun, Duration::from_secs(3)).await;
            // So the host's lookups find this client and punch towards it.
            // Discovery blocks on https; the runtime's other worker keeps
            // the rendezvous socket read meanwhile.
            let _ = tokio::task::block_in_place(|| directory.announce(&remote::announced(mapped)));
            let endpoints = match tokio::task::block_in_place(|| directory.lookup(&host_discovery))
            {
                Answer::Found(addresses) => remote::endpoints(&addresses),
                _ => Vec::new(),
            };
            if endpoints.is_empty() {
                return Err(ClientError::NotFound(
                    host_id.to_owned(),
                    "discovery has no address for it".into(),
                ));
            }
            let deadline = std::time::Instant::now() + within;
            let stream = loop {
                let mut last = None;
                for endpoint in &endpoints {
                    match rendezvous.connect(*endpoint, Duration::from_secs(4)).await {
                        Ok(stream) => {
                            last = Some(Ok(stream));
                            break;
                        }
                        Err(e) => last = Some(Err(e)),
                    }
                }
                match last {
                    Some(Ok(stream)) => break stream,
                    Some(Err(e)) if std::time::Instant::now() >= deadline => {
                        return Err(ClientError::NotFound(host_id.to_owned(), e.to_string()));
                    }
                    _ => {}
                }
            };
            let session = Session::away(&config.stun_names()).await?;
            Ok::<_, ClientError>(
                connect(
                    stream,
                    session,
                    &identity,
                    ClientCredential::Pinned(&trusted),
                )
                .await?,
            )
        })?;
        Ok(self.started(established))
    }

    /// Stops trusting the host whose identity is `host_id` (hex, as
    /// [`ClientSession::host_id`] gives it): it has to be paired again.
    pub fn forget(&self, host_id: &str) -> Result<(), ClientError> {
        let peer = windowcast_identity::PeerId::from_hex(host_id)?;
        let mut trust = self.trust.lock().expect("trust store");
        trust.revoke(&peer);
        trust.save(&self.trust_path)?;
        Ok(())
    }
}

/// Something that happened on a session. Serialized as JSON for the C
/// interface, tagged by `type`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Windows {
        windows: Vec<WindowInfo>,
    },
    StreamStarted {
        window: u64,
        backend: BackendKind,
        /// The video codec, for backends that send video on the session.
        codec: Option<VideoCodec>,
    },
    StreamRefused {
        window: u64,
        reason: String,
    },
    StreamStopped {
        window: u64,
    },
    WindowResized {
        window: u64,
        width: u32,
        height: u32,
    },
    WindowFocused {
        window: u64,
    },
    /// The host's clipboard text changed.
    Clipboard {
        text: String,
    },
    /// What a stream is sent at now and the network the host sees: on
    /// every change and every few seconds.
    StreamQuality {
        window: u64,
        quality: StreamQuality,
    },
    Closed,
}

/// Opus packets queued per window before the oldest are dropped: a second
/// of sound, more than any player buffers.
const AUDIO_QUEUE: usize = 50;

/// The result of waiting for a window's sound.
pub enum AudioPoll {
    /// One Opus packet (48 kHz, stereo), 20 ms of sound.
    Packet(AudioPacket),
    /// Nothing within the timeout: the window is quiet, or it has no
    /// audio (yet, or at all).
    Timeout,
    /// The window's audio ended.
    Ended,
}

/// The result of waiting for a frame.
pub enum FramePoll {
    Frame(WindowFrame),
    Timeout,
    /// The window's stream ended, or was never started.
    Ended,
}

struct FrameQueue {
    frames: Receiver<WindowFrame>,
    /// A frame a caller could not take (its buffer was too small); handed
    /// out again first.
    held: Option<WindowFrame>,
}

/// One streamed window's frames. Created when the host accepts the stream
/// (before its track delivers anything), so a client can start waiting
/// for frames as soon as it sees [`Event::StreamStarted`].
struct WindowSlot {
    queue: Mutex<FrameQueue>,
    /// Taken by the frame pump when the window's track arrives; dropping it
    /// ends the queue.
    sender: Mutex<Option<SyncSender<WindowFrame>>>,
    /// Set by [`ClientSession::request_keyframe`]; the frame pump asks the
    /// host for one with the next frame it sees.
    keyframe: Arc<AtomicBool>,
}

struct Shared {
    events: Mutex<Receiver<Event>>,
    slots: Mutex<HashMap<WindowId, Arc<WindowSlot>>>,
    /// Each window's sound, from its audio track's arrival to its end.
    audio: Mutex<HashMap<WindowId, Arc<Mutex<Receiver<AudioPacket>>>>>,
    /// When the unanswered ping went out, and the last round trip.
    ping: Mutex<(Option<Instant>, Option<Duration>)>,
    /// The ceilings this client set per window, sent with each start.
    limits: Mutex<HashMap<WindowId, StreamLimits>>,
}

impl Shared {
    fn slot(&self, window: WindowId) -> Arc<WindowSlot> {
        Arc::clone(
            self.slots
                .lock()
                .expect("slots")
                .entry(window)
                .or_insert_with(new_slot),
        )
    }

    fn existing_slot(&self, window: WindowId) -> Option<Arc<WindowSlot>> {
        self.slots.lock().expect("slots").get(&window).cloned()
    }

    /// The sender for a newly arrived track: the waiting slot's, or a
    /// fresh slot's when that one's sender is already in use.
    fn take_sender(&self, window: WindowId) -> (SyncSender<WindowFrame>, Arc<AtomicBool>) {
        let mut slots = self.slots.lock().expect("slots");
        let slot = slots.entry(window).or_insert_with(new_slot);
        if let Some(sender) = slot.sender.lock().expect("sender").take() {
            return (sender, Arc::clone(&slot.keyframe));
        }
        let fresh = new_slot();
        let sender = fresh
            .sender
            .lock()
            .expect("sender")
            .take()
            .expect("new slot");
        let keyframe = Arc::clone(&fresh.keyframe);
        *slot = fresh;
        (sender, keyframe)
    }
}

fn new_slot() -> Arc<WindowSlot> {
    let (tx, rx) = mpsc::sync_channel(FRAME_QUEUE);
    Arc::new(WindowSlot {
        queue: Mutex::new(FrameQueue {
            frames: rx,
            held: None,
        }),
        sender: Mutex::new(Some(tx)),
        keyframe: Arc::new(AtomicBool::new(false)),
    })
}

/// A connected session, seen from the client.
pub struct ClientSession {
    runtime: Arc<Runtime>,
    session: Arc<Session>,
    shared: Arc<Shared>,
    host: String,
    paired: bool,
    /// The user's backend rules, checked before the defaults.
    rules: Mutex<Vec<BackendRule>>,
    /// The last window list, for choosing backends by content.
    windows: Arc<Mutex<Vec<WindowInfo>>>,
    /// The microphone track, while the microphone is on.
    microphone: Mutex<Option<AudioTrack>>,
}

impl ClientSession {
    fn start(
        runtime: Arc<Runtime>,
        session: Session,
        host: windowcast_identity::PeerId,
        paired: bool,
        rendezvous: Rendezvous,
    ) -> Self {
        let session = Arc::new(session);
        if let Some(discovery_id) = rendezvous.own.clone() {
            let session = Arc::clone(&session);
            runtime.spawn(async move {
                let _ = session
                    .send_control(&ControlMessage::Rendezvous { discovery_id })
                    .await;
            });
        }
        let (event_tx, event_rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            events: Mutex::new(event_rx),
            slots: Mutex::new(HashMap::new()),
            audio: Mutex::new(HashMap::new()),
            ping: Mutex::new((None, None)),
            limits: Mutex::default(),
        });
        let windows = Arc::new(Mutex::new(Vec::new()));

        runtime.spawn(pump_events(
            Arc::clone(&session),
            Arc::clone(&shared),
            event_tx,
            Arc::clone(&windows),
            (host, rendezvous.peers),
        ));
        runtime.spawn(pump_windows(Arc::clone(&session), Arc::clone(&shared)));

        ClientSession {
            runtime,
            session,
            shared,
            host: host.to_hex(),
            paired,
            rules: Mutex::new(Vec::new()),
            windows,
            microphone: Mutex::new(None),
        }
    }

    /// The host's identity.
    pub fn host_id(&self) -> &str {
        &self.host
    }

    /// Whether this connection paired by PIN (the host is now pinned).
    pub fn paired(&self) -> bool {
        self.paired
    }

    /// Replaces the user's backend rules (per-app overrides and the like).
    pub fn set_rules(&self, rules: Vec<BackendRule>) {
        *self.rules.lock().expect("rules") = rules;
    }

    /// Asks for the window list; it arrives as [`Event::Windows`].
    pub fn request_windows(&self) -> Result<(), ClientError> {
        self.send(ControlMessage::ListWindowsRequest)
    }

    /// Asks to stream `window`, decodable in `codecs` (most preferred
    /// first). The backend comes from the user's rules, then the defaults;
    /// the answer arrives as [`Event::StreamStarted`] or
    /// [`Event::StreamRefused`], and frames through [`Self::next_frame`].
    pub fn start_window(&self, window: WindowId, codecs: &[VideoCodec]) -> Result<(), ClientError> {
        let backend = {
            let windows = self.windows.lock().expect("windows");
            let rules = self.rules.lock().expect("rules");
            windows
                .iter()
                .find(|info| info.id == window)
                .map_or(BackendKind::Native, |info| {
                    selection::choose_backend(info, &rules)
                })
        };
        let limits = self
            .shared
            .limits
            .lock()
            .expect("limits")
            .get(&window)
            .copied()
            .unwrap_or_default();
        self.send(ControlMessage::StreamStartRequest {
            target: StreamTarget::Window(window),
            options: StreamOptions {
                backend,
                codecs: codecs.to_vec(),
                limits,
            },
        })
    }

    /// Sets this client's ceilings for a window's stream: kept for its
    /// next start, and sent at once to a stream already running (the host
    /// ignores it for a window it is not streaming). The host adapts below
    /// them to what the network carries.
    pub fn set_stream_limits(
        &self,
        window: WindowId,
        limits: StreamLimits,
    ) -> Result<(), ClientError> {
        self.shared
            .limits
            .lock()
            .expect("limits")
            .insert(window, limits);
        self.send(ControlMessage::StreamLimits { window, limits })
    }

    /// Sends input to the host. Pointer and touch events go to the window
    /// they name (one this session streams); keys, text and gamepads to the
    /// last such window.
    pub fn send_input(&self, event: InputEvent) -> Result<(), ClientError> {
        self.send(ControlMessage::Input(event))
    }

    /// Gives the host this client's clipboard text.
    pub fn set_clipboard(&self, text: &str) -> Result<(), ClientError> {
        self.send(ControlMessage::Clipboard(text.to_owned()))
    }

    pub fn stop_window(&self, window: WindowId) -> Result<(), ClientError> {
        self.send(ControlMessage::StreamStopRequest(StreamTarget::Window(
            window,
        )))
    }

    /// The next session event, waiting up to `timeout`. `None` on timeout.
    /// After [`Event::Closed`] every call returns `Closed`.
    pub fn next_event(&self, timeout: Duration) -> Option<Event> {
        match self
            .shared
            .events
            .lock()
            .expect("events")
            .recv_timeout(timeout)
        {
            Ok(event) => Some(event),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(Event::Closed),
        }
    }

    /// The next frame of `window`, waiting up to `timeout`.
    pub fn next_frame(&self, window: WindowId, timeout: Duration) -> FramePoll {
        let Some(slot) = self.shared.existing_slot(window) else {
            return FramePoll::Ended;
        };
        let mut queue = slot.queue.lock().expect("frame queue");
        if let Some(frame) = queue.held.take() {
            return FramePoll::Frame(frame);
        }
        match queue.frames.recv_timeout(timeout) {
            Ok(frame) => FramePoll::Frame(frame),
            Err(RecvTimeoutError::Timeout) => FramePoll::Timeout,
            Err(RecvTimeoutError::Disconnected) => {
                drop(queue);
                let mut slots = self.shared.slots.lock().expect("slots");
                if slots
                    .get(&window)
                    .is_some_and(|current| Arc::ptr_eq(current, &slot))
                {
                    slots.remove(&window);
                }
                FramePoll::Ended
            }
        }
    }

    /// The next Opus packet of `window`'s sound, waiting up to `timeout`.
    /// A window's audio track arrives a little after its stream starts, and
    /// only if the host captures its sound; until then this times out.
    pub fn next_audio(&self, window: WindowId, timeout: Duration) -> AudioPoll {
        let queue = self
            .shared
            .audio
            .lock()
            .expect("audio")
            .get(&window)
            .cloned();
        let Some(queue) = queue else {
            std::thread::sleep(timeout.min(Duration::from_millis(50)));
            return AudioPoll::Timeout;
        };
        let received = queue.lock().expect("audio queue").recv_timeout(timeout);
        match received {
            Ok(packet) => AudioPoll::Packet(packet),
            Err(RecvTimeoutError::Timeout) => AudioPoll::Timeout,
            Err(RecvTimeoutError::Disconnected) => {
                let mut audio = self.shared.audio.lock().expect("audio");
                if audio
                    .get(&window)
                    .is_some_and(|current| Arc::ptr_eq(current, &queue))
                {
                    audio.remove(&window);
                }
                AudioPoll::Ended
            }
        }
    }

    /// Puts a frame back to be returned by the next [`Self::next_frame`].
    pub(crate) fn hold_frame(&self, window: WindowId, frame: WindowFrame) {
        if let Some(slot) = self.shared.existing_slot(window) {
            slot.queue.lock().expect("frame queue").held = Some(frame);
        }
    }

    /// Asks the host for a keyframe of `window`, e.g. after the client
    /// reset its decoder; it comes within about one frame.
    pub fn request_keyframe(&self, window: WindowId) {
        if let Some(slot) = self.shared.existing_slot(window) {
            slot.keyframe.store(true, Ordering::SeqCst);
        }
    }

    /// Starts sending this client's microphone to the host: Opus packets
    /// given to [`Self::send_microphone`] reach the host's virtual
    /// microphone (where the host has one and allows it).
    pub fn start_microphone(&self) -> Result<(), ClientError> {
        let track = self.runtime.block_on(self.session.attach_microphone())?;
        *self.microphone.lock().expect("microphone") = Some(track);
        Ok(())
    }

    /// Sends one Opus packet (48 kHz, stereo, 20 ms) of microphone sound.
    pub fn send_microphone(&self, packet: &[u8]) -> Result<(), ClientError> {
        let track = self.microphone.lock().expect("microphone").clone();
        let Some(track) = track else {
            return Err(ClientError::Transport(TransportError::Closed));
        };
        Ok(self.runtime.block_on(track.write_packet(
            bytes::Bytes::copy_from_slice(packet),
            Duration::from_millis(20),
        ))?)
    }

    /// Stops sending the microphone.
    pub fn stop_microphone(&self) -> Result<(), ClientError> {
        self.microphone.lock().expect("microphone").take();
        self.runtime.block_on(self.session.detach_microphone())?;
        Ok(())
    }

    /// Measures the round trip to the host over the control channel; the
    /// result is [`Self::round_trip`] once the host answers.
    pub fn ping(&self) -> Result<(), ClientError> {
        self.shared.ping.lock().expect("ping").0 = Some(Instant::now());
        self.send(ControlMessage::Ping)
    }

    /// The last round trip [`Self::ping`] measured.
    pub fn round_trip(&self) -> Option<Duration> {
        self.shared.ping.lock().expect("ping").1
    }

    /// Ends the session; the host is told at once.
    pub fn close(&self) {
        let _ = self.runtime.block_on(self.session.close());
    }

    fn send(&self, message: ControlMessage) -> Result<(), ClientError> {
        Ok(self.runtime.block_on(self.session.send_control(&message))?)
    }
}

impl Drop for ClientSession {
    fn drop(&mut self) {
        self.close();
    }
}

async fn pump_events(
    session: Arc<Session>,
    shared: Arc<Shared>,
    events: mpsc::Sender<Event>,
    windows: Arc<Mutex<Vec<WindowInfo>>>,
    (host, remote_hosts): (windowcast_identity::PeerId, Arc<RemotePeers>),
) {
    loop {
        let message = match session.recv_control().await {
            Ok(message) => message,
            Err(_) => {
                let _ = events.send(Event::Closed);
                return;
            }
        };
        let event = match message {
            ControlMessage::ListWindowsResponse(list) => {
                *windows.lock().expect("windows") = list.clone();
                Event::Windows { windows: list }
            }
            ControlMessage::StreamStartResponse {
                target: StreamTarget::Window(window),
                accepted,
                backend,
                reason,
                ..
            } => {
                if accepted {
                    if backend.session_codec().is_some() {
                        shared.slot(window);
                    }
                    Event::StreamStarted {
                        window: window.0,
                        backend: backend.kind(),
                        codec: backend.session_codec(),
                    }
                } else {
                    Event::StreamRefused {
                        window: window.0,
                        reason: reason.unwrap_or_default(),
                    }
                }
            }
            ControlMessage::StreamStopped(StreamTarget::Window(window)) => {
                // A stream stopped before its track delivered anything still
                // holds its sender; dropping it ends the queue. The slot
                // leaves the map, so a new stream of the same window gets a
                // fresh queue instead of this ended one.
                let slot = shared.slots.lock().expect("slots").remove(&window);
                if let Some(slot) = slot {
                    slot.sender.lock().expect("sender").take();
                }
                Event::StreamStopped { window: window.0 }
            }
            ControlMessage::WindowResized {
                window,
                width,
                height,
            } => Event::WindowResized {
                window: window.0,
                width,
                height,
            },
            ControlMessage::WindowFocused(window) => Event::WindowFocused { window: window.0 },
            ControlMessage::Clipboard(text) => Event::Clipboard { text },
            ControlMessage::Pong => {
                let mut ping = shared.ping.lock().expect("ping");
                if let Some(sent) = ping.0.take() {
                    ping.1 = Some(sent.elapsed());
                }
                continue;
            }
            // Kept for reaching this host away from the LAN later.
            ControlMessage::Rendezvous { discovery_id } => {
                let _ = remote_hosts.set(&host, &discovery_id);
                continue;
            }
            // The host measures the round trip for adaptive quality.
            ControlMessage::Ping => {
                if session.send_control(&ControlMessage::Pong).await.is_err() {
                    let _ = events.send(Event::Closed);
                    return;
                }
                continue;
            }
            ControlMessage::StreamQuality { window, quality } => Event::StreamQuality {
                window: window.0,
                quality,
            },
            _ => continue,
        };
        if events.send(event).is_err() {
            return;
        }
    }
}

/// Takes each window track as it arrives and pumps its frames into that
/// window's queue.
async fn pump_windows(session: Arc<Session>, shared: Arc<Shared>) {
    while let Ok(remote) = session.next_remote_track().await {
        match remote {
            RemoteTrack::Window(remote) => {
                let (sender, keyframe) = shared.take_sender(remote.window());
                tokio::spawn(pump_frames(remote, sender, keyframe));
            }
            RemoteTrack::Audio(remote) => {
                let (sender, receiver) = mpsc::sync_channel(AUDIO_QUEUE);
                shared
                    .audio
                    .lock()
                    .expect("audio")
                    .insert(remote.window(), Arc::new(Mutex::new(receiver)));
                tokio::spawn(pump_audio(remote, sender));
            }
            // Only hosts receive a microphone.
            RemoteTrack::Microphone(_) => {}
        }
    }
}

/// A window's Opus packets into its queue; a full queue (nobody playing)
/// drops the new packet rather than block the session.
async fn pump_audio(mut remote: RemoteAudio, queue: SyncSender<AudioPacket>) {
    while let Ok(packet) = remote.next_packet().await {
        if let Err(TrySendError::Disconnected(_)) = queue.try_send(packet) {
            return;
        }
    }
}

async fn pump_frames(
    mut remote: RemoteWindow,
    queue: SyncSender<WindowFrame>,
    keyframe: Arc<AtomicBool>,
) {
    let mut skipping = false;
    while let Ok(frame) = remote.next_frame().await {
        if keyframe.swap(false, Ordering::SeqCst) {
            remote.request_keyframe().await;
        }
        if skipping && !frame.keyframe {
            continue;
        }
        match queue.try_send(frame) {
            Ok(()) => skipping = false,
            Err(TrySendError::Full(_)) => {
                skipping = true;
                remote.request_keyframe().await;
            }
            Err(TrySendError::Disconnected(_)) => return,
        }
    }
}

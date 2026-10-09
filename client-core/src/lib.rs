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
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::runtime::Runtime;
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::selection::{self, BackendRule};
use windowcast_protocol::{
    BackendKind, ControlMessage, InputEvent, StreamOptions, StreamTarget, VideoCodec, WindowId,
    WindowInfo,
};
use windowcast_transport::{
    connect, ClientCredential, RemoteWindow, Session, TransportError, WindowFrame,
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
}

/// One client identity and its trusted hosts; connects to any number of
/// hosts.
pub struct Client {
    runtime: Arc<Runtime>,
    identity: Arc<Identity>,
    trust: Mutex<TrustStore>,
    trust_path: PathBuf,
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
        Ok(ClientSession::start(
            Arc::clone(&self.runtime),
            established.session,
            established.peer.to_hex(),
            established.paired,
        ))
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
    Closed,
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
}

struct Shared {
    events: Mutex<Receiver<Event>>,
    slots: Mutex<HashMap<WindowId, Arc<WindowSlot>>>,
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
    fn take_sender(&self, window: WindowId) -> SyncSender<WindowFrame> {
        let mut slots = self.slots.lock().expect("slots");
        let slot = slots.entry(window).or_insert_with(new_slot);
        if let Some(sender) = slot.sender.lock().expect("sender").take() {
            return sender;
        }
        let fresh = new_slot();
        let sender = fresh
            .sender
            .lock()
            .expect("sender")
            .take()
            .expect("new slot");
        *slot = fresh;
        sender
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
}

impl ClientSession {
    fn start(runtime: Arc<Runtime>, session: Session, host: String, paired: bool) -> Self {
        let session = Arc::new(session);
        let (event_tx, event_rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            events: Mutex::new(event_rx),
            slots: Mutex::new(HashMap::new()),
        });
        let windows = Arc::new(Mutex::new(Vec::new()));

        runtime.spawn(pump_events(
            Arc::clone(&session),
            Arc::clone(&shared),
            event_tx,
            Arc::clone(&windows),
        ));
        runtime.spawn(pump_windows(Arc::clone(&session), Arc::clone(&shared)));

        ClientSession {
            runtime,
            session,
            shared,
            host,
            paired,
            rules: Mutex::new(Vec::new()),
            windows,
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
        self.send(ControlMessage::StreamStartRequest {
            target: StreamTarget::Window(window),
            options: StreamOptions {
                backend,
                codecs: codecs.to_vec(),
            },
        })
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

    /// Puts a frame back to be returned by the next [`Self::next_frame`].
    pub(crate) fn hold_frame(&self, window: WindowId, frame: WindowFrame) {
        if let Some(slot) = self.shared.existing_slot(window) {
            slot.queue.lock().expect("frame queue").held = Some(frame);
        }
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
    while let Ok(remote) = session.next_remote_window().await {
        let sender = shared.take_sender(remote.window());
        tokio::spawn(pump_frames(remote, sender));
    }
}

async fn pump_frames(mut remote: RemoteWindow, queue: SyncSender<WindowFrame>) {
    let mut skipping = false;
    while let Ok(frame) = remote.next_frame().await {
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

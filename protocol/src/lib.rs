//! Wire types shared by every windowcast agent/client. Pure data + codec —
//! no networking, no crypto, no platform deps, so this crate can be reused
//! by anything embedding windowcast without pulling in WebRTC or capture
//! backends it doesn't need.

pub mod command;
pub mod keys;
pub mod selection;

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to the message shapes below. A peer
/// that receives a mismatched version should refuse the session rather
/// than guess at how to interpret an unknown wire format.
pub const PROTOCOL_VERSION: u16 = 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WindowId(pub u64);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: WindowId,
    pub title: String,
    /// App/executable identifier (Wayland app_id, Windows exe name, macOS bundle id).
    pub app_id: String,
    pub width: u32,
    pub height: u32,
    pub focused: bool,
    /// What the host thinks the window shows ([`selection::classify`]);
    /// selection rules key on it.
    pub content: ContentHint,
    /// The window that owns this one: a dialog's, popup's or menu's
    /// program window. `None` for a top-level window. Every window,
    /// owned or not, is its own entry and is shown on its own
    /// (docs/BACKENDS.md, "Window by window, always").
    pub owner: Option<WindowId>,
    pub kind: WindowKind,
    /// Its top-left corner on the host's desktop, in the same pixels as
    /// `width` and `height`, when the host knows it (Wayland does not tell
    /// one client where another's windows are). A client places a popup
    /// against its owner by the difference of their positions.
    pub position: Option<(i32, i32)>,
}

/// What a window is, as far as a client shows it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    /// A program's own window.
    #[default]
    Normal,
    /// A dialog a window owns (a Save As box, a settings window).
    Dialog,
    /// A short-lived window a window owns: a drop-down, a tooltip, a
    /// completion list.
    Popup,
    /// A menu.
    Menu,
}

/// What kind of content a window shows, as far as choosing a backend goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ContentHint {
    General,
    /// Mostly text: editors, terminals, documents.
    Text,
    /// A game: latency and controller input matter most.
    Game,
    /// A video player: the content is already encoded video.
    Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TouchPhase {
    Start,
    Move,
    End,
    Cancel,
}

/// Input from the client to the host, on the control channel, whatever the
/// stream's backend (one input back-channel, docs/BACKENDS.md).
///
/// Pointer and touch coordinates are normalized to the streamed picture of
/// `window`, [0.0, 1.0] on each axis, so the host needs no client viewport
/// size. Events that name a window also make it the session's input focus;
/// keys, text and gamepads go to the focus (the host brings that window to
/// the front first).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    PointerMove {
        window: WindowId,
        x: f32,
        y: f32,
    },
    PointerButton {
        window: WindowId,
        button: PointerButton,
        pressed: bool,
    },
    /// Scroll in wheel notches; positive `dy` scrolls content up (wheel
    /// away from the user), positive `dx` to the right.
    PointerScroll {
        window: WindowId,
        dx: f32,
        dy: f32,
    },
    /// Platform-neutral keycode: the evdev/Linux keycode space, which every
    /// agent (including Windows/macOS ones) translates into on the way in.
    Key {
        keycode: u32,
        pressed: bool,
    },
    /// Text typed through an input method (an on-screen keyboard), sent as
    /// characters rather than keys.
    Text {
        text: String,
    },
    Touch {
        window: WindowId,
        id: u32,
        x: f32,
        y: f32,
        phase: TouchPhase,
    },
    /// The whole state of one gamepad (up to four, `pad` 0 to 3), sent on
    /// every change.
    Gamepad {
        pad: u8,
        state: GamepadState,
    },
    /// That gamepad was disconnected on the client.
    GamepadGone {
        pad: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// An Xbox-layout gamepad: the layout Windows' XInput, Linux's evdev
/// gamepad mapping and Android's KeyEvent/MotionEvent gamepad codes share.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GamepadState {
    /// [`GamepadButtons`] bits.
    pub buttons: u32,
    /// Sticks, -32768 to 32767, up and right positive.
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
    /// Triggers, 0 to 255.
    pub left_trigger: u8,
    pub right_trigger: u8,
}

/// Bits of [`GamepadState::buttons`], the same values as XInput's.
pub struct GamepadButtons;

impl GamepadButtons {
    pub const DPAD_UP: u32 = 0x0001;
    pub const DPAD_DOWN: u32 = 0x0002;
    pub const DPAD_LEFT: u32 = 0x0004;
    pub const DPAD_RIGHT: u32 = 0x0008;
    pub const START: u32 = 0x0010;
    pub const BACK: u32 = 0x0020;
    pub const LEFT_THUMB: u32 = 0x0040;
    pub const RIGHT_THUMB: u32 = 0x0080;
    pub const LEFT_SHOULDER: u32 = 0x0100;
    pub const RIGHT_SHOULDER: u32 = 0x0200;
    pub const GUIDE: u32 = 0x0400;
    pub const A: u32 = 0x1000;
    pub const B: u32 = 0x2000;
    pub const X: u32 = 0x4000;
    pub const Y: u32 = 0x8000;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GameId(pub u32);

/// One game the host can stream through the GameStream backend (see
/// [`StreamBackend::GameStream`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameEntry {
    pub id: GameId,
    pub name: String,
    pub artwork_uri: Option<String>,
}

/// What a client is asking to stream. A window is the normal case; a game
/// is an entry from the host's game list, not a window; `Desktop` is the
/// host's whole desktop as one stream.
///
/// Other target kinds (an SSH/PTY session, something reached over a
/// web-facing protocol) are real, anticipated additions — but their
/// addressing needs are different enough from "a window" or "a game" (an
/// SSH target needs a host/user/command, not a window handle) that adding
/// a guessed-at variant now, before any such backend exists, would likely
/// just need reshaping later. Add the variant when the backend that needs
/// it actually gets built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamTarget {
    Window(WindowId),
    Game(GameId),
    Desktop,
}

/// Video codecs a session track can carry. The client lists the ones it
/// can decode in hardware; the host picks the first it can produce (or,
/// for passthrough, the one the media already is).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
}

/// Which backend carries one stream. Every backend is windowcast's own
/// implementation inside this library, on both ends; none wraps another
/// project's client or server (see docs/BACKENDS.md). Many streams with
/// different backends can be live on one session at once: this is per
/// [`StreamTarget`], not a session-wide mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamBackend {
    /// The window is captured and encoded by the host agent and sent as a
    /// video track on this session. The default for everything.
    Native { codec: VideoCodec },
    /// Video the window is already playing, forwarded as it was encoded on
    /// a video track of this session: no second encode. Not built yet.
    Passthrough { codec: VideoCodec },
    /// Cut out of a capture of the whole desktop, for hosts that cannot
    /// capture single windows, and for [`StreamTarget::Desktop`]. A video
    /// track on this session. Not built yet.
    Desktop { codec: VideoCodec },
    /// windowcast's own GameStream implementation, for games: its own
    /// low-latency video, audio and controller channels to the
    /// [`HandoffTarget`]. Not built yet.
    GameStream,
    /// RDP (on IronRDP's protocol crates), for text-heavy windows: the
    /// host serves the window over RDP at the [`HandoffTarget`], with a
    /// login made for the stream.
    Rdp,
    /// windowcast's own VNC implementation. Not built yet.
    Vnc,
    /// Anything not yet a first-class variant — carries a protocol name so
    /// experimental backends don't need a protocol version bump to exist,
    /// at the cost of no compile-time guarantee any given peer implements it.
    Other(String),
}

impl StreamBackend {
    pub fn kind(&self) -> BackendKind {
        match self {
            StreamBackend::Native { .. } => BackendKind::Native,
            StreamBackend::Passthrough { .. } => BackendKind::Passthrough,
            StreamBackend::Desktop { .. } => BackendKind::Desktop,
            StreamBackend::GameStream => BackendKind::GameStream,
            StreamBackend::Rdp => BackendKind::Rdp,
            StreamBackend::Vnc => BackendKind::Vnc,
            StreamBackend::Other(_) => BackendKind::Other,
        }
    }

    /// The codec of the video track this backend sends on this session,
    /// or `None` for a backend with its own connection.
    pub fn session_codec(&self) -> Option<VideoCodec> {
        match self {
            StreamBackend::Native { codec }
            | StreamBackend::Passthrough { codec }
            | StreamBackend::Desktop { codec } => Some(*codec),
            _ => None,
        }
    }
}

/// [`StreamBackend`] without its parameters: what selection rules and
/// user overrides name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BackendKind {
    Native,
    Passthrough,
    Desktop,
    GameStream,
    Rdp,
    Vnc,
    Other,
}

/// Where to reach a backend that runs its own connection (GameStream,
/// RDP, VNC): normally the same machine as the host agent, on its own port,
/// with a login made for this one stream and the backend's certificate to
/// pin, both handed over this paired session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffTarget {
    /// Empty for "the address this session reached the host at".
    pub address: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    /// SHA-256 of the backend's TLS certificate.
    pub certificate_sha256: Option<[u8; 32]>,
}

/// What the client asks for with a stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamOptions {
    /// The backend the client's rules chose ([`selection::choose_backend`]).
    /// The host uses it if it can serve it and falls back to `Native`
    /// otherwise ([`selection::serve`]); the response says which it used.
    pub backend: BackendKind,
    /// Codecs the client decodes, most preferred first.
    pub codecs: Vec<VideoCodec>,
    /// The client's own ceilings for this stream; the host adapts below
    /// them (and below its own settings) to what the network carries.
    pub limits: StreamLimits,
}

/// A client's ceilings for one stream, each `None` for "no limit of mine".
/// The host never goes above them, and goes below them when the network
/// cannot carry them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamLimits {
    /// Kilobits a second.
    pub max_bitrate_kbps: Option<u32>,
    pub max_fps: Option<u32>,
    /// Picture height; the width follows at the window's shape.
    pub max_height: Option<u32>,
}

/// What a stream is being sent at now, and the network it is sent over,
/// as the host sees it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamQuality {
    /// The rate the host holds the encoder to, kilobits a second; `None`
    /// while only the settings and limits hold it (nothing to adapt to).
    pub target_kbps: Option<u32>,
    /// What actually went out over the last second or so.
    pub sent_kbps: u32,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    /// Packets the client lost, percent, from its receiver reports.
    pub loss_percent: f32,
    /// Round trip over the session, milliseconds.
    pub rtt_ms: Option<u32>,
}

/// Control-channel request/response traffic, independent of the actual
/// media tracks carrying encoded frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMessage {
    ListWindowsRequest,
    ListWindowsResponse(Vec<WindowInfo>),

    /// Games the host can stream through its GameStream backend.
    ListGamesRequest,
    ListGamesResponse(Vec<GameEntry>),

    /// Client asks to start streaming a window, a game or the desktop. The
    /// agent may require a one-time host-user approval before answering
    /// (see the security model's authorization section) — this can be a
    /// slow round-trip, not just a lookup.
    ///
    /// A window's stream may have several carriers at once while it
    /// switches (docs/BACKENDS.md, "One window, any carrier"): each start
    /// names a `generation`, the client's number for that carrier, and a
    /// second start of a window while its first runs is a switch. Carriers
    /// running at once differ in backend.
    StreamStartRequest {
        target: StreamTarget,
        options: StreamOptions,
        generation: u32,
    },
    StreamStartResponse {
        target: StreamTarget,
        generation: u32,
        accepted: bool,
        backend: StreamBackend,
        /// Session track id the video arrives on, for backends that send on
        /// this session ([`StreamBackend::session_codec`] is `Some`).
        track_id: Option<String>,
        /// Where to connect, for backends with their own connection.
        handoff: Option<HandoffTarget>,
        reason: Option<String>,
    },

    /// Client: stop the stream, every carrier of it.
    StreamStopRequest(StreamTarget),
    /// Client: stop one carrier of a window's stream, the one a switch
    /// replaced. The stream goes on on its others; stopping the last one
    /// ends it.
    CarrierStop {
        window: WindowId,
        generation: u32,
    },
    /// Host: the stream ended (stopped, or the window closed).
    StreamStopped(StreamTarget),
    /// Transport: a window's video track on the session ended. Its stream
    /// may go on on another carrier; `StreamStopped` says when it ends.
    TrackEnded(WindowId),

    Input(InputEvent),

    /// The clipboard's text, both ways: the client sends it when its
    /// clipboard changes (the host sets its own), the host when its
    /// clipboard changes.
    Clipboard(String),

    /// Either side may ping; the other answers at once.
    Ping,
    Pong,

    /// The client changes its ceilings for a stream it watches.
    StreamLimits {
        window: WindowId,
        limits: StreamLimits,
    },
    /// The host reports a stream's quality when it changes and every few
    /// seconds.
    StreamQuality {
        window: WindowId,
        quality: StreamQuality,
    },

    /// SDP renegotiation once the session is up (a window track attached or
    /// detached). It rides the control channel, which is already inside the
    /// DTLS session both sides authenticated at connect time, so it needs no
    /// signature of its own. Handled inside `windowcast-transport`; an
    /// embedder never sees it.
    SessionDescription {
        kind: SdpKind,
        sdp: String,
    },

    /// A streamed window changed size on the host. The video track's own
    /// bitstream also carries the new size; this lets the client resize its
    /// surface before the first frame at the new size arrives.
    WindowResized {
        window: WindowId,
        width: u32,
        height: u32,
    },
    /// Which streamed window has keyboard focus on the host now.
    WindowFocused(WindowId),

    /// The sender is closing the session. Handled inside
    /// `windowcast-transport`: the receiver's session ends at once instead
    /// of waiting for the connection to time out.
    Goodbye,

    /// The client asks for an OpenSSH user certificate for its SSH key
    /// (an OpenSSH public key line), for an account it signed in with
    /// (docs/ACCOUNTS.md, "SSH and the command stream").
    SshCertificateRequest {
        public_key: String,
    },
    /// The certificate (an OpenSSH certificate line), or why there is none.
    SshCertificateResponse(Result<String, String>),

    /// This side's discovery ID, for finding it away from the LAN later
    /// (`windowcast-transport`'s `remote`). Each side sends its own when a
    /// session starts; the other keeps it with the pinned identity.
    Rendezvous {
        discovery_id: String,
    },

    /// The command stream (docs/COMMAND-STREAM.md): shells, commands and
    /// application launches on the host. Authorized by the host separately
    /// from streaming windows, because a shell is far more than a picture.
    Command(command::CommandMessage),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SdpKind {
    Offer,
    Answer,
}

/// Which credential a connecting client presents during signaling. See
/// docs/SECURITY.md: `Pair` runs the PIN-seeded PAKE once and pins both
/// identities; `Resume` relies on identities pinned by an earlier pairing
/// or registered by an earlier sign-in; `Account` signs in with an account
/// (docs/ACCOUNTS.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectMode {
    Pair,
    Resume,
    Account,
}

/// A kind of account sign-in a host takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignInMethod {
    /// A user name and password the host checks (its own accounts, the
    /// OS's, or a directory's).
    Password,
    /// An ID token from one of the host's OpenID Connect providers.
    Oidc,
    /// A Kerberos ticket for the host's service principal.
    Kerberos,
}

/// An OpenID Connect provider a host accepts, as clients need it to sign
/// in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OidcProviderInfo {
    pub name: String,
    pub issuer: String,
    /// windowcast's client id at the provider (a public client).
    pub client_id: String,
    /// Scopes to ask for besides `openid`.
    pub scopes: Vec<String>,
}

/// What a host offers a client that came to sign in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountOffer {
    pub methods: Vec<SignInMethod>,
    pub providers: Vec<OidcProviderInfo>,
    /// The Kerberos service principal to ask a ticket for.
    pub kerberos_service: Option<String>,
    /// The host's HPKE public key for this sign-in (X25519).
    pub seal_key: Vec<u8>,
}

/// A client's account credential. Only ever sent sealed to the host
/// ([`SignalMessage::AccountProof`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AccountCredential {
    Password { username: String, password: String },
    Oidc { provider: String, id_token: String },
    Kerberos { token: Vec<u8> },
}

/// Messages on the signaling channel: whatever carries the very first
/// offer/answer before any WebRTC connection exists (a LAN TCP socket
/// today). The channel itself is NOT trusted; every description it carries
/// is signed by the sender's persistent identity and, when pairing,
/// HMAC-tagged with the PIN-derived key, over a transcript that binds both
/// peers' identities and fresh nonces. See `windowcast-transport::signaling`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SignalMessage {
    Hello {
        version: u16,
        /// The sender's Ed25519 public key (`windowcast-identity::PeerId`).
        peer_id: [u8; 32],
        /// Fresh per connection, so a recorded handshake cannot be replayed.
        nonce: [u8; 32],
        /// Only meaningful from the client; the host echoes the client's.
        mode: ConnectMode,
    },
    /// One SPAKE2 message (pairing only).
    Pake(Vec<u8>),
    /// Signing in with an account: the host's offer, signed by its
    /// identity over the transcript so far.
    AccountOffer {
        offer: AccountOffer,
        signature: Vec<u8>,
    },
    /// The client's [`AccountCredential`], sealed to the offer's key.
    AccountProof {
        encapsulated: Vec<u8>,
        sealed: Vec<u8>,
    },
    /// The host took the sign-in (a refusal is a [`SignalMessage::Reject`]),
    /// so the client learns the verdict before it gathers its offer.
    AccountAccepted,
    Description {
        kind: SdpKind,
        sdp: String,
        /// Ed25519 signature (64 bytes) over the signaling transcript.
        signature: Vec<u8>,
        /// HMAC-SHA256 of the same transcript under the PIN-derived key;
        /// present only while pairing.
        pin_tag: Option<[u8; 32]>,
    },
    /// The peer refuses the session. The text is for logs; it never says
    /// which check failed beyond "authentication failed", so it is no
    /// oracle for a PIN guesser.
    Reject(String),
}

/// The bytes an [`AccountOffer`]'s signature covers, and a credential's
/// before it is sealed.
pub fn encode_offer(offer: &AccountOffer) -> Result<Vec<u8>, ProtocolError> {
    Ok(bincode::serialize(offer)?)
}

pub fn encode_credential(credential: &AccountCredential) -> Result<Vec<u8>, ProtocolError> {
    Ok(bincode::serialize(credential)?)
}

pub fn decode_credential(bytes: &[u8]) -> Result<AccountCredential, ProtocolError> {
    Ok(bincode::deserialize(bytes)?)
}

pub fn encode_signal(message: &SignalMessage) -> Result<Vec<u8>, ProtocolError> {
    Ok(bincode::serialize(message)?)
}

pub fn decode_signal(bytes: &[u8]) -> Result<SignalMessage, ProtocolError> {
    let message: SignalMessage = bincode::deserialize(bytes)?;
    if let SignalMessage::Hello { version, .. } = &message {
        if *version != PROTOCOL_VERSION {
            return Err(ProtocolError::VersionMismatch {
                found: *version,
                expected: PROTOCOL_VERSION,
            });
        }
    }
    Ok(message)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub version: u16,
    pub message: ControlMessage,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("unsupported protocol version {found}, expected {expected}")]
    VersionMismatch { found: u16, expected: u16 },
    #[error("failed to decode message: {0}")]
    Decode(#[from] bincode::Error),
}

/// Encode a [`ControlMessage`] as a length-free binary frame. The caller
/// (transport layer) is responsible for framing (WebRTC data channel
/// messages are already message-oriented, so no length prefix is needed
/// there).
pub fn encode(message: &ControlMessage) -> Result<Vec<u8>, ProtocolError> {
    let envelope = Envelope {
        version: PROTOCOL_VERSION,
        message: message.clone(),
    };
    Ok(bincode::serialize(&envelope)?)
}

pub fn decode(bytes: &[u8]) -> Result<ControlMessage, ProtocolError> {
    let envelope: Envelope = bincode::deserialize(bytes)?;
    if envelope.version != PROTOCOL_VERSION {
        return Err(ProtocolError::VersionMismatch {
            found: envelope.version,
            expected: PROTOCOL_VERSION,
        });
    }
    Ok(envelope.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_window_list() {
        let msg = ControlMessage::ListWindowsResponse(vec![WindowInfo {
            id: WindowId(7),
            title: "Terminal".into(),
            app_id: "org.example.term".into(),
            width: 800,
            height: 600,
            focused: true,
            owner: None,
            kind: WindowKind::Normal,
            position: None,
            content: ContentHint::Text,
        }]);
        let bytes = encode(&msg).unwrap();
        assert_eq!(decode(&bytes).unwrap(), msg);
    }

    #[test]
    fn a_window_stream_and_a_game_handoff_are_independent_targets() {
        // windowcast's core premise: many streams can be live at once, not
        // just one at a time -- a native window track and a GameStream
        // handoff are independently started/stopped, each keeping its own
        // response shape.
        let window_response = ControlMessage::StreamStartResponse {
            generation: 1,
            target: StreamTarget::Window(WindowId(3)),
            accepted: true,
            backend: StreamBackend::Native {
                codec: VideoCodec::H265,
            },
            track_id: Some("track-3".into()),
            handoff: None,
            reason: None,
        };
        let game_response = ControlMessage::StreamStartResponse {
            generation: 1,
            target: StreamTarget::Game(GameId(101)),
            accepted: true,
            backend: StreamBackend::GameStream,
            track_id: None,
            handoff: Some(HandoffTarget {
                address: "127.0.0.1".into(),
                port: 47989,
                username: String::new(),
                password: String::new(),
                certificate_sha256: None,
            }),
            reason: None,
        };

        let window_bytes = encode(&window_response).unwrap();
        let game_bytes = encode(&game_response).unwrap();

        assert_eq!(decode(&window_bytes).unwrap(), window_response);
        assert_eq!(decode(&game_bytes).unwrap(), game_response);
        assert_ne!(window_response, game_response);
    }

    #[test]
    fn stream_backend_is_extensible_without_a_protocol_version_bump() {
        let rdp = StreamBackend::Rdp;
        let experimental = StreamBackend::Other("web-vnc-poc".into());

        let rdp_msg = ControlMessage::StreamStartResponse {
            generation: 1,
            target: StreamTarget::Window(WindowId(9)),
            accepted: true,
            backend: rdp,
            track_id: None,
            handoff: Some(HandoffTarget {
                address: "10.0.0.5".into(),
                port: 3389,
                username: "windowcast".into(),
                password: "one-time".into(),
                certificate_sha256: Some([7; 32]),
            }),
            reason: None,
        };
        let experimental_msg = ControlMessage::StreamStartResponse {
            generation: 1,
            target: StreamTarget::Window(WindowId(10)),
            accepted: true,
            backend: experimental,
            track_id: None,
            handoff: Some(HandoffTarget {
                address: "10.0.0.5".into(),
                port: 8080,
                username: String::new(),
                password: String::new(),
                certificate_sha256: None,
            }),
            reason: None,
        };

        assert_eq!(decode(&encode(&rdp_msg).unwrap()).unwrap(), rdp_msg);
        assert_eq!(
            decode(&encode(&experimental_msg).unwrap()).unwrap(),
            experimental_msg
        );
    }

    #[test]
    fn signal_messages_round_trip_and_hello_checks_the_version() {
        let hello = SignalMessage::Hello {
            version: PROTOCOL_VERSION,
            peer_id: [1; 32],
            nonce: [2; 32],
            mode: ConnectMode::Pair,
        };
        assert_eq!(
            decode_signal(&encode_signal(&hello).unwrap()).unwrap(),
            hello
        );

        let old = SignalMessage::Hello {
            version: PROTOCOL_VERSION - 1,
            peer_id: [1; 32],
            nonce: [2; 32],
            mode: ConnectMode::Resume,
        };
        assert!(matches!(
            decode_signal(&encode_signal(&old).unwrap()),
            Err(ProtocolError::VersionMismatch { .. })
        ));
    }

    #[test]
    fn account_messages_round_trip() {
        let offer = SignalMessage::AccountOffer {
            offer: AccountOffer {
                methods: vec![SignInMethod::Password, SignInMethod::Oidc],
                providers: vec![OidcProviderInfo {
                    name: "corp".into(),
                    issuer: "https://id.example.org".into(),
                    client_id: "windowcast".into(),
                    scopes: vec!["groups".into()],
                }],
                kerberos_service: Some("host/h.example.org".into()),
                seal_key: vec![5; 32],
            },
            signature: vec![6; 64],
        };
        assert_eq!(
            decode_signal(&encode_signal(&offer).unwrap()).unwrap(),
            offer
        );
        let proof = SignalMessage::AccountProof {
            encapsulated: vec![1; 32],
            sealed: vec![2; 40],
        };
        assert_eq!(
            decode_signal(&encode_signal(&proof).unwrap()).unwrap(),
            proof
        );
        let credential = AccountCredential::Oidc {
            provider: "corp".into(),
            id_token: "a.b.c".into(),
        };
        assert_eq!(
            decode_credential(&encode_credential(&credential).unwrap()).unwrap(),
            credential
        );
        let request = ControlMessage::SshCertificateRequest {
            public_key: "ssh-ed25519 AAAA".into(),
        };
        assert_eq!(decode(&encode(&request).unwrap()).unwrap(), request);
    }

    #[test]
    fn input_and_clipboard_round_trip() {
        let messages = [
            ControlMessage::Input(InputEvent::PointerButton {
                window: WindowId(3),
                button: PointerButton::Right,
                pressed: true,
            }),
            ControlMessage::Input(InputEvent::Text {
                text: "héllo".into(),
            }),
            ControlMessage::Input(InputEvent::Gamepad {
                pad: 1,
                state: GamepadState {
                    buttons: GamepadButtons::A | GamepadButtons::DPAD_LEFT,
                    left_x: -32768,
                    right_trigger: 255,
                    ..Default::default()
                },
            }),
            ControlMessage::Clipboard("copied".into()),
        ];
        for message in messages {
            assert_eq!(decode(&encode(&message).unwrap()).unwrap(), message);
        }
    }

    #[test]
    fn rejects_a_future_protocol_version() {
        let envelope = Envelope {
            version: PROTOCOL_VERSION + 1,
            message: ControlMessage::Ping,
        };
        let bytes = bincode::serialize(&envelope).unwrap();
        assert!(matches!(
            decode(&bytes),
            Err(ProtocolError::VersionMismatch { .. })
        ));
    }
}

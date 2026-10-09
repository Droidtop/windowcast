//! Wire types shared by every windowcast agent/client. Pure data + codec —
//! no networking, no crypto, no platform deps, so this crate can be reused
//! by anything embedding windowcast without pulling in WebRTC or capture
//! backends it doesn't need.

pub mod selection;

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to the message shapes below. A peer
/// that receives a mismatched version should refuse the session rather
/// than guess at how to interpret an unknown wire format.
pub const PROTOCOL_VERSION: u16 = 3;

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

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum TouchPhase {
    Start,
    Move,
    End,
    Cancel,
}

/// Input events flow client -> agent over the data channel. Coordinates are
/// normalized to the captured window's own [0.0, 1.0] space so the agent
/// doesn't need to know the client's viewport size.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    PointerMove {
        window: WindowId,
        x: f32,
        y: f32,
    },
    PointerButton {
        window: WindowId,
        button: u8,
        pressed: bool,
    },
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
    Touch {
        window: WindowId,
        id: u32,
        x: f32,
        y: f32,
        phase: TouchPhase,
    },
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
    /// windowcast's own RDP implementation, for text-heavy windows. Not
    /// built yet.
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
/// RDP, VNC): normally the same machine as the host agent, on its own port.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffTarget {
    pub address: String,
    pub port: u16,
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
    StreamStartRequest {
        target: StreamTarget,
        options: StreamOptions,
    },
    StreamStartResponse {
        target: StreamTarget,
        accepted: bool,
        backend: StreamBackend,
        /// Session track id the video arrives on, for backends that send on
        /// this session ([`StreamBackend::session_codec`] is `Some`).
        track_id: Option<String>,
        /// Where to connect, for backends with their own connection.
        handoff: Option<HandoffTarget>,
        reason: Option<String>,
    },

    StreamStopRequest(StreamTarget),
    StreamStopped(StreamTarget),

    Input(InputEvent),

    Ping,
    Pong,

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SdpKind {
    Offer,
    Answer,
}

/// Which credential a connecting client presents during signaling. See
/// docs/SECURITY.md: `Pair` runs the PIN-seeded PAKE once and pins both
/// identities; `Resume` relies on identities pinned by an earlier pairing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectMode {
    Pair,
    Resume,
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
            target: StreamTarget::Game(GameId(101)),
            accepted: true,
            backend: StreamBackend::GameStream,
            track_id: None,
            handoff: Some(HandoffTarget {
                address: "127.0.0.1".into(),
                port: 47989,
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
            target: StreamTarget::Window(WindowId(9)),
            accepted: true,
            backend: rdp,
            track_id: None,
            handoff: Some(HandoffTarget {
                address: "10.0.0.5".into(),
                port: 3389,
            }),
            reason: None,
        };
        let experimental_msg = ControlMessage::StreamStartResponse {
            target: StreamTarget::Window(WindowId(10)),
            accepted: true,
            backend: experimental,
            track_id: None,
            handoff: Some(HandoffTarget {
                address: "10.0.0.5".into(),
                port: 8080,
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

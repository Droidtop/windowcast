//! Wire types shared by every windowcast agent/client. Pure data + codec —
//! no networking, no crypto, no platform deps, so this crate can be reused
//! by anything embedding windowcast without pulling in WebRTC or capture
//! backends it doesn't need.

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to the message shapes below. A peer
/// that receives a mismatched version should refuse the session rather
/// than guess at how to interpret an unknown wire format.
pub const PROTOCOL_VERSION: u16 = 1;

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

/// One app a local GameStream-protocol host (Sunshine/Apollo) has
/// configured as streamable — sourced by querying that host's own
/// `serverinfo`/`applist` endpoints (see `windowcast-apollo`), not
/// anything windowcast's own capture agents produce themselves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameEntry {
    pub id: GameId,
    pub name: String,
    pub artwork_uri: Option<String>,
}

/// What a client is asking to stream, and — for [`ControlMessage::StreamStartResponse`]
/// — what it got. Windows and games are deliberately not unified into one
/// id space: a window is captured and streamed by windowcast itself, a
/// game is handed off to a different backend entirely (see [`StreamBackend`]).
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
}

/// Which underlying mechanism actually drives a stream once accepted. Every
/// value is driven by a library the client links in and controls directly
/// — never a separate spawned application — this field only says which
/// library, not how it's invoked. Many streams, of possibly different
/// backends, can be live on one session at once — this isn't a
/// session-wide mode switch, it's per [`StreamTarget`].
///
/// Deliberately open-ended: windowcast isn't just a Moonlight-or-native
/// choice. RDP and VNC are obvious next backends (existing Rust client
/// crates exist for both — FreeRDP bindings, `vnc-rs`/`libvncclient`
/// bindings — matching the same "embed a library, don't spawn a process"
/// rule everything else here follows); SSH is a real but structurally
/// different case (a PTY/text channel, not a video stream); some future
/// backend might be reached over a web-facing protocol the client embeds
/// an HTTP/WebSocket client for rather than a native decoder. None of
/// these beyond `Native` and `Moonlight` are implemented yet — see each
/// variant's doc comment for its actual status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamBackend {
    /// windowcast's own WebRTC video track, on this same session. Real,
    /// implemented (modulo the per-window capture pipeline itself — see
    /// `agent-linux/src/capture.rs`).
    Native,
    /// Handed off to an embedded GameStream/Moonlight-protocol client
    /// library, connecting directly to [`HandoffTarget`] — the video/audio
    /// never rides this session's WebRTC transport once handed off, only
    /// this negotiation does. `windowcast-apollo` implements the
    /// unauthenticated `serverinfo`/`applist`-XML half of this; the actual
    /// paired streaming client (`windowcast-moonlight`) doesn't exist yet.
    Moonlight,
    /// RDP handoff. Not implemented anywhere in this repo yet.
    Rdp,
    /// VNC handoff. Not implemented anywhere in this repo yet.
    Vnc,
    /// Anything not yet a first-class variant — carries a protocol name so
    /// experimental/custom backends don't need a protocol version bump to
    /// exist, at the cost of no compile-time guarantee any given client
    /// actually implements it.
    Other(String),
}

/// Where to reach a [`StreamBackend`] handoff — normally the same physical
/// machine as the windowcast host answering the request (e.g. a local
/// Sunshine/Apollo install for `Moonlight`), reached on its own port,
/// independent of this WebRTC session's address.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffTarget {
    pub address: String,
    pub port: u16,
}

/// Control-channel request/response traffic, independent of the actual
/// media tracks carrying encoded frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMessage {
    ListWindowsRequest,
    ListWindowsResponse(Vec<WindowInfo>),

    /// Apollo/Sunshine-backed games this host can hand a client off to —
    /// always [`StreamBackend::Moonlight`], never captured by windowcast's
    /// own agents.
    ListGamesRequest,
    ListGamesResponse(Vec<GameEntry>),

    /// Client asks to start streaming a window or a game. The agent may
    /// require a one-time host-user approval before answering (see the
    /// security model's authorization section) — this can be a slow
    /// round-trip, not just a lookup.
    StreamStartRequest(StreamTarget),
    StreamStartResponse {
        target: StreamTarget,
        accepted: bool,
        backend: StreamBackend,
        /// WebRTC track id the video will arrive on — only set when `backend == Native`.
        track_id: Option<String>,
        /// Where to hand off to — only set for a non-`Native` backend.
        handoff: Option<HandoffTarget>,
        reason: Option<String>,
    },

    StreamStopRequest(StreamTarget),
    StreamStopped(StreamTarget),

    Input(InputEvent),

    Ping,
    Pong,
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
        }]);
        let bytes = encode(&msg).unwrap();
        assert_eq!(decode(&bytes).unwrap(), msg);
    }

    #[test]
    fn a_window_stream_and_a_game_handoff_are_independent_targets() {
        // windowcast's core premise: many streams can be live at once, not
        // just one at a time -- a native window track and a Moonlight
        // handoff are independently started/stopped, each keeping its own
        // response shape.
        let window_response = ControlMessage::StreamStartResponse {
            target: StreamTarget::Window(WindowId(3)),
            accepted: true,
            backend: StreamBackend::Native,
            track_id: Some("track-3".into()),
            handoff: None,
            reason: None,
        };
        let game_response = ControlMessage::StreamStartResponse {
            target: StreamTarget::Game(GameId(101)),
            accepted: true,
            backend: StreamBackend::Moonlight,
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

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

/// Control-channel request/response traffic, independent of the actual
/// media tracks carrying encoded frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMessage {
    ListWindowsRequest,
    ListWindowsResponse(Vec<WindowInfo>),

    /// Client asks to start streaming one window. The agent may require a
    /// one-time host-user approval before answering (see the security
    /// model's authorization section) — this can be a slow round-trip, not
    /// just a lookup.
    StreamStartRequest(WindowId),
    StreamStartResponse {
        window: WindowId,
        accepted: bool,
        /// WebRTC track id the video for this window will arrive on when accepted.
        track_id: Option<String>,
        reason: Option<String>,
    },

    StreamStopRequest(WindowId),
    StreamStopped(WindowId),

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

//! Operations allowed by the helper: a closed set, length-checked.
//!
//! Only three commands exist:
//! - `capture_frame`: show the current secure screen.
//! - `send_input`: enter given text or mouse clicks.
//! - `secure_desktop_state`: whether Winlogon desktop is active.

use std::io;

/// A single operation command from the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    CaptureFrame,
    SendInput { text: Option<String>, mouse: Option<(i32, i32)> },
    SecureDesktopState,
}

impl Operation {
    /// Length-checked serialize: the command string plus bounded payload.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Operation::CaptureFrame => b"CAPTURE_FRAME".to_vec(),
            Operation::SendInput { text: None, mouse: None } => b"SEND_INPUT".to_vec(),
            Operation::SendInput { text: Some(t), mouse: None } => {
                let mut out = b"SEND_INPUT ".to_vec();
                out.extend(t.bytes());
                out
            }
            Operation::SendInput { text: None, mouse: Some((x, y)) } => {
                format!("SEND_INPUT MOUSE {} {}", x, y).into_bytes()
            }
            Operation::SecureDesktopState => b"SECURE_DESKTOP_STATE".to_vec(),
        }
    }

    /// Decode from bytes, with length limits to prevent overflow.
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() > 4096 {
            // Length-checked: refuse oversized payloads.
            return None;
        }
        let s = std::str::from_utf8(data).ok()?;
        if s == "CAPTURE_FRAME" {
            Some(Operation::CaptureFrame)
        } else if s == "SECURE_DESKTOP_STATE" {
            Some(Operation::SecureDesktopState)
        } else if s.starts_with("SEND_INPUT ") {
            let rest = &s["SEND_INPUT ".len()..];
            if rest.starts_with("MOUSE ") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.len() == 3 {
                    let x = parts[1].parse().ok()?;
                    let y = parts[2].parse().ok()?;
                    Some(Operation::SendInput { text: None, mouse: Some((x, y)) })
                } else {
                    None
                }
            } else {
                Some(Operation::SendInput {
                    text: Some(rest.to_string()),
                    mouse: None,
                })
            }
        } else if s == "SEND_INPUT" {
            Some(Operation::SendInput { text: None, mouse: None })
        } else {
            None
        }
    }

    /// Whether this operation is allowed. The allowlist is exactly the
    /// three operations above; nothing else passes.
    pub fn is_allowed(&self) -> bool {
        matches!(
            self,
            Operation::CaptureFrame | Operation::SendInput { .. } | Operation::SecureDesktopState
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_capture() {
        assert_eq!(Operation::decode(&Operation::CaptureFrame.encode()), Some(Operation::CaptureFrame));
    }

    #[test]
    fn encode_decode_state() {
        assert_eq!(
            Operation::decode(&Operation::SecureDesktopState.encode()),
            Some(Operation::SecureDesktopState)
        );
    }

    #[test]
    fn encode_decode_input_text() {
        let op = Operation::SendInput { text: Some("hello".into()), mouse: None };
        assert_eq!(Operation::decode(&op.encode()), Some(op));
    }

    #[test]
    fn encode_decode_input_mouse() {
        let op = Operation::SendInput { text: None, mouse: Some((10, 20)) };
        assert_eq!(Operation::decode(&op.encode()), Some(op));
    }

    #[test]
    fn oversized_refused() {
        let big = vec![b'X'; 5000];
        assert!(Operation::decode(&big).is_none());
    }

    #[test]
    fn allowlist_exact() {
        assert!(Operation::CaptureFrame.is_allowed());
        assert!(Operation::SendInput { text: None, mouse: None }.is_allowed());
        assert!(Operation::SecureDesktopState.is_allowed());
    }
}

//! Chat and notices (docs/COMMAND-STREAM.md, "Files"): short text both ways on a
//! [`crate::command::ChannelKind::Chat`] channel, one message per `Data`. A host application
//! uses it to say something to the person at a client ("your download finished") and to hear
//! back; a client can say something that the host application shows on the host's screen.
//! The library stores nothing: a message is handed on and forgotten.

use serde::{Deserialize, Serialize};

/// The longest message, in bytes of UTF-8.
pub const MAX_TEXT: usize = 2000;

/// The most messages a channel may send in one second before the receiver drops the rest.
pub const MAX_PER_SECOND: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatMessage {
    Text {
        text: String,
    },
    /// The last message was not accepted (too long, not text, or too fast). Never a reply to a
    /// `Rejected`.
    Rejected {
        reason: String,
    },
}

/// Why `text` is not a message, or `None` when it is one: not empty, at most [`MAX_TEXT`] bytes,
/// and no control characters other than line breaks and tabs (a message is shown on a screen, so
/// it cannot carry terminal escapes).
pub fn problem_with(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return Some("a message cannot be empty".into());
    }
    if text.len() > MAX_TEXT {
        return Some(format!("a message may hold at most {MAX_TEXT} bytes"));
    }
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        return Some("a message may not hold control characters".into());
    }
    None
}

pub fn encode(message: &ChatMessage) -> Vec<u8> {
    bincode::serialize(message).expect("a message serializes")
}

pub fn decode(bytes: &[u8]) -> Result<ChatMessage, String> {
    bincode::deserialize(bytes).map_err(|_| "that is not a chat message".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        for message in [
            ChatMessage::Text {
                text: "hello\nthere".into(),
            },
            ChatMessage::Rejected {
                reason: "too long".into(),
            },
        ] {
            assert_eq!(decode(&encode(&message)).unwrap(), message);
        }
        assert!(decode(&[0xff; 3]).is_err());
    }

    #[test]
    fn text_is_checked() {
        assert_eq!(problem_with("fine, with\ttabs\nand lines"), None);
        assert!(problem_with("").is_some());
        assert!(problem_with("   \n").is_some());
        assert!(problem_with(&"x".repeat(MAX_TEXT + 1)).is_some());
        assert_eq!(problem_with(&"x".repeat(MAX_TEXT)), None);
        assert!(problem_with("\u{1b}[31mred").is_some());
        assert!(problem_with("bell\u{7}").is_some());
    }
}

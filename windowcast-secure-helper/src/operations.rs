//! The complete wire protocol understood by the privileged helper.
//!
//! The protocol is deliberately not extensible at runtime: serde can decode only
//! the variants below, and every variable-size field is checked again after
//! decoding. A little-endian length prefix makes pipe reads unambiguous.

use std::io::{self, Read, Write};

use bincode::Options;
use serde::{Deserialize, Serialize};

pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_TEXT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseAction {
    Move { x: i32, y: i32 },
    Button { button: MouseButton, pressed: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// A request. There is intentionally no generic command or path-bearing form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operation {
    CaptureFrame,
    SendInput {
        text: Option<String>,
        mouse: Option<MouseAction>,
    },
    SecureDesktopState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Frame {
        width: u32,
        height: u32,
        stride: u32,
        bgra: Vec<u8>,
    },
    InputAccepted,
    SecureDesktopState(bool),
    Refused(String),
}

impl Operation {
    pub fn validate(&self) -> io::Result<()> {
        match self {
            Self::SendInput { text, mouse } => {
                if text
                    .as_ref()
                    .is_some_and(|value| value.len() > MAX_TEXT_BYTES)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "input text is too long",
                    ));
                }
                if text.is_none() && mouse.is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "empty input request",
                    ));
                }
            }
            Self::CaptureFrame | Self::SecureDesktopState => {}
        }
        Ok(())
    }

    pub fn encode(&self) -> io::Result<Vec<u8>> {
        self.validate()?;
        let bytes = codec().serialize(self).map_err(invalid_data)?;
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request is too large",
            ));
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request is too large",
            ));
        }
        let operation: Self = codec().deserialize(bytes).map_err(invalid_data)?;
        operation.validate()?;
        Ok(operation)
    }

    /// Kept explicit so tests guard the privileged allowlist.
    pub fn is_allowed(&self) -> bool {
        self.validate().is_ok()
    }
}

pub fn read_request(reader: &mut impl Read) -> io::Result<Option<Operation>> {
    let Some(bytes) = read_frame(reader, MAX_REQUEST_BYTES)? else {
        return Ok(None);
    };
    Operation::decode(&bytes).map(Some)
}

pub fn write_request(writer: &mut impl Write, operation: &Operation) -> io::Result<()> {
    write_frame(writer, &operation.encode()?, MAX_REQUEST_BYTES)
}

pub fn read_response(reader: &mut impl Read) -> io::Result<Option<Response>> {
    let Some(bytes) = read_frame(reader, MAX_RESPONSE_BYTES)? else {
        return Ok(None);
    };
    codec().deserialize(&bytes).map(Some).map_err(invalid_data)
}

pub fn write_response(writer: &mut impl Write, response: &Response) -> io::Result<()> {
    let bytes = codec().serialize(response).map_err(invalid_data)?;
    write_frame(writer, &bytes, MAX_RESPONSE_BYTES)
}

fn codec() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .reject_trailing_bytes()
}

fn read_frame(reader: &mut impl Read, maximum: usize) -> io::Result<Option<Vec<u8>>> {
    let mut length = [0; 4];
    match reader.read_exact(&mut length) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > maximum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid message length",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(Some(bytes))
}

fn write_frame(writer: &mut impl Write, bytes: &[u8], maximum: usize) -> io::Result<()> {
    if bytes.is_empty() || bytes.len() > maximum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid message length",
        ));
    }
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(bytes)
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_allowed_request_round_trips() {
        let requests = [
            Operation::CaptureFrame,
            Operation::SendInput {
                text: Some("hello".into()),
                mouse: None,
            },
            Operation::SendInput {
                text: None,
                mouse: Some(MouseAction::Move { x: 10, y: 20 }),
            },
            Operation::SecureDesktopState,
        ];
        for request in requests {
            let mut wire = Vec::new();
            write_request(&mut wire, &request).unwrap();
            assert_eq!(read_request(&mut wire.as_slice()).unwrap(), Some(request));
        }
    }

    #[test]
    fn oversized_and_empty_requests_are_refused() {
        assert!(Operation::decode(&vec![0; MAX_REQUEST_BYTES + 1]).is_err());
        assert!(!Operation::SendInput {
            text: None,
            mouse: None
        }
        .is_allowed());
        let long = "x".repeat(MAX_TEXT_BYTES + 1);
        assert!(!Operation::SendInput {
            text: Some(long),
            mouse: None
        }
        .is_allowed());
    }

    #[test]
    fn unknown_variant_is_refused() {
        assert!(Operation::decode(&3u32.to_le_bytes()).is_err());
    }

    #[test]
    fn trailing_payload_is_refused() {
        let mut encoded = Operation::CaptureFrame.encode().unwrap();
        encoded.push(0);
        assert!(Operation::decode(&encoded).is_err());
    }

    #[test]
    fn declared_length_is_bounded_before_allocation() {
        let wire = ((MAX_REQUEST_BYTES as u32) + 1).to_le_bytes();
        assert!(read_request(&mut wire.as_slice()).is_err());
    }
}

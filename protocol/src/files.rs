//! File transfer on a share (docs/COMMAND-STREAM.md, "Files"): the typed
//! requests and replies a [`crate::command::ChannelKind::Files`] channel
//! carries, one message per `Data`.
//!
//! A host shares named folders ([`ShareInfo`]); the host application, never
//! the client, decides which and whether each is writable. A client names a
//! share and a path inside it, written with `/`, relative to the share's
//! root. It never gives an absolute path, and the host refuses a path that
//! would leave the share.
//!
//! A download is one request answered by a run of [`FileReply::Chunk`]s and
//! an [`FileReply::End`]. An upload is a [`FileRequest::Write`] the host
//! answers with [`FileReply::Ready`] (where to continue from), the client's
//! [`FileRequest::Chunk`]s, and a [`FileRequest::Finish`]; the host writes
//! to a temporary name and renames it into place only when the whole file
//! has arrived.

use serde::{Deserialize, Serialize};

use crate::command::MAX_CHUNK;

/// The most file bytes one chunk carries, leaving room for the message's
/// own framing inside one `Data`.
pub const FILE_CHUNK: usize = MAX_CHUNK - 256;

/// An upload is acknowledged each time this many more bytes have been
/// written, and a client keeps no more than [`UPLOAD_WINDOW`] bytes
/// unacknowledged, so a slow link does not make it queue the whole file.
pub const ACK_EVERY: u64 = 256 * 1024;
pub const UPLOAD_WINDOW: u64 = 1024 * 1024;

/// The longest path a request may name, in bytes.
pub const MAX_PATH: usize = 1024;

/// The most entries one listing returns.
pub const MAX_ENTRIES: usize = 5000;

/// The ending of a file being uploaded; a file with this ending is never
/// listed, read or written by name.
pub const PART_SUFFIX: &str = ".wcpart";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInfo {
    pub name: String,
    pub writable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    /// Seconds since the Unix epoch, when the system gives it.
    pub modified: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileRequest {
    /// The shares this host offers this client.
    Shares,
    List {
        share: String,
        path: String,
    },
    Stat {
        share: String,
        path: String,
    },
    /// Download from `offset`: chunks, then `End`.
    Read {
        share: String,
        path: String,
        offset: u64,
    },
    /// Upload a file of `size` bytes. With `resume` the host continues a
    /// partial upload of the same name and says where in `Ready`;
    /// otherwise it starts over.
    Write {
        share: String,
        path: String,
        size: u64,
        resume: bool,
    },
    /// File bytes of the upload, in order from where `Ready` said.
    Chunk {
        bytes: Vec<u8>,
    },
    /// The upload is complete.
    Finish,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileReply {
    Shares(Vec<ShareInfo>),
    Entries(Vec<Entry>),
    Info(Entry),
    Chunk {
        bytes: Vec<u8>,
    },
    /// The download is complete; the file is `size` bytes.
    End {
        size: u64,
    },
    /// The upload may go on from `offset`.
    Ready {
        offset: u64,
    },
    /// `written` bytes of the upload are on the host's disk.
    Ack {
        written: u64,
    },
    /// The upload is complete and in place.
    Done {
        size: u64,
    },
    /// The request was not carried out. The reason is for the user.
    Error(String),
}

pub fn encode_request(request: &FileRequest) -> Vec<u8> {
    bincode::serialize(request).expect("a request serializes")
}

pub fn decode_request(bytes: &[u8]) -> Result<FileRequest, String> {
    bincode::deserialize(bytes).map_err(|_| "that is not a file request".to_owned())
}

pub fn encode_reply(reply: &FileReply) -> Vec<u8> {
    bincode::serialize(reply).expect("a reply serializes")
}

pub fn decode_reply(bytes: &[u8]) -> Result<FileReply, String> {
    bincode::deserialize(bytes).map_err(|_| "that is not a file reply".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_replies_round_trip() {
        for request in [
            FileRequest::Shares,
            FileRequest::Read {
                share: "saves".into(),
                path: "a/b.sav".into(),
                offset: 7,
            },
            FileRequest::Write {
                share: "saves".into(),
                path: "a.sav".into(),
                size: 9,
                resume: true,
            },
            FileRequest::Chunk { bytes: vec![1, 2] },
            FileRequest::Finish,
        ] {
            assert_eq!(decode_request(&encode_request(&request)).unwrap(), request);
        }
        let reply = FileReply::Info(Entry {
            name: "x".into(),
            dir: false,
            size: 3,
            modified: Some(5),
        });
        assert_eq!(decode_reply(&encode_reply(&reply)).unwrap(), reply);
        assert!(decode_request(&[0xff; 4]).is_err());
    }

    #[test]
    fn a_full_chunk_fits_in_one_data_message() {
        let reply = FileReply::Chunk {
            bytes: vec![0; FILE_CHUNK],
        };
        assert!(encode_reply(&reply).len() <= MAX_CHUNK);
    }
}

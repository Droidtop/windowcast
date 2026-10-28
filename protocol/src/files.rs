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

/// Names Windows treats as devices, with or without an extension.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// The parts of a path a client named, or why it is refused. The path is
/// written with `/` and is relative to the share's folder; the empty path
/// is the folder itself.
pub fn clean(path: &str) -> Result<Vec<&str>, String> {
    if path.len() > MAX_PATH {
        return Err("that path is too long".into());
    }
    if path.starts_with('/') {
        return Err("give a path inside the share, not an absolute one".into());
    }
    let mut parts = Vec::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        if part == "." || part == ".." {
            return Err("a path may not contain . or ..".into());
        }
        if part
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        {
            return Err(format!("{part:?} has a character a path may not have"));
        }
        if part.ends_with('.') || part.ends_with(' ') {
            return Err(format!("{part:?} ends with a dot or a space"));
        }
        let stem = part.split('.').next().unwrap_or(part).to_uppercase();
        if RESERVED.contains(&stem.as_str()) {
            return Err(format!("{part:?} is a reserved name"));
        }
        if part.to_lowercase().ends_with(PART_SUFFIX) {
            return Err(format!("a name may not end with {PART_SUFFIX}"));
        }
        parts.push(part);
    }
    Ok(parts)
}

/// What a folder watch (`ChannelKind::Watch`) sends the client: for each file that has changed
/// and stopped changing, a `File`, its `Chunk`s and an `End`. The client files it under the folder
/// it chose, at the relative `path`; the host is not trusted with that path, so the client
/// checks it with [`clean`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WatchReply {
    /// The watch is on; changes from now on are sent. `existing` is how many files were already
    /// there (sent first when the client asked for them).
    Started {
        existing: u32,
    },
    File {
        path: String,
        size: u64,
    },
    Chunk {
        bytes: Vec<u8>,
    },
    /// The file just announced is complete.
    End,
    /// The watch cannot go on (the folder went away, say). The channel then closes.
    Error(String),
}

pub fn encode_watch(reply: &WatchReply) -> Vec<u8> {
    bincode::serialize(reply).expect("a reply serializes")
}

pub fn decode_watch(bytes: &[u8]) -> Result<WatchReply, String> {
    bincode::deserialize(bytes).map_err(|_| "that is not a watch reply".to_owned())
}

/// Whether `name` matches `pattern`, where `*` stands for any run of characters and `?` for one.
/// The match is on a file's name, without case.
pub fn glob_matches(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some(('*', rest)) => (0..=n.len()).any(|skip| go(rest, &n[skip..])),
            Some(('?', rest)) => !n.is_empty() && go(rest, &n[1..]),
            Some((c, rest)) => n.first() == Some(c) && go(rest, &n[1..]),
        }
    }
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    go(&p, &n)
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
    fn globs_match_names_without_case() {
        assert!(glob_matches("*.sav", "Slot1.SAV"));
        assert!(glob_matches("save?.dat", "save3.dat"));
        assert!(glob_matches("*", ""));
        assert!(glob_matches("a*b*c", "aXXbYYc"));
        assert!(!glob_matches("*.sav", "slot1.sav.bak"));
        assert!(!glob_matches("save?.dat", "save.dat"));
    }

    #[test]
    fn watch_replies_round_trip() {
        for reply in [
            WatchReply::Started { existing: 3 },
            WatchReply::File {
                path: "a/b.sav".into(),
                size: 4,
            },
            WatchReply::Chunk { bytes: vec![1] },
            WatchReply::End,
            WatchReply::Error("gone".into()),
        ] {
            assert_eq!(decode_watch(&encode_watch(&reply)).unwrap(), reply);
        }
    }

    #[test]
    fn a_full_chunk_fits_in_one_data_message() {
        let reply = FileReply::Chunk {
            bytes: vec![0; FILE_CHUNK],
        };
        assert!(encode_reply(&reply).len() <= MAX_CHUNK);
    }
}

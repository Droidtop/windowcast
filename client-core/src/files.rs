//! File transfer from the client (docs/COMMAND-STREAM.md, "Files"): the
//! host's shared folders, listed, downloaded and uploaded over a files
//! channel. The host decides what is shared; this only names a share and a
//! path inside it.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windowcast_protocol::command::ChannelKind;
use windowcast_protocol::files::{
    decode_reply, encode_request, Entry, FileReply, FileRequest, ShareInfo, FILE_CHUNK,
    PART_SUFFIX, UPLOAD_WINDOW,
};
use windowcast_terminal::{Channel, ChannelEvent};

use crate::{ClientError, ClientSession};

/// How long the host has to answer a request, or to send the next chunk.
const REPLY_WITHIN: Duration = Duration::from_secs(30);

/// An open files channel on a host.
pub struct FilesClient {
    channel: Channel,
}

impl ClientSession {
    /// Opens file transfer on the host. The host refuses when it shares no
    /// folders or does not allow this client; the reason is in the error.
    pub fn open_files(&self) -> Result<FilesClient, ClientError> {
        Ok(FilesClient {
            channel: self.open_channel(ChannelKind::Files)?,
        })
    }
}

impl FilesClient {
    fn send(&self, request: &FileRequest) {
        self.channel.write(&encode_request(request));
    }

    /// The host's next reply, waiting up to `within`.
    fn reply_within(&self, within: Duration) -> Result<Option<FileReply>, ClientError> {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self
                .channel
                .next_event(left.min(Duration::from_millis(250)))
            {
                Some(ChannelEvent::Data(bytes)) => {
                    return decode_reply(&bytes).map(Some).map_err(ClientError::Refused)
                }
                Some(ChannelEvent::Closed) => {
                    return Err(ClientError::Refused("the file channel closed".into()))
                }
                Some(ChannelEvent::Exited(_)) | None => {}
            }
            if left.is_zero() {
                return Ok(None);
            }
        }
    }

    fn reply(&self) -> Result<FileReply, ClientError> {
        match self.reply_within(REPLY_WITHIN)? {
            Some(FileReply::Error(reason)) => Err(ClientError::Refused(reason)),
            Some(reply) => Ok(reply),
            None => Err(ClientError::Refused("the host did not answer".into())),
        }
    }

    fn unexpected<T>(reply: FileReply) -> Result<T, ClientError> {
        Err(ClientError::Refused(format!(
            "the host answered unexpectedly: {reply:?}"
        )))
    }

    /// The shares this host offers.
    pub fn shares(&self) -> Result<Vec<ShareInfo>, ClientError> {
        self.send(&FileRequest::Shares);
        match self.reply()? {
            FileReply::Shares(shares) => Ok(shares),
            other => Self::unexpected(other),
        }
    }

    /// The entries of a folder of a share (`path` is `""` for the share's
    /// top).
    pub fn list(&self, share: &str, path: &str) -> Result<Vec<Entry>, ClientError> {
        self.send(&FileRequest::List {
            share: share.into(),
            path: path.into(),
        });
        match self.reply()? {
            FileReply::Entries(entries) => Ok(entries),
            other => Self::unexpected(other),
        }
    }

    pub fn stat(&self, share: &str, path: &str) -> Result<Entry, ClientError> {
        self.send(&FileRequest::Stat {
            share: share.into(),
            path: path.into(),
        });
        match self.reply()? {
            FileReply::Info(entry) => Ok(entry),
            other => Self::unexpected(other),
        }
    }

    /// Downloads a file to `dest`. It is written beside it under a
    /// temporary name and renamed when complete; with `resume` a partial
    /// download of the same name continues where it stopped. Returns the
    /// file's size.
    pub fn download(
        &self,
        share: &str,
        path: &str,
        dest: &Path,
        resume: bool,
    ) -> Result<u64, ClientError> {
        let part = part_of(dest);
        let held = if resume {
            std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };
        self.send(&FileRequest::Read {
            share: share.into(),
            path: path.into(),
            offset: held,
        });
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(held == 0)
            .open(&part)?;
        file.seek(SeekFrom::Start(held))?;
        let mut have = held;
        loop {
            match self.reply()? {
                FileReply::Chunk { bytes } => {
                    file.write_all(&bytes)?;
                    have += bytes.len() as u64;
                }
                FileReply::End { size } => {
                    if have != size {
                        return Err(ClientError::Refused(format!(
                            "the download is incomplete: {have} of {size} bytes"
                        )));
                    }
                    file.sync_all()?;
                    drop(file);
                    if dest.exists() {
                        std::fs::remove_file(dest)?;
                    }
                    std::fs::rename(&part, dest)?;
                    return Ok(size);
                }
                other => return Self::unexpected(other),
            }
        }
    }

    /// Uploads the file at `source` to `path` of a writable share. With
    /// `resume` an interrupted upload of the same name continues where the
    /// host has it. The host puts the file in place only when all of it has
    /// arrived. Returns the size.
    pub fn upload(
        &self,
        share: &str,
        path: &str,
        source: &Path,
        resume: bool,
    ) -> Result<u64, ClientError> {
        let mut file = File::open(source)?;
        let size = file.metadata()?.len();
        self.send(&FileRequest::Write {
            share: share.into(),
            path: path.into(),
            size,
            resume,
        });
        let offset = match self.reply()? {
            FileReply::Ready { offset } => offset,
            other => return Self::unexpected(other),
        };
        file.seek(SeekFrom::Start(offset))?;
        let (mut sent, mut acked) = (offset, offset);
        let mut buf = vec![0u8; FILE_CHUNK];
        while sent < size {
            // Never more than the window in flight; look for the host's
            // acknowledgements, and for an error, without waiting.
            loop {
                let wait = if sent - acked >= UPLOAD_WINDOW {
                    REPLY_WITHIN
                } else {
                    Duration::ZERO
                };
                match self.reply_within(wait)? {
                    Some(FileReply::Ack { written }) => acked = acked.max(written),
                    Some(FileReply::Error(reason)) => return Err(ClientError::Refused(reason)),
                    Some(other) => return Self::unexpected(other),
                    None if wait.is_zero() => break,
                    None => return Err(ClientError::Refused("the host did not answer".into())),
                }
                if sent - acked < UPLOAD_WINDOW {
                    break;
                }
            }
            let want = FILE_CHUNK.min((size - sent) as usize);
            file.read_exact(&mut buf[..want])?;
            self.send(&FileRequest::Chunk {
                bytes: buf[..want].to_vec(),
            });
            sent += want as u64;
        }
        self.send(&FileRequest::Finish);
        loop {
            match self.reply()? {
                FileReply::Ack { .. } => {}
                FileReply::Done { size } => return Ok(size),
                other => return Self::unexpected(other),
            }
        }
    }

    /// Ends the channel.
    pub fn close(&self) {
        self.channel.close();
    }
}

fn part_of(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_owned();
    name.push(PART_SUFFIX);
    PathBuf::from(name)
}

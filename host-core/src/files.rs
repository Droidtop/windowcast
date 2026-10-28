//! The host's end of file transfer (docs/COMMAND-STREAM.md, "Files"): the
//! folders a host application shares, and the service that answers a
//! client's [`FileRequest`]s on them.
//!
//! The scope is the host application's: it builds a [`Shares`] and gives it
//! to the host ([`crate::HostControl::set_file_shares`]). A client can only
//! name a share and a path inside it. Every path is checked twice: its
//! spelling ([`clean`]: no `..`, no absolute or drive path, no stream or
//! device names, no control characters), then where it really lands, with
//! symbolic links followed, which must still be inside the share's folder.
//! Nothing a client sends changes either check.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::UNIX_EPOCH;

use windowcast_protocol::files::{
    decode_request, encode_reply, ACK_EVERY, Entry, FileReply, FileRequest, ShareInfo, FILE_CHUNK,
    MAX_ENTRIES, MAX_PATH, PART_SUFFIX,
};

use crate::command::RunningProcess;

/// A folder shared under a name.
#[derive(Debug, Clone)]
pub struct Share {
    pub name: String,
    pub root: PathBuf,
    pub writable: bool,
}

/// The folders a host shares. Empty by default: a host shares nothing until
/// its application says so.
#[derive(Debug, Clone)]
pub struct Shares {
    shares: Vec<Share>,
    /// The largest file an upload may be, in bytes.
    pub max_file_size: u64,
}

impl Default for Shares {
    fn default() -> Self {
        Shares {
            shares: Vec::new(),
            max_file_size: 4 << 30,
        }
    }
}

impl Shares {
    /// Shares `root` as `name`. A name is a plain word: letters, digits,
    /// `-`, `_`, `.` and spaces, not empty, not repeated.
    pub fn add(
        &mut self,
        name: &str,
        root: impl Into<PathBuf>,
        writable: bool,
    ) -> Result<(), String> {
        let plain = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ');
        if name.trim_matches('.').is_empty() || name.len() > 64 || !name.chars().all(plain) {
            return Err(format!("{name:?} is not a share name"));
        }
        if self.shares.iter().any(|s| s.name == name) {
            return Err(format!("there is already a share named {name:?}"));
        }
        self.shares.push(Share {
            name: name.to_owned(),
            root: root.into(),
            writable,
        });
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.shares.is_empty()
    }

    pub fn info(&self) -> Vec<ShareInfo> {
        self.shares
            .iter()
            .map(|s| ShareInfo {
                name: s.name.clone(),
                writable: s.writable,
            })
            .collect()
    }

    fn share(&self, name: &str) -> Result<&Share, String> {
        self.shares
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| format!("there is no share named {name:?}"))
    }

    /// Where `path` of share `name` is, which must exist and be inside the
    /// share. Returns the share's canonical root too.
    pub fn resolve_existing(&self, name: &str, path: &str) -> Result<(PathBuf, PathBuf), String> {
        let share = self.share(name)?;
        let parts = clean(path)?;
        let root = canonical_root(share)?;
        let found = parts
            .iter()
            .fold(root.clone(), |dir, part| dir.join(part))
            .canonicalize()
            .map_err(|_| "there is no such file or folder".to_owned())?;
        if !found.starts_with(&root) {
            return Err("that path is outside the share".into());
        }
        Ok((found, root))
    }

    /// Where a file that may not exist yet would go: its folder must exist
    /// inside the share, and the name must not be a symbolic link.
    pub fn resolve_new(&self, name: &str, path: &str) -> Result<PathBuf, String> {
        let share = self.share(name)?;
        let parts = clean(path)?;
        let Some((last, folders)) = parts.split_last() else {
            return Err("name the file to write".into());
        };
        let root = canonical_root(share)?;
        let folder = folders
            .iter()
            .fold(root.clone(), |dir, part| dir.join(part))
            .canonicalize()
            .map_err(|_| "the folder does not exist".to_owned())?;
        if !folder.starts_with(&root) {
            return Err("that path is outside the share".into());
        }
        let target = folder.join(last);
        if let Ok(meta) = std::fs::symlink_metadata(&target) {
            if meta.file_type().is_symlink() {
                return Err("that name is a link".into());
            }
        }
        Ok(target)
    }

    pub fn writable(&self, name: &str) -> Result<bool, String> {
        Ok(self.share(name)?.writable)
    }
}

fn canonical_root(share: &Share) -> Result<PathBuf, String> {
    share
        .root
        .canonicalize()
        .map_err(|_| format!("the folder of share {:?} is not available", share.name))
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

fn entry_of(name: String, meta: &std::fs::Metadata) -> Entry {
    Entry {
        name,
        dir: meta.is_dir(),
        size: if meta.is_dir() { 0 } else { meta.len() },
        modified: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs()),
    }
}

fn part_path(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_owned();
    name.push(PART_SUFFIX);
    PathBuf::from(name)
}

struct Upload {
    file: File,
    part: PathBuf,
    target: PathBuf,
    size: u64,
    written: u64,
    acked: u64,
}

/// Answers one channel's requests.
pub struct FileService {
    shares: Arc<Shares>,
    upload: Option<Upload>,
}

impl FileService {
    pub fn new(shares: Arc<Shares>) -> Self {
        FileService {
            shares,
            upload: None,
        }
    }

    /// Carries out `request`, giving each reply to `emit`; `emit` returns
    /// false when the channel is gone, and a download stops there.
    pub fn handle(
        &mut self,
        request: FileRequest,
        cancelled: &AtomicBool,
        emit: &mut dyn FnMut(FileReply) -> bool,
    ) {
        let outcome = match request {
            FileRequest::Shares => {
                emit(FileReply::Shares(self.shares.info()));
                Ok(())
            }
            FileRequest::List { share, path } => self.list(&share, &path).map(|e| {
                emit(FileReply::Entries(e));
            }),
            FileRequest::Stat { share, path } => self.stat(&share, &path).map(|e| {
                emit(FileReply::Info(e));
            }),
            FileRequest::Read {
                share,
                path,
                offset,
            } => self.read(&share, &path, offset, cancelled, emit),
            FileRequest::Write {
                share,
                path,
                size,
                resume,
            } => self
                .begin_upload(&share, &path, size, resume)
                .map(|offset| {
                    emit(FileReply::Ready { offset });
                }),
            FileRequest::Chunk { bytes } => self.chunk(&bytes).map(|acked| {
                if let Some(written) = acked {
                    emit(FileReply::Ack { written });
                }
            }),
            FileRequest::Finish => self.finish().map(|size| {
                emit(FileReply::Done { size });
            }),
        };
        if let Err(reason) = outcome {
            emit(FileReply::Error(reason));
        }
    }

    fn list(&self, share: &str, path: &str) -> Result<Vec<Entry>, String> {
        let (dir, root) = self.shares.resolve_existing(share, path)?;
        if !dir.is_dir() {
            return Err("that is not a folder".into());
        }
        let mut entries = Vec::new();
        for item in std::fs::read_dir(&dir)
            .map_err(|e| e.to_string())?
            .flatten()
        {
            let Some(name) = item.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.to_lowercase().ends_with(PART_SUFFIX) {
                continue;
            }
            // Through any link, and only what is still inside the share.
            let Ok(real) = item.path().canonicalize() else {
                continue;
            };
            if !real.starts_with(&root) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&real) else {
                continue;
            };
            if meta.is_file() || meta.is_dir() {
                entries.push(entry_of(name, &meta));
            }
            if entries.len() >= MAX_ENTRIES {
                break;
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn stat(&self, share: &str, path: &str) -> Result<Entry, String> {
        let (found, _) = self.shares.resolve_existing(share, path)?;
        let meta = std::fs::metadata(&found).map_err(|e| e.to_string())?;
        let name = clean(path)?.last().map_or(share, |l| *l).to_owned();
        Ok(entry_of(name, &meta))
    }

    fn read(
        &self,
        share: &str,
        path: &str,
        offset: u64,
        cancelled: &AtomicBool,
        emit: &mut dyn FnMut(FileReply) -> bool,
    ) -> Result<(), String> {
        let (found, _) = self.shares.resolve_existing(share, path)?;
        let mut file = File::open(&found).map_err(|e| e.to_string())?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file() {
            return Err("that is not a file".into());
        }
        let size = meta.len();
        if offset > size {
            return Err("the offset is past the end of the file".into());
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; FILE_CHUNK];
        loop {
            if cancelled.load(Ordering::SeqCst) {
                return Ok(());
            }
            let n = file.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            if !emit(FileReply::Chunk {
                bytes: buf[..n].to_vec(),
            }) {
                return Ok(());
            }
        }
        emit(FileReply::End { size });
        Ok(())
    }

    fn begin_upload(
        &mut self,
        share: &str,
        path: &str,
        size: u64,
        resume: bool,
    ) -> Result<u64, String> {
        self.upload = None;
        if !self.shares.writable(share)? {
            return Err("that share is read-only".into());
        }
        if size > self.shares.max_file_size {
            return Err(format!(
                "the file is larger than the {} bytes this host accepts",
                self.shares.max_file_size
            ));
        }
        let target = self.shares.resolve_new(share, path)?;
        if target.is_dir() {
            return Err("that name is a folder".into());
        }
        let part = part_path(&target);
        let held = if resume {
            std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };
        let held = if held > size { 0 } else { held };
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(held == 0)
            .open(&part)
            .map_err(|e| e.to_string())?;
        file.seek(SeekFrom::Start(held))
            .map_err(|e| e.to_string())?;
        self.upload = Some(Upload {
            file,
            part,
            target,
            size,
            written: held,
            acked: held,
        });
        Ok(held)
    }

    /// Writes upload bytes; the byte count to acknowledge, when enough have
    /// arrived since the last acknowledgement.
    fn chunk(&mut self, bytes: &[u8]) -> Result<Option<u64>, String> {
        let Some(upload) = self.upload.as_mut() else {
            return Err("no upload is open".into());
        };
        if upload.written + bytes.len() as u64 > upload.size {
            self.upload = None;
            return Err("more bytes arrived than the file was announced to have".into());
        }
        if let Err(e) = upload.file.write_all(bytes) {
            self.upload = None;
            return Err(e.to_string());
        }
        upload.written += bytes.len() as u64;
        if upload.written - upload.acked >= ACK_EVERY {
            upload.acked = upload.written;
            return Ok(Some(upload.written));
        }
        Ok(None)
    }

    fn finish(&mut self) -> Result<u64, String> {
        let Some(upload) = self.upload.as_mut() else {
            return Err("no upload is open".into());
        };
        if upload.written != upload.size {
            return Err(format!(
                "the file is incomplete: {} of {} bytes arrived",
                upload.written, upload.size
            ));
        }
        upload.file.sync_all().map_err(|e| e.to_string())?;
        let upload = self.upload.take().expect("checked above");
        drop(upload.file);
        // Windows will not rename over a file.
        if upload.target.is_file() {
            std::fs::remove_file(&upload.target).map_err(|e| e.to_string())?;
        }
        std::fs::rename(&upload.part, &upload.target).map_err(|e| e.to_string())?;
        Ok(upload.size)
    }
}

/// A files channel as the channel table sees it: bytes written to it are
/// requests, handled in order off the session's task; replies go out
/// through `send`.
pub(crate) struct FilesProcess {
    service: Mutex<FileService>,
    cancelled: AtomicBool,
    send: Box<dyn Fn(Vec<u8>) -> bool + Send + Sync>,
    ended: Mutex<bool>,
    ended_changed: Condvar,
}

impl FilesProcess {
    pub(crate) fn new(
        shares: Arc<Shares>,
        send: Box<dyn Fn(Vec<u8>) -> bool + Send + Sync>,
    ) -> Self {
        FilesProcess {
            service: Mutex::new(FileService::new(shares)),
            cancelled: AtomicBool::new(false),
            send,
            ended: Mutex::new(false),
            ended_changed: Condvar::new(),
        }
    }
}

impl RunningProcess for FilesProcess {
    fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut emit = |reply: FileReply| (self.send)(encode_reply(&reply));
        match decode_request(bytes) {
            Ok(request) => {
                self.service
                    .lock()
                    .expect("files")
                    .handle(request, &self.cancelled, &mut emit)
            }
            Err(reason) => {
                emit(FileReply::Error(reason));
            }
        }
        Ok(())
    }

    fn eof(&self) {}

    fn kill(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        *self.ended.lock().expect("files") = true;
        self.ended_changed.notify_all();
    }

    fn wait(&self) -> Option<i32> {
        let mut ended = self.ended.lock().expect("files");
        while !*ended {
            ended = self.ended_changed.wait(ended).expect("files");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("windowcast-files-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn service(root: &Path, writable: bool) -> FileService {
        let mut shares = Shares::default();
        shares.add("saves", root, writable).unwrap();
        FileService::new(Arc::new(shares))
    }

    fn ask(service: &mut FileService, request: FileRequest) -> Vec<FileReply> {
        let mut replies = Vec::new();
        service.handle(request, &AtomicBool::new(false), &mut |r| {
            replies.push(r);
            true
        });
        replies
    }

    fn error_of(replies: &[FileReply]) -> &str {
        match replies {
            [FileReply::Error(reason)] => reason,
            other => panic!("expected one error, got {other:?}"),
        }
    }

    fn list(path: &str) -> FileRequest {
        FileRequest::List {
            share: "saves".into(),
            path: path.into(),
        }
    }

    #[test]
    fn paths_that_could_leave_the_share_are_refused_by_spelling() {
        for bad in [
            "../x",
            "a/../../x",
            "a/./b",
            "/etc/passwd",
            "C:\\Windows",
            "a\\b",
            "x:stream",
            "CON",
            "nul.txt",
            "com1",
            "trailing.",
            "trailing ",
            "a\0b",
            "new\nline",
            "x.wcpart",
            "x.WCPART",
        ] {
            assert!(clean(bad).is_err(), "{bad:?} should be refused");
        }
        assert_eq!(clean("").unwrap(), Vec::<&str>::new());
        assert_eq!(clean("a/b.sav").unwrap(), ["a", "b.sav"]);
        assert_eq!(clean("a//b/").unwrap(), ["a", "b"]);
    }

    #[test]
    fn a_share_lists_downloads_and_stats() {
        let root = temp("read");
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("a.sav"), b"hello").unwrap();
        let mut service = service(&root, false);

        match &ask(&mut service, list(""))[..] {
            [FileReply::Entries(entries)] => {
                let names: Vec<_> = entries
                    .iter()
                    .map(|e| (e.name.as_str(), e.dir, e.size))
                    .collect();
                assert_eq!(names, [("a.sav", false, 5), ("sub", true, 0)]);
            }
            other => panic!("{other:?}"),
        }
        let replies = ask(
            &mut service,
            FileRequest::Read {
                share: "saves".into(),
                path: "a.sav".into(),
                offset: 2,
            },
        );
        assert_eq!(
            replies,
            [
                FileReply::Chunk {
                    bytes: b"llo".to_vec()
                },
                FileReply::End { size: 5 }
            ]
        );
        match &ask(
            &mut service,
            FileRequest::Stat {
                share: "saves".into(),
                path: "a.sav".into(),
            },
        )[..]
        {
            [FileReply::Info(e)] => assert_eq!((e.name.as_str(), e.size), ("a.sav", 5)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_large_file_comes_in_chunks_and_an_offset_past_the_end_is_refused() {
        let root = temp("chunks");
        let data: Vec<u8> = (0..FILE_CHUNK * 2 + 10).map(|i| i as u8).collect();
        std::fs::write(root.join("big.bin"), &data).unwrap();
        let mut service = service(&root, false);
        let replies = ask(
            &mut service,
            FileRequest::Read {
                share: "saves".into(),
                path: "big.bin".into(),
                offset: 0,
            },
        );
        let mut got: Vec<u8> = Vec::new();
        for reply in &replies[..replies.len() - 1] {
            match reply {
                FileReply::Chunk { bytes } => got.extend(bytes),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(got, data);
        assert_eq!(replies.len(), 4);
        let replies = ask(
            &mut service,
            FileRequest::Read {
                share: "saves".into(),
                path: "big.bin".into(),
                offset: data.len() as u64 + 1,
            },
        );
        assert!(error_of(&replies).contains("past the end"));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_share_is_not_followed() {
        let root = temp("link");
        let outside = temp("link-outside");
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        std::fs::write(root.join("fine.txt"), b"fine").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("file-link")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("dir-link")).unwrap();
        // A link that stays inside is fine.
        std::os::unix::fs::symlink(root.join("fine.txt"), root.join("inside-link")).unwrap();
        let mut service = service(&root, true);

        for path in ["file-link", "dir-link/secret.txt", "dir-link"] {
            let replies = ask(
                &mut service,
                FileRequest::Read {
                    share: "saves".into(),
                    path: path.into(),
                    offset: 0,
                },
            );
            assert!(
                error_of(&replies).contains("outside"),
                "{path}: {replies:?}"
            );
        }
        match &ask(&mut service, list(""))[..] {
            [FileReply::Entries(entries)] => {
                let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
                assert_eq!(names, ["fine.txt", "inside-link"]);
            }
            other => panic!("{other:?}"),
        }
        let replies = ask(
            &mut service,
            FileRequest::Read {
                share: "saves".into(),
                path: "inside-link".into(),
                offset: 0,
            },
        );
        assert_eq!(
            replies[0],
            FileReply::Chunk {
                bytes: b"fine".to_vec()
            }
        );
        // Writing through a link out, by name or by folder, is refused too.
        for path in ["file-link", "dir-link/new.txt"] {
            let replies = ask(
                &mut service,
                FileRequest::Write {
                    share: "saves".into(),
                    path: path.into(),
                    size: 1,
                    resume: false,
                },
            );
            assert!(
                !matches!(replies[..], [FileReply::Ready { .. }]),
                "{path}: {replies:?}"
            );
        }
        assert_eq!(
            std::fs::read(outside.join("secret.txt")).unwrap(),
            b"secret"
        );
        assert!(!outside.join("new.txt").exists());
    }

    #[test]
    fn an_unknown_share_and_a_missing_file_are_refused() {
        let root = temp("unknown");
        let mut service = service(&root, false);
        let replies = ask(
            &mut service,
            FileRequest::List {
                share: "nope".into(),
                path: "".into(),
            },
        );
        assert!(error_of(&replies).contains("no share"));
        let replies = ask(
            &mut service,
            FileRequest::Read {
                share: "saves".into(),
                path: "missing".into(),
                offset: 0,
            },
        );
        assert!(error_of(&replies).contains("no such file"));
    }

    fn upload(service: &mut FileService, path: &str, data: &[u8], resume: bool) -> Vec<FileReply> {
        let mut replies = ask(
            service,
            FileRequest::Write {
                share: "saves".into(),
                path: path.into(),
                size: data.len() as u64,
                resume,
            },
        );
        let offset = match replies[..] {
            [FileReply::Ready { offset }] => offset as usize,
            _ => return replies,
        };
        for chunk in data[offset..].chunks(FILE_CHUNK) {
            let more = ask(
                service,
                FileRequest::Chunk {
                    bytes: chunk.to_vec(),
                },
            );
            assert!(more.is_empty(), "{more:?}");
        }
        replies.extend(ask(service, FileRequest::Finish));
        replies
    }

    #[test]
    fn an_upload_is_written_under_a_temporary_name_and_renamed_when_complete() {
        let root = temp("write");
        std::fs::create_dir(root.join("sub")).unwrap();
        let mut service = service(&root, true);
        let data: Vec<u8> = (0..FILE_CHUNK + 5).map(|i| (i % 251) as u8).collect();

        let replies = ask(
            &mut service,
            FileRequest::Write {
                share: "saves".into(),
                path: "sub/a.sav".into(),
                size: data.len() as u64,
                resume: false,
            },
        );
        assert_eq!(replies, [FileReply::Ready { offset: 0 }]);
        ask(
            &mut service,
            FileRequest::Chunk {
                bytes: data[..10].to_vec(),
            },
        );
        assert!(root.join("sub/a.sav.wcpart").exists());
        assert!(
            !root.join("sub/a.sav").exists(),
            "not in place before it is complete"
        );
        // Not finished: Finish says so and nothing appears.
        assert!(error_of(&ask(&mut service, FileRequest::Finish)).contains("incomplete"));
        assert!(!root.join("sub/a.sav").exists());
        // And a partial file is not listed.
        match &ask(&mut service, list("sub"))[..] {
            [FileReply::Entries(entries)] => assert!(entries.is_empty()),
            other => panic!("{other:?}"),
        }
        for chunk in data[10..].chunks(FILE_CHUNK) {
            ask(
                &mut service,
                FileRequest::Chunk {
                    bytes: chunk.to_vec(),
                },
            );
        }
        assert_eq!(
            ask(&mut service, FileRequest::Finish),
            [FileReply::Done {
                size: data.len() as u64
            }]
        );
        assert_eq!(std::fs::read(root.join("sub/a.sav")).unwrap(), data);
        assert!(!root.join("sub/a.sav.wcpart").exists());
    }

    #[test]
    fn an_interrupted_upload_resumes_where_it_stopped() {
        let root = temp("resume");
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 253) as u8).collect();
        {
            let mut first = service(&root, true);
            ask(
                &mut first,
                FileRequest::Write {
                    share: "saves".into(),
                    path: "r.bin".into(),
                    size: 5000,
                    resume: false,
                },
            );
            ask(
                &mut first,
                FileRequest::Chunk {
                    bytes: data[..1234].to_vec(),
                },
            );
        } // the channel is gone
        let mut second = service(&root, true);
        let replies = upload(&mut second, "r.bin", &data, true);
        assert_eq!(replies[0], FileReply::Ready { offset: 1234 });
        assert_eq!(replies.last(), Some(&FileReply::Done { size: 5000 }));
        assert_eq!(std::fs::read(root.join("r.bin")).unwrap(), data);
        // Without `resume` it starts over.
        let mut third = service(&root, true);
        let replies = ask(
            &mut third,
            FileRequest::Write {
                share: "saves".into(),
                path: "r2.bin".into(),
                size: 10,
                resume: true,
            },
        );
        assert_eq!(replies, [FileReply::Ready { offset: 0 }]);
    }

    #[test]
    fn a_read_only_share_and_oversized_or_overlong_uploads_are_refused() {
        let root = temp("limits");
        let mut read_only = service(&root, false);
        let replies = upload(&mut read_only, "x.bin", b"abc", false);
        assert!(error_of(&replies).contains("read-only"));

        let mut shares = Shares::default();
        shares.add("saves", &root, true).unwrap();
        shares.max_file_size = 4;
        let mut small = FileService::new(Arc::new(shares));
        assert!(error_of(&upload(&mut small, "big.bin", b"12345", false)).contains("larger"));

        let mut service = service(&root, true);
        ask(
            &mut service,
            FileRequest::Write {
                share: "saves".into(),
                path: "short.bin".into(),
                size: 2,
                resume: false,
            },
        );
        let replies = ask(
            &mut service,
            FileRequest::Chunk {
                bytes: vec![1, 2, 3],
            },
        );
        assert!(error_of(&replies).contains("more bytes"));
        assert!(!root.join("short.bin").exists());
        // A chunk with no upload open.
        assert!(
            error_of(&ask(&mut service, FileRequest::Chunk { bytes: vec![1] }))
                .contains("no upload")
        );
        // Names and paths that escape or are reserved never reach the disk.
        for path in ["../escape.bin", "/abs.bin", "nul", "x.wcpart"] {
            let replies = upload(&mut service, path, b"x", false);
            assert!(
                matches!(replies[..], [FileReply::Error(_)]),
                "{path}: {replies:?}"
            );
        }
        assert!(!root.parent().unwrap().join("escape.bin").exists());
    }

    #[test]
    fn a_share_needs_a_plain_unique_name() {
        let mut shares = Shares::default();
        for bad in ["", "a/b", "..", "x\\y", "c:"] {
            assert!(shares.add(bad, "/tmp", false).is_err(), "{bad:?}");
        }
        shares.add("Game saves", "/tmp", true).unwrap();
        assert!(shares.add("Game saves", "/tmp", true).is_err());
    }
}

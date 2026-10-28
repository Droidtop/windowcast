//! Folder watch for save sync (docs/COMMAND-STREAM.md, "Files"): a client
//! asks the host to watch a folder of a share, and each file in it that has
//! changed and then stopped changing is sent over, once.
//!
//! "Stopped changing" is the guard against sending a half-written file: a
//! file is sent only when its size and modification time have been the same
//! for the whole debounce, and it can be opened for reading (a program
//! that still holds it exclusively on Windows keeps it back). Files are
//! found by looking at the folder every half second, not by operating
//! system notifications: the same code works on every host, and a save
//! folder is small.
//!
//! The scope is the same as for transfers: the share the host application
//! set, nothing outside it, links that lead out skipped.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use windowcast_protocol::files::{encode_watch, glob_matches, WatchReply, FILE_CHUNK, PART_SUFFIX};

use crate::command::RunningProcess;
use crate::files::Shares;

/// How long a file must be unchanged before it is sent.
pub const DEBOUNCE: Duration = Duration::from_secs(2);

/// How often the folder is looked at.
const POLL_EVERY: Duration = Duration::from_millis(500);

/// The most files one watch follows, and how deep into the folder it looks.
const MAX_FILES: usize = 5000;
const MAX_DEPTH: usize = 4;

type Stamp = (u64, Option<SystemTime>);

struct Seen {
    stamp: Stamp,
    /// When `stamp` was first seen.
    since: Instant,
    /// The stamp last sent, if any.
    sent: Option<Stamp>,
}

/// A file ready to send.
#[derive(Debug, PartialEq, Eq)]
pub struct Ready {
    /// Relative to the watched folder, written with `/`.
    pub path: String,
    pub file: PathBuf,
    pub size: u64,
}

/// What changed in a folder, from one look to the next.
pub struct Watcher {
    root: PathBuf,
    folder: PathBuf,
    pattern: Option<String>,
    debounce: Duration,
    seen: HashMap<String, Seen>,
}

impl Watcher {
    /// Starts watching `path` of share `share`. With `existing` the files already there count as
    /// changed and are sent; otherwise only what changes from now on. Returns how many files
    /// were already there.
    pub fn new(
        shares: &Shares,
        share: &str,
        path: &str,
        pattern: Option<String>,
        existing: bool,
        debounce: Duration,
        now: Instant,
    ) -> Result<(Watcher, u32), String> {
        let (folder, root) = shares.resolve_existing(share, path)?;
        if !folder.is_dir() {
            return Err("that is not a folder".into());
        }
        if let Some(pattern) = &pattern {
            if pattern.len() > 128 || pattern.contains('/') || pattern.contains('\\') {
                return Err("a pattern matches file names: * and ?, no folders".into());
            }
        }
        let mut watcher = Watcher {
            root,
            folder,
            pattern,
            debounce,
            seen: HashMap::new(),
        };
        let found = watcher.scan();
        let count = found.len() as u32;
        for (path, (_, stamp)) in found {
            watcher.seen.insert(
                path,
                Seen {
                    stamp,
                    since: now,
                    sent: if existing { None } else { Some(stamp) },
                },
            );
        }
        Ok((watcher, count))
    }

    /// Looks at the folder at `now` and returns the files to send, marking them sent.
    pub fn poll(&mut self, now: Instant) -> Vec<Ready> {
        let found = self.scan();
        self.seen.retain(|path, _| found.contains_key(path));
        let mut ready = Vec::new();
        for (path, (file, stamp)) in found {
            let seen = self.seen.entry(path.clone()).or_insert(Seen {
                stamp,
                since: now,
                sent: None,
            });
            if seen.stamp != stamp {
                seen.stamp = stamp;
                seen.since = now;
            }
            let settled = now.saturating_duration_since(seen.since) >= self.debounce;
            if settled && seen.sent != Some(stamp) && File::open(&file).is_ok() {
                seen.sent = Some(stamp);
                ready.push(Ready {
                    path,
                    file,
                    size: stamp.0,
                });
            }
        }
        ready.sort_by(|a, b| a.path.cmp(&b.path));
        ready
    }

    /// Every watched file now: relative path -> (real path, stamp).
    fn scan(&self) -> HashMap<String, (PathBuf, Stamp)> {
        let mut found = HashMap::new();
        let mut folders = vec![(self.folder.clone(), String::new(), 0usize)];
        while let Some((dir, prefix, depth)) = folders.pop() {
            let Ok(read) = std::fs::read_dir(&dir) else {
                continue;
            };
            for item in read.flatten() {
                if found.len() >= MAX_FILES {
                    return found;
                }
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
                if !real.starts_with(&self.root) {
                    continue;
                }
                let Ok(meta) = std::fs::metadata(&real) else {
                    continue;
                };
                let relative = format!("{prefix}{name}");
                if meta.is_dir() {
                    if depth < MAX_DEPTH {
                        folders.push((real, format!("{relative}/"), depth + 1));
                    }
                } else if meta.is_file()
                    && self
                        .pattern
                        .as_deref()
                        .is_none_or(|p| glob_matches(p, &name))
                {
                    found.insert(relative, (real, (meta.len(), meta.modified().ok())));
                }
            }
        }
        found
    }
}

/// A watch channel as the channel table sees it: a thread looks at the folder and sends what
/// settled; the client sends nothing.
pub(crate) struct WatchProcess {
    cancelled: Arc<AtomicBool>,
    ended: Arc<(Mutex<bool>, Condvar)>,
}

impl WatchProcess {
    /// Starts watching, or says why not. `send` carries one reply to the client and returns
    /// false once the channel is gone.
    pub(crate) fn start(
        shares: &Shares,
        share: &str,
        path: &str,
        pattern: Option<String>,
        existing: bool,
        send: Box<dyn Fn(Vec<u8>) -> bool + Send + Sync>,
    ) -> Result<WatchProcess, String> {
        let (mut watcher, count) = Watcher::new(
            shares,
            share,
            path,
            pattern,
            existing,
            DEBOUNCE,
            Instant::now(),
        )?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let ended = Arc::new((Mutex::new(false), Condvar::new()));
        let (flag, done) = (Arc::clone(&cancelled), Arc::clone(&ended));
        std::thread::spawn(move || {
            run(&mut watcher, count, &send, &flag);
            *done.0.lock().expect("watch") = true;
            done.1.notify_all();
        });
        Ok(WatchProcess { cancelled, ended })
    }
}

fn run(
    watcher: &mut Watcher,
    existing: u32,
    send: &(dyn Fn(Vec<u8>) -> bool + Send + Sync),
    cancelled: &AtomicBool,
) {
    let emit = |reply: WatchReply| send(encode_watch(&reply));
    if !emit(WatchReply::Started { existing }) {
        return;
    }
    while !cancelled.load(Ordering::SeqCst) {
        for ready in watcher.poll(Instant::now()) {
            if cancelled.load(Ordering::SeqCst) {
                return;
            }
            match stream(&ready, &emit, cancelled) {
                Ok(true) => {}
                Ok(false) => return,
                Err(e) => {
                    // The file went away or cannot be read now: it is looked at again as it changes.
                    tracing::debug!("watch: {}: {e}", ready.path);
                }
            }
        }
        std::thread::sleep(POLL_EVERY);
    }
}

/// Sends one file. `Ok(false)` when the channel is gone.
fn stream(
    ready: &Ready,
    emit: &dyn Fn(WatchReply) -> bool,
    cancelled: &AtomicBool,
) -> std::io::Result<bool> {
    let mut file = File::open(&ready.file)?;
    if !emit(WatchReply::File {
        path: ready.path.clone(),
        size: ready.size,
    }) {
        return Ok(false);
    }
    let mut buf = vec![0u8; FILE_CHUNK];
    loop {
        if cancelled.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if !emit(WatchReply::Chunk {
            bytes: buf[..n].to_vec(),
        }) {
            return Ok(false);
        }
    }
    Ok(emit(WatchReply::End))
}

impl RunningProcess for WatchProcess {
    fn write(&self, _bytes: &[u8]) -> std::io::Result<()> {
        // A watch listens to nothing.
        Ok(())
    }

    fn eof(&self) {}

    fn kill(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    fn wait(&self) -> Option<i32> {
        let (lock, changed) = &*self.ended;
        let mut ended = lock.lock().expect("watch");
        while !*ended {
            ended = changed.wait(ended).expect("watch");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("windowcast-watch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn shares(root: &Path) -> Shares {
        let mut shares = Shares::default();
        shares.add("saves", root, false).unwrap();
        shares
    }

    const SECOND: Duration = Duration::from_secs(1);

    fn paths(ready: &[Ready]) -> Vec<&str> {
        ready.iter().map(|r| r.path.as_str()).collect()
    }

    #[test]
    fn a_new_file_is_sent_once_it_has_been_quiet_for_the_debounce() {
        let root = temp("debounce");
        let t0 = Instant::now();
        let (mut watcher, existing) =
            Watcher::new(&shares(&root), "saves", "", None, false, 2 * SECOND, t0).unwrap();
        assert_eq!(existing, 0);
        std::fs::write(root.join("slot1.sav"), b"one").unwrap();
        assert!(watcher.poll(t0 + SECOND).is_empty(), "just seen: not yet");
        assert!(watcher.poll(t0 + 2 * SECOND).is_empty(), "seen 1 s ago");
        let ready = watcher.poll(t0 + 3 * SECOND);
        assert_eq!(paths(&ready), ["slot1.sav"]);
        assert_eq!(ready[0].size, 3);
        // Sent once.
        assert!(watcher.poll(t0 + 10 * SECOND).is_empty());
    }

    #[test]
    fn a_file_still_being_written_is_not_sent_early() {
        let root = temp("growing");
        let t0 = Instant::now();
        let (mut watcher, _) =
            Watcher::new(&shares(&root), "saves", "", None, false, 2 * SECOND, t0).unwrap();
        std::fs::write(root.join("big.sav"), b"1").unwrap();
        watcher.poll(t0 + SECOND);
        // It grows every second, so it never settles.
        for step in 2..8u64 {
            std::fs::write(root.join("big.sav"), vec![b'x'; step as usize]).unwrap();
            assert!(
                watcher.poll(t0 + SECOND * step as u32).is_empty(),
                "step {step}"
            );
        }
        // It stops; two quiet seconds later it goes.
        assert!(watcher.poll(t0 + 8 * SECOND).is_empty());
        let ready = watcher.poll(t0 + 10 * SECOND);
        assert_eq!(paths(&ready), ["big.sav"]);
        assert_eq!(ready[0].size, 7);
    }

    #[test]
    fn a_change_to_a_sent_file_sends_it_again() {
        let root = temp("again");
        let t0 = Instant::now();
        let (mut watcher, _) =
            Watcher::new(&shares(&root), "saves", "", None, false, SECOND, t0).unwrap();
        std::fs::write(root.join("a.sav"), b"v1").unwrap();
        watcher.poll(t0);
        assert_eq!(paths(&watcher.poll(t0 + 2 * SECOND)), ["a.sav"]);
        std::fs::write(root.join("a.sav"), b"version two").unwrap();
        assert!(watcher.poll(t0 + 3 * SECOND).is_empty());
        let ready = watcher.poll(t0 + 5 * SECOND);
        assert_eq!((paths(&ready), ready[0].size), (vec!["a.sav"], 11));
    }

    #[test]
    fn existing_files_are_sent_only_when_asked_for() {
        let root = temp("existing");
        std::fs::write(root.join("old.sav"), b"old").unwrap();
        let t0 = Instant::now();
        let (mut quiet, count) =
            Watcher::new(&shares(&root), "saves", "", None, false, SECOND, t0).unwrap();
        assert_eq!(count, 1);
        assert!(quiet.poll(t0 + 5 * SECOND).is_empty());
        let (mut eager, _) =
            Watcher::new(&shares(&root), "saves", "", None, true, SECOND, t0).unwrap();
        eager.poll(t0);
        assert_eq!(paths(&eager.poll(t0 + 2 * SECOND)), ["old.sav"]);
    }

    #[test]
    fn a_pattern_limits_names_and_folders_are_followed() {
        let root = temp("pattern");
        std::fs::create_dir_all(root.join("profiles/p1")).unwrap();
        let t0 = Instant::now();
        let (mut watcher, _) = Watcher::new(
            &shares(&root),
            "saves",
            "profiles",
            Some("*.sav".into()),
            false,
            SECOND,
            t0,
        )
        .unwrap();
        std::fs::write(root.join("profiles/p1/a.sav"), b"a").unwrap();
        std::fs::write(root.join("profiles/p1/a.log"), b"log").unwrap();
        std::fs::write(root.join("outside.sav"), b"no").unwrap();
        watcher.poll(t0);
        assert_eq!(paths(&watcher.poll(t0 + 2 * SECOND)), ["p1/a.sav"]);
    }

    #[test]
    fn partial_uploads_and_links_out_of_the_share_are_not_watched() {
        let root = temp("skip");
        let t0 = Instant::now();
        let (mut watcher, _) =
            Watcher::new(&shares(&root), "saves", "", None, false, SECOND, t0).unwrap();
        std::fs::write(root.join("half.sav.wcpart"), b"x").unwrap();
        #[cfg(unix)]
        {
            let outside = temp("skip-outside");
            std::fs::write(outside.join("secret"), b"s").unwrap();
            std::os::unix::fs::symlink(outside.join("secret"), root.join("link")).unwrap();
        }
        watcher.poll(t0);
        assert!(watcher.poll(t0 + 3 * SECOND).is_empty());
    }

    #[test]
    fn a_bad_start_is_refused() {
        let root = temp("bad");
        std::fs::write(root.join("file"), b"x").unwrap();
        let s = shares(&root);
        let t0 = Instant::now();
        let start = |share: &str, path: &str, pattern: Option<&str>| {
            Watcher::new(
                &s,
                share,
                path,
                pattern.map(str::to_owned),
                false,
                SECOND,
                t0,
            )
            .err()
        };
        assert!(start("nope", "", None).unwrap().contains("no share"));
        assert!(start("saves", "file", None)
            .unwrap()
            .contains("not a folder"));
        assert!(start("saves", "../x", None).is_some());
        assert!(start("saves", "", Some("a/b")).unwrap().contains("pattern"));
    }
}

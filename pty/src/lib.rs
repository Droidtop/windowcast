//! A shell on a pseudo-terminal: ConPTY on Windows, openpty everywhere
//! else, behind one small type. The host side of a windowcast terminal
//! and the shell the tests' SSH server runs both use
//! it. Blocking by nature: the reader blocks in `read`, so callers read on
//! a thread of their own.

use std::io::{Read, Write};
use std::sync::Mutex;

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

/// A terminal's size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Size {
    fn pty(self) -> PtySize {
        PtySize {
            rows: self.rows.max(1),
            cols: self.cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

/// What to run. `program` is `None` for the user's own shell.
#[derive(Debug, Clone)]
pub struct Spawn {
    pub size: Size,
    /// The `TERM` the program is told; empty leaves it unset.
    pub term: String,
    /// A program and its arguments instead of the user's shell.
    pub program: Option<Vec<String>>,
}

/// The shell a host runs for a terminal when it is not told otherwise:
/// `$SHELL` (or `sh`) on Unix, PowerShell on Windows.
pub fn default_shell() -> Vec<String> {
    if cfg!(windows) {
        vec!["powershell.exe".into(), "-NoLogo".into()]
    } else {
        vec![std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| "/bin/sh".into())]
    }
}

pub struct Pty {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    reader: Mutex<Option<Box<dyn Read + Send>>>,
}

impl Pty {
    pub fn spawn(spawn: &Spawn) -> std::io::Result<Pty> {
        let pair = native_pty_system()
            .openpty(spawn.size.pty())
            .map_err(std::io::Error::other)?;
        let argv = spawn.program.clone().unwrap_or_else(default_shell);
        let mut command = CommandBuilder::new(&argv[0]);
        command.args(&argv[1..]);
        if !spawn.term.is_empty() {
            command.env("TERM", &spawn.term);
        }
        if let Some(home) = dirs_home() {
            command.cwd(home);
        }
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(std::io::Error::other)?;
        // The slave end belongs to the child now; keeping ours open would
        // stop the reader seeing the end of the shell.
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(std::io::Error::other)?;
        let writer = pair.master.take_writer().map_err(std::io::Error::other)?;
        Ok(Pty {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            reader: Mutex::new(Some(reader)),
        })
    }

    /// The output side, once. Reading blocks; it ends when the shell has
    /// gone and its output is read.
    pub fn take_reader(&self) -> Option<Box<dyn Read + Send>> {
        self.reader.lock().expect("pty reader").take()
    }

    /// Types `bytes` into the terminal.
    pub fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut writer = self.writer.lock().expect("pty writer");
        writer.write_all(bytes)?;
        writer.flush()
    }

    pub fn resize(&self, size: Size) -> std::io::Result<()> {
        self.master
            .lock()
            .expect("pty master")
            .resize(size.pty())
            .map_err(std::io::Error::other)
    }

    /// Ends the shell.
    pub fn kill(&self) {
        let _ = self.child.lock().expect("pty child").kill();
    }

    /// The shell's exit code if it has ended; `None` while it runs. A shell
    /// killed by a signal, or one the system gives no code for, reads as
    /// `Some(None)`.
    pub fn try_exit_code(&self) -> Option<Option<i32>> {
        match self.child.lock().expect("pty child").try_wait() {
            Ok(Some(status)) => Some(i32::try_from(status.exit_code()).ok()),
            _ => None,
        }
    }

    /// Waits for the shell to end.
    pub fn wait(&self) -> Option<i32> {
        let status = self.child.lock().expect("pty child").wait().ok()?;
        i32::try_from(status.exit_code()).ok()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.kill();
    }
}

fn dirs_home() -> Option<std::path::PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_dir())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn read_until(reader: &mut dyn Read, needle: &str) -> String {
        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        while !String::from_utf8_lossy(&seen).contains(needle) {
            let n = reader.read(&mut buf).expect("read");
            assert!(
                n > 0,
                "the shell ended before {needle:?}; saw {:?}",
                String::from_utf8_lossy(&seen)
            );
            seen.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    fn sh() -> Spawn {
        Spawn {
            size: Size {
                cols: 100,
                rows: 30,
            },
            term: "xterm-256color".into(),
            program: Some(vec!["/bin/sh".into()]),
        }
    }

    #[test]
    fn a_shell_runs_commands_and_reports_the_terminal_size() {
        let pty = Pty::spawn(&sh()).unwrap();
        let mut reader = pty.take_reader().unwrap();
        pty.write(b"echo hi-$((20+22)); stty size\n").unwrap();
        let out = read_until(&mut *reader, "30 100");
        assert!(out.contains("hi-42"), "{out:?}");

        pty.resize(Size { cols: 61, rows: 17 }).unwrap();
        pty.write(b"stty size; echo $TERM\n").unwrap();
        let out = read_until(&mut *reader, "xterm-256color");
        assert!(out.contains("17 61"), "{out:?}");
    }

    #[test]
    fn the_exit_code_comes_back() {
        let pty = Pty::spawn(&sh()).unwrap();
        let mut reader = pty.take_reader().unwrap();
        pty.write(b"exit 7\n").unwrap();
        let mut sink = Vec::new();
        let _ = reader.read_to_end(&mut sink);
        assert_eq!(pty.wait(), Some(7));
    }
}

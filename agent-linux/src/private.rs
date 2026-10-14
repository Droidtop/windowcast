//! The Linux host's private session (docs/BACKENDS.md, "A private
//! session"): a sway of the agent's own with no screen
//! (`WLR_BACKENDS=headless`), whose windows the agent serves instead of
//! the seat's while the session is on. Nothing shows on the computer's
//! screen and the seat's lock screen does not reach it. It stays up while
//! the host runs, so ending the private session leaves its programs
//! running for the next one.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::display::Display;

/// The sway to run: `$WINDOWCAST_SWAY`, or `sway` from the path.
fn program() -> PathBuf {
    std::env::var_os("WINDOWCAST_SWAY").map_or_else(|| PathBuf::from("sway"), PathBuf::from)
}

/// Whether this computer can have a private session, or why not, in words
/// its user understands.
pub fn available() -> Result<(), String> {
    let program = program();
    let found = if program.components().count() > 1 {
        program.is_file()
    } else {
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| dir.join(&program).is_file())
        })
    };
    if !found {
        return Err(
            "sway is not installed on this computer, and a private session runs in a \
                    sway of its own."
                .into(),
        );
    }
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        return Err("this computer's session has no XDG_RUNTIME_DIR for a sway of its own.".into());
    }
    Ok(())
}

/// The Wayland sockets in `dir`, by name.
fn wayland_sockets(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("wayland-") && !name.ends_with(".lock"))
        .collect()
}

/// sway's IPC socket for process `pid` in `dir`
/// (`sway-ipc.<uid>.<pid>.sock`).
fn sway_socket(dir: &Path, pid: u32) -> Option<PathBuf> {
    let suffix = format!(".{pid}.sock");
    std::fs::read_dir(dir).ok()?.find_map(|e| {
        let name = e.ok()?.file_name().into_string().ok()?;
        (name.starts_with("sway-ipc.") && name.ends_with(&suffix)).then(|| dir.join(name))
    })
}

/// The headless sway.
pub struct Headless {
    child: Child,
    display: Display,
}

impl Headless {
    /// Starts it, at 1920x1080, and waits for its sockets.
    pub fn start() -> Result<Self, String> {
        available()?;
        let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("checked"));
        let config = runtime.join("windowcast-private-sway.conf");
        std::fs::write(&config, "output HEADLESS-1 resolution 1920x1080\n")
            .map_err(|e| format!("could not write {}: {e}", config.display()))?;
        let log = std::fs::File::create(runtime.join("windowcast-private-sway.log"))
            .map_err(|e| format!("could not write sway's log: {e}"))?;
        let before = wayland_sockets(&runtime);
        let mut child = Command::new(program())
            .arg("-c")
            .arg(&config)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("SWAYSOCK")
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .map_err(|e| format!("could not start sway: {e}"))?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!(
                    "sway stopped at once ({status}); its log is {}",
                    runtime.join("windowcast-private-sway.log").display()
                ));
            }
            let name = wayland_sockets(&runtime)
                .into_iter()
                .find(|name| !before.contains(name));
            if let (Some(name), Some(sway)) = (name, sway_socket(&runtime, child.id())) {
                return Ok(Headless {
                    display: Display {
                        wayland: runtime.join(&name),
                        name,
                        sway,
                    },
                    child,
                });
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("sway did not start within 15 s".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn display(&self) -> Display {
        self.display.clone()
    }

    /// Whether it is still running.
    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Headless {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

//! The compositor the agent serves: the seat's (`WAYLAND_DISPLAY` and
//! `SWAYSOCK` from the environment), or during a private session a
//! headless sway of the agent's own (`crate::private`; docs/BACKENDS.md,
//! "A private session"). Every Wayland and sway connection the agent makes
//! goes through here.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use wayland_client::{ConnectError, Connection};

/// A compositor by its sockets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    /// Its Wayland socket's name in `$XDG_RUNTIME_DIR` (`wayland-1`) and
    /// path.
    pub name: String,
    pub wayland: PathBuf,
    /// Its sway IPC socket.
    pub sway: PathBuf,
}

static CURRENT: RwLock<Option<Display>> = RwLock::new(None);
/// Counts changes of [`CURRENT`], so what was made for one compositor
/// (input devices) is made again for the next.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Serves `display` from now on; `None` goes back to the seat's.
pub fn set(display: Option<Display>) {
    let mut current = CURRENT.write().expect("display");
    if *current != display {
        *current = display;
        GENERATION.fetch_add(1, Ordering::SeqCst);
    }
}

/// The compositor served when it is not the seat's.
pub fn current() -> Option<Display> {
    CURRENT.read().expect("display").clone()
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

/// A Wayland connection to the compositor served now.
pub fn connect() -> Result<Connection, ConnectError> {
    match current() {
        Some(display) => {
            let stream =
                UnixStream::connect(&display.wayland).map_err(|_| ConnectError::NoCompositor)?;
            Connection::from_socket(stream)
        }
        None => Connection::connect_to_env(),
    }
}

/// The sway IPC socket of the compositor served now, when it is sway.
pub fn sway_socket() -> Option<PathBuf> {
    match current() {
        Some(display) => Some(display.sway),
        None => std::env::var_os("SWAYSOCK").map(PathBuf::from),
    }
}

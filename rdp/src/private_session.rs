//! A private session on a Windows host (docs/BACKENDS.md, "A private
//! session"): the host logs in to its own Remote Desktop as the user it
//! runs as. Windows 10 and 11 run one interactive session, so the login
//! takes that session over: the computer's screen goes to the sign-in
//! screen, and the session, the host and its programs go on, drawn for
//! this connection instead of the screen, unlocked, out of sight. The
//! connection's pictures are dropped; closing it leaves the session
//! disconnected and locked until someone signs in at the computer.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::Ordering;

use crate::client::{self, ClientConfig, RdpStream};
use crate::remoteapp_host::{self, Status, WindowsRemoteApps};

/// Whether this computer can have a private session, or why not, in words
/// its user understands.
pub fn support() -> Result<(), String> {
    let status = WindowsRemoteApps::status()?;
    why_not(&status).map_or(Ok(()), |reason| Err(reason.to_owned()))
}

/// Why a computer with `status` cannot have a private session.
pub fn why_not(status: &Status) -> Option<&'static str> {
    if status.home {
        Some(
            "This PC runs Windows Home, which has no Remote Desktop to move its session to. \
             Windows Pro, Enterprise and Education have it.",
        )
    } else if status.port.is_none() {
        Some("Remote Desktop is off on this PC. Turn it on in Settings, System, Remote Desktop.")
    } else {
        None
    }
}

/// The host's own Remote Desktop connection, holding the session.
pub struct PrivateSession {
    stream: RdpStream,
}

impl PrivateSession {
    /// Logs in to this computer's Remote Desktop as the user the host runs
    /// as, with `password`, on a desktop of `size`.
    pub fn start(password: &str, size: (u16, u16)) -> Result<Self, String> {
        let status = WindowsRemoteApps::status()?;
        if let Some(reason) = why_not(&status) {
            return Err(reason.to_owned());
        }
        let port = status.port.expect("Remote Desktop is on");
        let username = remoteapp_host::current_user()?;
        // A loopback address of its own: Remote Desktop clients refuse
        // 127.0.0.1, the server takes the whole range.
        let address = SocketAddr::from((Ipv4Addr::new(127, 0, 0, 2), port));
        let stream = client::connect(&ClientConfig {
            address,
            server_name: "localhost".into(),
            username,
            password: password.to_owned(),
            domain: None,
            size,
            pinned: None,
            remote_app: None,
        })
        .map_err(|e| format!("Remote Desktop refused the sign-in ({e})"))?;
        Ok(PrivateSession { stream })
    }

    /// Whether the connection ended (someone signed in at the computer,
    /// which takes the session back, or Remote Desktop closed it).
    pub fn ended(&self) -> bool {
        self.stream.ended.load(Ordering::SeqCst)
    }
}

impl Drop for PrivateSession {
    fn drop(&mut self) {
        self.stream.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(home: bool, port: Option<u16>) -> Status {
        Status {
            server: false,
            home,
            port,
            session_free: true,
            any_program: true,
        }
    }

    #[test]
    fn a_private_session_needs_remote_desktop_and_not_windows_home() {
        assert_eq!(why_not(&status(false, Some(3389))), None);
        assert!(why_not(&status(true, Some(3389))).is_some_and(|w| w.contains("Windows Home")));
        assert!(why_not(&status(false, None)).is_some_and(|w| w.contains("Remote Desktop is off")));
    }
}

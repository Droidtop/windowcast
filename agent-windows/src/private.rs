//! The Windows host's private session (docs/BACKENDS.md, "A private
//! session"): the session moved to a Remote Desktop connection of the
//! host's own (`windowcast_rdp::private_session`), so its programs run
//! unlocked and out of sight of the computer's screen.

use std::sync::Mutex;

use windowcast_host::PrivateRefusal;
use windowcast_protocol::PrivateSession;
use windowcast_rdp::private_session::{self, PrivateSession as Connection};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

#[derive(Default)]
pub struct Private {
    connection: Mutex<Option<Connection>>,
}

impl Private {
    /// On, offered (when `offered`, the owner's choice), or why not.
    pub fn state(&self, offered: bool) -> PrivateSession {
        let mut connection = self.connection.lock().expect("private session");
        if connection.as_ref().is_some_and(Connection::ended) {
            // Someone signed in at the computer, which takes the session back.
            *connection = None;
        }
        if connection.is_some() {
            return PrivateSession::Active;
        }
        if !offered {
            return PrivateSession::Off;
        }
        match private_session::support() {
            Ok(()) => PrivateSession::Available,
            Err(reason) => PrivateSession::Unavailable(reason),
        }
    }

    pub fn set(&self, start: bool, password: Option<&str>) -> Result<(), PrivateRefusal> {
        let mut connection = self.connection.lock().expect("private session");
        if !start {
            *connection = None;
            return Ok(());
        }
        if connection.as_ref().is_some_and(|c| !c.ended()) {
            return Ok(());
        }
        let Some(password) = password else {
            return Err(PrivateRefusal {
                reason: "A private session signs in to this PC's Remote Desktop, which needs \
                         the Windows password of the user windowcast runs as."
                    .into(),
                password_needed: true,
            });
        };
        // The session keeps the screen's size.
        let size = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        let size = (
            size.0.clamp(640, 8192) as u16,
            size.1.clamp(480, 8192) as u16,
        );
        let started = Connection::start(password, size).map_err(|reason| PrivateRefusal {
            reason,
            password_needed: true,
        })?;
        *connection = Some(started);
        Ok(())
    }
}

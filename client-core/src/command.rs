//! The command stream from the client (docs/COMMAND-STREAM.md): shells,
//! commands and application launches opened as channels, on the host of a
//! windowcast session or on any SSH server, as the same
//! [`windowcast_terminal::Channel`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::runtime::Runtime;
use windowcast_protocol::command::{ChannelId, ChannelKind, CommandMessage, Pty, TerminalSize};
use windowcast_protocol::ControlMessage;
use windowcast_terminal::{
    Channel, ChannelEvent, Command, HostKeyPolicy, SshAuth, SshConnection, SshTarget, Terminal,
};

use crate::{Client, ClientError, ClientSession};

/// How long a host or server has to answer an open.
const OPEN_WITHIN: Duration = Duration::from_secs(15);

/// The `TERM` the screen model understands.
pub const TERM: &str = "xterm-256color";

struct Route {
    /// Waiting for `Opened` or `Refused`.
    opened: Option<mpsc::Sender<Result<Option<u32>, String>>>,
    events: mpsc::Sender<ChannelEvent>,
}

/// The channels of one session, by id.
#[derive(Default)]
pub(crate) struct Routes {
    map: Mutex<HashMap<ChannelId, Route>>,
    next: AtomicU32,
}

impl Routes {
    /// A host's message for one of this session's channels.
    pub(crate) fn handle(&self, message: CommandMessage) {
        let mut map = self.map.lock().expect("channels");
        match message {
            CommandMessage::Opened { id, pid } => {
                if let Some(opened) = map.get_mut(&id).and_then(|r| r.opened.take()) {
                    let _ = opened.send(Ok(pid));
                }
            }
            CommandMessage::Refused { id, reason } => {
                if let Some(route) = map.remove(&id) {
                    if let Some(opened) = route.opened {
                        let _ = opened.send(Err(reason));
                    }
                }
            }
            CommandMessage::Data { id, bytes } => {
                if let Some(route) = map.get(&id) {
                    let _ = route.events.send(ChannelEvent::Data(bytes));
                }
            }
            CommandMessage::Exited { id, code } => {
                if let Some(route) = map.remove(&id) {
                    let _ = route.events.send(ChannelEvent::Exited(code));
                    let _ = route.events.send(ChannelEvent::Closed);
                }
            }
            _ => {}
        }
    }

    /// The session ended: every channel ends with it.
    pub(crate) fn close_all(&self) {
        for (_, route) in self.map.lock().expect("channels").drain() {
            if let Some(opened) = route.opened {
                let _ = opened.send(Err("the session ended".into()));
            }
            let _ = route.events.send(ChannelEvent::Closed);
        }
    }

    fn forget(&self, id: ChannelId) {
        self.map.lock().expect("channels").remove(&id);
    }
}

impl ClientSession {
    /// Opens a channel of `kind` on the host. Blocks until the host
    /// answers. The host may refuse (it does not allow commands, or the
    /// user is not allowed): the reason is in the error.
    pub fn open_channel(&self, kind: ChannelKind) -> Result<Channel, ClientError> {
        let routes = Arc::clone(&self.shared.channels);
        let id = ChannelId(routes.next.fetch_add(1, Ordering::SeqCst));
        let (channel, end) = Channel::pair(None);
        let (opened_tx, opened_rx) = mpsc::channel();
        routes.map.lock().expect("channels").insert(
            id,
            Route {
                opened: Some(opened_tx),
                events: end.events.clone(),
            },
        );
        let send = |message| {
            self.runtime
                .block_on(self.session.send_control(&ControlMessage::Command(message)))
        };
        if let Err(e) = send(CommandMessage::Open { id, kind }) {
            routes.forget(id);
            return Err(e.into());
        }
        let pid = match opened_rx.recv_timeout(OPEN_WITHIN) {
            Ok(Ok(pid)) => pid,
            Ok(Err(reason)) => return Err(ClientError::Refused(reason)),
            Err(_) => {
                routes.forget(id);
                return Err(ClientError::Refused(
                    "the host did not answer (it may not support commands)".into(),
                ));
            }
        };
        channel.set_pid(pid);

        // From here the channel's commands go to the host as messages.
        let session = Arc::clone(&self.session);
        let mut commands = end.commands;
        self.runtime.spawn(async move {
            while let Some(command) = commands.recv().await {
                let message = match command {
                    Command::Data(bytes) => CommandMessage::Data { id, bytes },
                    Command::Resize(size) => CommandMessage::Resize { id, size },
                    Command::Eof => CommandMessage::Eof { id },
                    Command::Close => CommandMessage::Close { id },
                };
                let closing = matches!(message, CommandMessage::Close { .. });
                if session
                    .send_control(&ControlMessage::Command(message))
                    .await
                    .is_err()
                    || closing
                {
                    break;
                }
            }
            routes.forget(id);
        });
        Ok(channel)
    }

    /// A shell on the host, drawn on a screen of `size`.
    pub fn open_terminal(&self, size: TerminalSize) -> Result<Terminal, ClientError> {
        let channel = self.open_channel(ChannelKind::Shell(Pty {
            size,
            term: TERM.into(),
        }))?;
        Ok(Terminal::new(channel, size))
    }

    /// Starts an application on the host. Its windows show up in the
    /// window list. Returns its process id when the host knows it.
    pub fn launch(&self, argv: Vec<String>) -> Result<Option<u32>, ClientError> {
        Ok(self.open_channel(ChannelKind::Launch { argv })?.pid())
    }

    /// Runs a command on the host and waits for it: its output (standard
    /// output and error together) and exit code.
    pub fn exec(
        &self,
        argv: Vec<String>,
        within: Duration,
    ) -> Result<(Vec<u8>, Option<i32>), ClientError> {
        collect(
            self.open_channel(ChannelKind::Exec { argv, pty: None })?,
            within,
        )
    }
}

/// Reads a channel to its end.
fn collect(channel: Channel, within: Duration) -> Result<(Vec<u8>, Option<i32>), ClientError> {
    let deadline = Instant::now() + within;
    let (mut output, mut code) = (Vec::new(), None);
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| ClientError::Refused("the command did not finish in time".into()))?;
        match channel.next_event(left.min(Duration::from_millis(250))) {
            Some(ChannelEvent::Data(bytes)) => output.extend(bytes),
            Some(ChannelEvent::Exited(c)) => code = c,
            Some(ChannelEvent::Closed) => return Ok((output, code)),
            None => {}
        }
    }
}

/// A login to an SSH server, kept for opening channels on it.
pub struct SshSession {
    runtime: Arc<Runtime>,
    connection: SshConnection,
}

impl Client {
    /// Logs in to an SSH server. Its host key is pinned in this client's
    /// store (`ssh-known-hosts.json`); `policy` decides what to do with a
    /// server not pinned yet. A key that changed is always refused.
    pub fn ssh_connect(
        &self,
        target: &SshTarget,
        auth: &SshAuth,
        policy: HostKeyPolicy,
    ) -> Result<SshSession, ClientError> {
        let store = Arc::clone(&self.host_keys);
        let connection = self
            .runtime
            .block_on(windowcast_terminal::connect(target, auth, store, policy))?;
        Ok(SshSession {
            runtime: Arc::clone(&self.runtime),
            connection,
        })
    }
}

impl SshSession {
    /// The server's host key fingerprint.
    pub fn fingerprint(&self) -> &str {
        self.connection.fingerprint()
    }

    pub fn open_channel(&self, kind: ChannelKind) -> Result<Channel, ClientError> {
        Ok(self.runtime.block_on(self.connection.open(&kind))?)
    }

    /// A shell on the server, drawn on a screen of `size`.
    pub fn open_terminal(&self, size: TerminalSize) -> Result<Terminal, ClientError> {
        let channel = self.open_channel(ChannelKind::Shell(Pty {
            size,
            term: TERM.into(),
        }))?;
        Ok(Terminal::new(channel, size))
    }

    pub fn launch(&self, argv: Vec<String>) -> Result<Option<u32>, ClientError> {
        Ok(self.open_channel(ChannelKind::Launch { argv })?.pid())
    }

    pub fn exec(
        &self,
        argv: Vec<String>,
        within: Duration,
    ) -> Result<(Vec<u8>, Option<i32>), ClientError> {
        collect(
            self.open_channel(ChannelKind::Exec { argv, pty: None })?,
            within,
        )
    }

    pub fn is_closed(&self) -> bool {
        self.connection.is_closed()
    }
}

impl Drop for SshSession {
    fn drop(&mut self) {
        self.runtime.block_on(self.connection.close());
    }
}

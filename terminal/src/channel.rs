//! A channel of the command stream as a client holds it, whatever carries
//! it (the windowcast session, or an SSH server): bytes in, bytes and the
//! command's end out. The transport keeps the other half ([`ChannelEnd`])
//! and moves [`Command`]s onto the wire and wire events into
//! [`ChannelEvent`]s.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use windowcast_protocol::command::TerminalSize;

/// What the client asks of a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Data(Vec<u8>),
    Resize(TerminalSize),
    /// No more input.
    Eof,
    /// End the command.
    Close,
}

/// What a channel reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelEvent {
    Data(Vec<u8>),
    /// The command ended, with its exit code when there is one.
    Exited(Option<i32>),
    /// The channel (or the whole connection) is gone. Always last.
    Closed,
}

/// The client's half.
pub struct Channel {
    commands: UnboundedSender<Command>,
    events: Mutex<mpsc::Receiver<ChannelEvent>>,
    /// 0 until known.
    pid: AtomicU32,
}

/// The transport's half.
pub struct ChannelEnd {
    pub commands: UnboundedReceiver<Command>,
    pub events: mpsc::Sender<ChannelEvent>,
}

impl Channel {
    /// A channel and its transport end. `pid` is the host's process id for
    /// the command when the transport learned it.
    pub fn pair(pid: Option<u32>) -> (Channel, ChannelEnd) {
        let (command_tx, commands) = tokio::sync::mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::channel();
        (
            Channel {
                commands: command_tx,
                events: Mutex::new(event_rx),
                pid: AtomicU32::new(pid.unwrap_or(0)),
            },
            ChannelEnd { commands, events },
        )
    }

    pub fn pid(&self) -> Option<u32> {
        Some(self.pid.load(Ordering::SeqCst)).filter(|pid| *pid != 0)
    }

    /// Transport side: the process id the host reported.
    pub fn set_pid(&self, pid: Option<u32>) {
        self.pid.store(pid.unwrap_or(0), Ordering::SeqCst);
    }

    /// Sends input. Never blocks; an ended channel ignores it.
    pub fn write(&self, bytes: &[u8]) {
        for chunk in bytes.chunks(windowcast_protocol::command::MAX_CHUNK) {
            let _ = self.commands.send(Command::Data(chunk.to_vec()));
        }
    }

    pub fn resize(&self, size: TerminalSize) {
        let _ = self.commands.send(Command::Resize(size.clamped()));
    }

    pub fn eof(&self) {
        let _ = self.commands.send(Command::Eof);
    }

    pub fn close(&self) {
        let _ = self.commands.send(Command::Close);
    }

    /// The next event, waiting up to `timeout`. `None` on timeout; after
    /// [`ChannelEvent::Closed`] every call returns `Closed`.
    pub fn next_event(&self, timeout: Duration) -> Option<ChannelEvent> {
        match self
            .events
            .lock()
            .expect("channel events")
            .recv_timeout(timeout)
        {
            Ok(event) => Some(event),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(ChannelEvent::Closed),
        }
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.close();
    }
}

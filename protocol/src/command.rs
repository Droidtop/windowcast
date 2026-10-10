//! The command stream (docs/COMMAND-STREAM.md): numbered channels between
//! a client and a host, each opened for one purpose ([`ChannelKind`]: a
//! shell on a pseudo-terminal, one command, an application launch) and
//! carrying bytes both ways until it ends. They ride the session's control
//! channel, so they are always there while the session is, and the same
//! kinds open as SSH channels on any SSH server.
//!
//! A command is something the user asks for, not something a window looks
//! like, so [`crate::selection`] never chooses one and no default rule
//! leads to it. A launched application's windows appear in the window list
//! and are chosen a backend like any other.
//!
//! The client numbers its channels ([`ChannelId`]); the host answers each
//! `Open` with `Opened` or `Refused`, then both sides stream `Data` until
//! one closes it or the command exits. A shell's bytes are the terminal's
//! own: output with its escape sequences to the client, keystrokes (and
//! pasted text, in bracketed-paste form when the program asked) to the
//! host. A program sets the clipboard in the stream (OSC 52), which the
//! client's screen turns into a clipboard event, so no message of its own
//! carries it.

use serde::{Deserialize, Serialize};

/// A channel of one session, numbered by the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelId(pub u32);

/// A terminal's size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
}

impl TerminalSize {
    pub const MAX: u16 = 1000;

    /// Clamps to what a pseudo-terminal can be: at least one cell, and no
    /// larger than [`TerminalSize::MAX`].
    pub fn clamped(self) -> TerminalSize {
        TerminalSize {
            cols: self.cols.clamp(1, Self::MAX),
            rows: self.rows.clamp(1, Self::MAX),
        }
    }
}

/// A pseudo-terminal a command runs on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pty {
    pub size: TerminalSize,
    /// The `TERM` the client's screen understands.
    pub term: String,
}

/// What a channel is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelKind {
    /// The host's own shell on a pseudo-terminal.
    Shell(Pty),
    /// One command, `argv[0]` and its arguments, on a pseudo-terminal when
    /// asked for one. The channel carries its input and output; it ends
    /// with the command's exit code.
    Exec { argv: Vec<String>, pty: Option<Pty> },
    /// An application, started detached from the stream. The channel
    /// reports it started (`Opened` with the process id) and ends at once;
    /// the windows it makes show up in the session's window list.
    ///
    /// With `remote_app` the client asks for it as a RemoteApp of the
    /// host's Remote Desktop (docs/BACKENDS.md, "RemoteApp"): a host that
    /// serves it answers `RemoteApp` with the login, and the client runs
    /// the program there itself; a host that does not starts it as usual.
    Launch { argv: Vec<String>, remote_app: bool },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CommandMessage {
    /// Client: open a channel.
    Open { id: ChannelId, kind: ChannelKind },
    /// Host: the channel is open (for a launch: the application started,
    /// with its process id when the system gives one).
    Opened { id: ChannelId, pid: Option<u32> },
    /// Host, for a launch that asked for a RemoteApp: log in to the host's
    /// Remote Desktop at `target` and run the program there. An empty
    /// password means the user's own: the password the client signed in
    /// with when `sign_in_password` is set (the host's Windows account),
    /// else one the user is asked for. The channel then ends.
    RemoteApp {
        id: ChannelId,
        target: crate::HandoffTarget,
        sign_in_password: bool,
    },
    /// Host: no channel was opened. The reason is for the user.
    Refused { id: ChannelId, reason: String },
    /// Either way: bytes of the channel's stream.
    Data { id: ChannelId, bytes: Vec<u8> },
    /// Client: the window showing a pseudo-terminal changed size.
    Resize { id: ChannelId, size: TerminalSize },
    /// Client: no more input (the command sees its input end).
    Eof { id: ChannelId },
    /// Client: close the channel, ending its command.
    Close { id: ChannelId },
    /// Host: the command ended, with its exit code when the system gave
    /// one. The channel is closed.
    Exited { id: ChannelId, code: Option<i32> },
}

/// The largest chunk of bytes one `Data` carries.
pub const MAX_CHUNK: usize = 16 * 1024;

/// Channels one session may have open at once.
pub const MAX_CHANNELS: usize = 8;

/// Cuts `bytes` into `Data` messages of at most [`MAX_CHUNK`].
pub fn data_messages(id: ChannelId, bytes: &[u8]) -> impl Iterator<Item = CommandMessage> + '_ {
    bytes
        .chunks(MAX_CHUNK)
        .map(move |chunk| CommandMessage::Data {
            id,
            bytes: chunk.to_vec(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, encode, ControlMessage};

    #[test]
    fn command_messages_round_trip_on_the_control_channel() {
        let id = ChannelId(3);
        let pty = Pty {
            size: TerminalSize { cols: 80, rows: 24 },
            term: "xterm-256color".into(),
        };
        for message in [
            CommandMessage::Open {
                id,
                kind: ChannelKind::Shell(pty.clone()),
            },
            CommandMessage::Open {
                id,
                kind: ChannelKind::Exec {
                    argv: vec!["ls".into(), "-l".into()],
                    pty: None,
                },
            },
            CommandMessage::Open {
                id,
                kind: ChannelKind::Launch {
                    argv: vec!["firefox".into()],
                    remote_app: true,
                },
            },
            CommandMessage::RemoteApp {
                id,
                target: crate::HandoffTarget {
                    address: String::new(),
                    port: 3389,
                    username: "mech".into(),
                    password: String::new(),
                    certificate_sha256: Some([7; 32]),
                },
                sign_in_password: true,
            },
            CommandMessage::Data {
                id,
                bytes: b"ls\r\x1b[31mred\x1b[0m".to_vec(),
            },
            CommandMessage::Exited { id, code: Some(0) },
        ] {
            let control = ControlMessage::Command(message);
            assert_eq!(decode(&encode(&control).unwrap()).unwrap(), control);
        }
    }

    #[test]
    fn sizes_are_clamped_to_what_a_pty_can_be() {
        let size = TerminalSize {
            cols: 0,
            rows: 60000,
        }
        .clamped();
        assert_eq!(
            size,
            TerminalSize {
                cols: 1,
                rows: TerminalSize::MAX
            }
        );
    }

    #[test]
    fn large_writes_are_cut_into_chunks() {
        let bytes = vec![7u8; MAX_CHUNK * 2 + 5];
        let sizes: Vec<usize> = data_messages(ChannelId(1), &bytes)
            .map(|m| match m {
                CommandMessage::Data { bytes, .. } => bytes.len(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(sizes, [MAX_CHUNK, MAX_CHUNK, 5]);
    }
}

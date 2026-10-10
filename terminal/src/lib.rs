//! The client end of the command stream (docs/COMMAND-STREAM.md) that does
//! not depend on the transport:
//!
//! * [`channel`]: the [`Channel`] a shell or command is written and read
//!   through, whatever carries it;
//! * [`screen`]: the [`Terminal`], a channel plus the screen it draws, and
//!   the keys a viewer sends;
//! * [`ssh`]: an SSH client that opens the same kinds of channel on any
//!   SSH server, with its host keys pinned ([`hostkeys`]).

pub mod channel;
pub mod hostkeys;
pub mod screen;
pub mod ssh;

pub use channel::{Channel, ChannelEnd, ChannelEvent, Command};
pub use hostkeys::{HostKeyPolicy, HostKeyStore};
pub use screen::{Key, Snapshot, Terminal};
pub use ssh::{connect, SshAuth, SshConnection, SshError, SshTarget};

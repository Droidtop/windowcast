//! `Direct`: windowcast's own low-latency datagram transport, built from the
//! public design of Parsec's BUD between our own peers, with windowcast's
//! identity and standard crypto and none of Parsec's keys, servers or accounts
//! (Droidtop/tracker#441, docs/research/parsec.md).
//!
//! This crate is the packet layer only, with no sockets and no clock:
//!
//! - [`header`]: the 24-byte header on every datagram.
//! - [`replay`]: the receiver's replay window.
//! - [`packet`]: encrypting and opening datagrams, with the header as
//!   associated data and the packet number as the nonce.
//! - [`handshake`]: Noise `IK` between two pinned identities, giving a
//!   [`Session`] with a [`Sealer`] and an [`Opener`].
//!
//! Fragmenting, loss recovery, reliable channels, congestion control, pacing
//! and presentation sit above it and are not here yet. Neither is rekeying: a
//! [`Sealer`] stops after 2^64 - 1 packets and a new handshake starts a fresh
//! session.

pub mod handshake;
pub mod header;
pub mod packet;
pub mod replay;

pub use handshake::{accept, Accepted, HandshakeError, Initiator, Session};
pub use header::{Header, HeaderError, HEADER_LEN, MAX_DATAGRAM, MAX_PAYLOAD, TAG_LEN};
pub use packet::{OpenError, Opener, SealError, Sealer};
pub use replay::ReplayWindow;

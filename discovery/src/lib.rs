//! Finding a windowcast host on the local network, and waking one that sleeps
//! (Droidtop/tracker#113).
//!
//! **Local discovery.** A host announces itself with a small UDP broadcast
//! beacon every few seconds ([`Beacon`], [`announce`]); a client listens for
//! a moment and lists what it heard ([`Listener`]). A beacon says where to
//! connect and which device id to expect; it is **never trusted**. A client
//! still connects as it always does, resumes only with a host whose pinned
//! identity matches, and pairs with a PIN the first time: a forged beacon
//! can at worst send a client to the wrong address, where the identity check
//! fails. Everything in a beacon is length-checked before it is used, and
//! nothing is sent in reply to one.
//!
//! **Wake-on-LAN.** [`wake::magic_packet`] and [`wake::send`] send the
//! standard magic packet (six 0xFF bytes, then the MAC sixteen times) to a
//! sleeping computer on the same network.
//!
//! The beacon is IPv4 UDP broadcast on [`PORT`]: it needs no multicast
//! group, no mDNS stack and no permission beyond the network, and it works
//! on the networks windowcast is for (a home LAN). It is windowcast's own,
//! not mDNS: a mDNS responder is not needed to find another windowcast.

pub mod beacon;
pub mod macs;
pub mod wake;

pub use beacon::{announce, Beacon, BeaconError, Listener, Seen, PORT};

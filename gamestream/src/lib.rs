//! GameStream, windowcast's own implementation of the protocol Sunshine,
//! Apollo and Moonlight speak (Droidtop/tracker#110): a client for
//! Sunshine and Apollo hosts, and a host face that stock Moonlight can
//! pair with. Written from the reference sources (moonlight-qt,
//! moonlight-common-c, Sunshine), not guessed at.
//!
//! - [`crypto`]: the PIN-derived AES key, certificates and signatures.
//! - [`pairing`]: pairing, client and host ends, as a protocol over any
//!   request carrier.
//! - [`client`]: `serverinfo`, pairing, the app list, launch and quit over
//!   HTTP and HTTPS with the host's certificate pinned.
//! - [`server`]: a host's HTTP and HTTPS endpoints for Moonlight.
//! - [`rtsp`], [`video`], [`control`]: the stream setup, the video packets
//!   and the encrypted ENet control stream, both ends.
//! - [`input`]: the input packets a client sends on the control stream.
//! - [`stream`]: a host's launched stream; [`windows`]: a windowcast host's
//!   windows as the apps it offers.
//!
//! Not yet: sound in either direction.

pub mod client;
pub mod control;
pub mod crypto;
pub mod input;
pub mod pairing;
pub mod rtsp;
pub mod server;
pub mod session;
pub mod stream;
pub mod video;
pub mod windows;
pub mod xml;

#[derive(Debug, thiserror::Error)]
pub enum GameStreamError {
    #[error("http: {0}")]
    Http(String),
    #[error("tls: {0}")]
    Tls(String),
    #[error("certificate: {0}")]
    Certificate(String),
    #[error("the host said {0}: {1}")]
    Status(i64, String),
    #[error("pairing failed: {0}")]
    Pairing(&'static str),
    #[error("the PIN was wrong")]
    WrongPin,
    #[error("not paired with this host")]
    NotPaired,
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("rtsp: {0}")]
    Rtsp(&'static str),
}

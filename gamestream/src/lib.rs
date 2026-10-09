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
//!
//! Streaming itself (RTSP, the video, audio and control streams) is next.

pub mod client;
pub mod crypto;
pub mod pairing;
pub mod server;
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
}

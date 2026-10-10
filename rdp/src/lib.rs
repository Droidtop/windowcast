//! RDP for windowcast (Droidtop/tracker#111), on IronRDP's protocol crates
//! (see NOTICE): IronRDP speaks RDP (the connection sequence, TLS,
//! CredSSP/NLA, the bitmap codecs, the channels); this crate decides what
//! an RDP connection shows and where its input goes.
//!
//! - [`host`]: one window of a windowcast host served over RDP, so any RDP
//!   client (mstsc, FreeRDP, ours) shows that window and types into it.
//! - [`client`]: a client for RDP hosts (ours, Windows' own Remote Desktop,
//!   any other), its picture as RGBA and its input from windowcast's.
//! - [`tls`]: the host's TLS identity, pinned by fingerprint.

pub mod client;
#[cfg(feature = "host")]
pub mod host;
pub mod tls;

#[cfg(feature = "host")]
pub use ironrdp_server::Credentials;

#[derive(Debug, thiserror::Error)]
pub enum RdpError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tls: {0}")]
    Tls(String),
    #[error("connection: {0}")]
    Connect(String),
    #[error("session: {0}")]
    Session(String),
    #[error("certificate: {0}")]
    Certificate(String),
}

/// rustls needs one process-wide crypto provider when more than one is
/// built in (IronRDP's credential code brings aws-lc-rs, windowcast uses
/// ring); this picks ring, once.
pub(crate) fn crypto_provider() -> std::sync::Arc<rustls::crypto::CryptoProvider> {
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let _ = rustls::crypto::CryptoProvider::install_default((*provider).clone());
    provider
}

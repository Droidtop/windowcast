//! WebRTC session transport: one `PeerConnection` per client<->host
//! session, multiplexing every open window as a separate video track
//! within it, plus one data channel carrying `windowcast_protocol`
//! control/input messages. See the pairing crate for how the DTLS
//! fingerprint this module negotiates gets authenticated before either
//! side trusts the connection.
//!
//! This crate is intentionally still a thin skeleton: it stands up the
//! `PeerConnection`/data-channel plumbing and the fingerprint-extraction
//! point the pairing handshake needs, but real SDP offer/answer exchange
//! over an actual signaling transport, and per-window video track
//! attach/detach, are the next slice of work — not yet wired end-to-end.

use std::sync::Arc;

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::RTCPeerConnection;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("webrtc error: {0}")]
    WebRtc(#[from] webrtc::Error),
    #[error("no local DTLS certificate available yet — call after the peer connection is created")]
    NoLocalCertificate,
    #[error("protocol encode error: {0}")]
    Protocol(#[from] windowcast_protocol::ProtocolError),
}

/// Wraps one client<->host WebRTC session. Windows are added/removed as
/// tracks on this same connection after it's established, so opening a
/// new window never repeats the DTLS/ECDHE handshake — see the security
/// model in the project plan for why that matters for "each window is its
/// own channel" without paying a full handshake per window.
pub struct Session {
    pub peer_connection: Arc<RTCPeerConnection>,
    pub control_channel: Arc<RTCDataChannel>,
}

impl Session {
    /// Builds a fresh `RTCPeerConnection` plus the always-present control
    /// data channel. Uses no STUN/TURN servers by default — LAN-only
    /// sessions (droidtop's primary use case today) don't need ICE
    /// traversal beyond host candidates; WAN use across NATs will need
    /// `ice_servers` populated by the caller before this ships more
    /// broadly, tracked as a real follow-up, not silently assumed to work.
    pub async fn new() -> Result<Self, TransportError> {
        let mut media_engine = MediaEngine::default();
        media_engine.register_default_codecs()?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)?;

        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer::default()],
            ..Default::default()
        };
        let peer_connection = Arc::new(api.new_peer_connection(config).await?);

        let control_channel = peer_connection
            .create_data_channel("windowcast-control", None)
            .await?;

        Ok(Session {
            peer_connection,
            control_channel,
        })
    }

    /// The local DTLS certificate's fingerprint, in the form pairing's
    /// `authenticate_fingerprint`/`verify_fingerprint` expect: the exact
    /// bytes that must be authenticated by the PAKE-derived session key
    /// before this connection is trusted, per the security model.
    pub fn local_dtls_fingerprint(&self) -> Result<Vec<u8>, TransportError> {
        let params = self
            .peer_connection
            .sctp()
            .transport()
            .get_local_parameters()?;
        let fingerprint = params
            .fingerprints
            .first()
            .ok_or(TransportError::NoLocalCertificate)?;
        // "<algorithm> <hex-value>" — the algorithm is part of what must be
        // authenticated too (a downgrade to a weaker hash algorithm is
        // exactly the kind of substitution pairing's fingerprint check
        // exists to catch), so it's included in the authenticated bytes,
        // not just the raw hex value.
        Ok(format!("{} {}", fingerprint.algorithm, fingerprint.value).into_bytes())
    }

    pub async fn send_control(
        &self,
        message: &windowcast_protocol::ControlMessage,
    ) -> Result<(), TransportError> {
        let bytes = windowcast_protocol::encode(message)?;
        self.control_channel
            .send(&bytes::Bytes::from(bytes))
            .await?;
        Ok(())
    }
}

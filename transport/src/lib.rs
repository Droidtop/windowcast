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
use webrtc::ice_transport::ice_credential_type::RTCIceCredentialType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::RTCPeerConnection;

/// Credentials for a TURN relay a directory operator can offer as a
/// fallback when direct P2P ICE fails (symmetric NAT, restrictive
/// firewalls). Deliberately a plain relay, not a terminating proxy: TURN
/// forwards opaque encrypted WebRTC/DTLS-SRTP traffic without being able
/// to decrypt it, so a directory offering this never sees window content
/// — see docs/SECURITY.md's authorization/proxy-visibility notes in the
/// project plan for why that distinction matters and was a deliberate
/// choice, not an oversight.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// e.g. `["turn:relay.example.com:3478"]` — STUN URLs may also be
    /// included alongside TURN ones; ICE tries all of them.
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

impl RelayConfig {
    fn to_ice_server(&self) -> RTCIceServer {
        RTCIceServer {
            urls: self.urls.clone(),
            username: self.username.clone(),
            credential: self.credential.clone(),
            credential_type: RTCIceCredentialType::Password,
        }
    }
}

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
    /// data channel, using no STUN/TURN servers — LAN-only sessions
    /// (droidtop's primary use case today) don't need ICE traversal beyond
    /// host candidates. For a session that might cross a NAT/firewall,
    /// use [`Session::with_relay`] instead.
    pub async fn new() -> Result<Self, TransportError> {
        Self::build(vec![RTCIceServer::default()]).await
    }

    /// Same as [`Session::new`], but with a TURN relay available as an ICE
    /// candidate for when direct P2P connectivity fails. This does NOT
    /// force traffic through the relay — ICE still prefers a direct path
    /// when one works, falling back to relaying opaque encrypted traffic
    /// only when it doesn't. See [`RelayConfig`] for why this stays a
    /// blind relay rather than a terminating proxy.
    pub async fn with_relay(relay: &RelayConfig) -> Result<Self, TransportError> {
        Self::build(vec![RTCIceServer::default(), relay.to_ice_server()]).await
    }

    async fn build(ice_servers: Vec<RTCIceServer>) -> Result<Self, TransportError> {
        let mut media_engine = MediaEngine::default();
        media_engine.register_default_codecs()?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)?;

        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        let config = RTCConfiguration {
            ice_servers,
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

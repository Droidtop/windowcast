//! WebRTC session transport: one `PeerConnection` per client<->host
//! session, multiplexing every open window as a separate video track
//! within it, plus one data channel carrying `windowcast_protocol`
//! control/input messages.
//!
//! How a session comes up: [`signaling::connect`] (client) and
//! [`signaling::accept`] (host) exchange one offer and one answer over any
//! byte stream (a LAN TCP socket today), each description signed by the
//! sender's persistent identity and, while pairing, tagged with the
//! PIN-derived key. See `signaling` and docs/SECURITY.md.
//!
//! After that, everything rides the session itself. The control channel is
//! a pre-negotiated data channel (id 0) both sides create up front, so it
//! needs no in-band announcement. Attaching or detaching a window track
//! renegotiates over that control channel, which is already inside the
//! authenticated DTLS session; the host is the only side that renegotiates,
//! so offers never collide.

mod media;
pub mod signaling;

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch, Mutex};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_credential_type::RTCIceCredentialType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use windowcast_protocol::{ControlMessage, SdpKind, WindowId};

pub use media::{RemoteWindow, WindowFrame, WindowTrack};
pub use signaling::{accept, connect, ClientCredential, Established, HostCredential};

/// Label of the control data channel. Pre-negotiated with this id on both
/// sides (see module docs).
const CONTROL_CHANNEL_LABEL: &str = "windowcast-control";
const CONTROL_CHANNEL_ID: u16 = 0;

/// How long either side waits for the control channel to open after the
/// answer is applied, and for the client's answer to a renegotiation. ICE
/// on a LAN completes in well under a second; this only bounds a peer that
/// went away.
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(15);

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
    #[error("signaling i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("signaling frame of {0} bytes exceeds the limit")]
    FrameTooLarge(usize),
    #[error("unexpected signaling message: expected {0}")]
    Unexpected(&'static str),
    #[error("the peer refused the session: {0}")]
    Rejected(String),
    /// A description's signature or PIN tag did not verify: a wrong PIN, a
    /// tampered description, or someone in the middle.
    #[error("the peer's session description failed authentication")]
    AuthenticationFailed,
    #[error("the peer's identity is not pinned; pair with it first")]
    UnknownPeer,
    #[error("this host is not accepting new pairings")]
    PairingNotOpen,
    #[error("pairing key exchange failed: {0}")]
    Pairing(#[from] windowcast_pairing::PairingError),
    #[error("timed out")]
    Timeout,
    #[error("the session is closed")]
    Closed,
}

/// Wraps one client<->host WebRTC session. Windows are added/removed as
/// tracks on this same connection after it's established, so opening a
/// new window never repeats the DTLS/ECDHE handshake.
pub struct Session {
    pub peer_connection: Arc<RTCPeerConnection>,
    pub control_channel: Arc<RTCDataChannel>,
    control_open: watch::Receiver<bool>,
    closed: watch::Receiver<bool>,
    inbound: Mutex<mpsc::UnboundedReceiver<ControlMessage>>,
    remote_windows: Mutex<mpsc::UnboundedReceiver<RemoteWindow>>,
    pending_answer: PendingAnswer,
    /// Serializes this side's own renegotiations: one offer in flight.
    negotiation: Mutex<()>,
    /// Host side: the window tracks this session is sending.
    windows: Mutex<HashMap<WindowId, WindowTrack>>,
}

type PendingAnswer = Arc<std::sync::Mutex<Option<oneshot::Sender<String>>>>;

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
            .create_data_channel(
                CONTROL_CHANNEL_LABEL,
                Some(RTCDataChannelInit {
                    ordered: Some(true),
                    negotiated: Some(CONTROL_CHANNEL_ID),
                    ..Default::default()
                }),
            )
            .await?;

        let (open_tx, control_open) = watch::channel(false);
        control_channel.on_open(Box::new(move || {
            let _ = open_tx.send(true);
            Box::pin(async {})
        }));

        // Either the control channel closing (the peer closed the session)
        // or the connection failing for good ends the session for the
        // embedder: pending and later receives return `Closed`.
        let (closed_tx, closed) = watch::channel(false);
        let closed_tx = Arc::new(closed_tx);
        let on_channel_close = Arc::clone(&closed_tx);
        control_channel.on_close(Box::new(move || {
            let _ = on_channel_close.send(true);
            Box::pin(async {})
        }));
        peer_connection.on_peer_connection_state_change(Box::new(move |state| {
            if matches!(
                state,
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
            ) {
                let _ = closed_tx.send(true);
            }
            Box::pin(async {})
        }));

        let (raw_tx, raw_rx) = mpsc::unbounded_channel::<bytes::Bytes>();
        control_channel.on_message(Box::new(move |message| {
            let _ = raw_tx.send(message.data);
            Box::pin(async {})
        }));

        let (inbound_tx, inbound) = mpsc::unbounded_channel();
        let pending_answer: PendingAnswer = Arc::new(std::sync::Mutex::new(None));
        tokio::spawn(dispatch_control(
            raw_rx,
            inbound_tx,
            Arc::downgrade(&peer_connection),
            Arc::downgrade(&control_channel),
            Arc::clone(&pending_answer),
        ));

        let (windows_tx, remote_windows) = mpsc::unbounded_channel();
        let weak_pc = Arc::downgrade(&peer_connection);
        peer_connection.on_track(Box::new(move |track, _receiver, _transceiver| {
            if let Some(window) = media::window_for_track_id(&track.id()) {
                let _ = windows_tx.send(RemoteWindow::new(window, track, weak_pc.clone()));
            }
            Box::pin(async {})
        }));

        Ok(Session {
            peer_connection,
            control_channel,
            control_open,
            closed,
            inbound: Mutex::new(inbound),
            remote_windows: Mutex::new(remote_windows),
            pending_answer,
            negotiation: Mutex::new(()),
            windows: Mutex::new(HashMap::new()),
        })
    }

    /// The local DTLS certificate's fingerprint as "<algorithm> <hex>".
    /// Signaling authenticates the whole session description, which carries
    /// this fingerprint, so connecting does not need it; it stays for
    /// display (e.g. letting a user compare it out of band).
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
        Ok(format!("{} {}", fingerprint.algorithm, fingerprint.value).into_bytes())
    }

    pub async fn send_control(&self, message: &ControlMessage) -> Result<(), TransportError> {
        let bytes = windowcast_protocol::encode(message)?;
        self.control_channel
            .send(&bytes::Bytes::from(bytes))
            .await?;
        Ok(())
    }

    /// The next control message from the peer. Renegotiation traffic is
    /// handled internally and never shows up here.
    pub async fn recv_control(&self) -> Result<ControlMessage, TransportError> {
        let mut inbound = self.inbound.lock().await;
        self.until_closed(inbound.recv()).await
    }

    /// Client side: the next window track the host attached.
    pub async fn next_remote_window(&self) -> Result<RemoteWindow, TransportError> {
        let mut remote_windows = self.remote_windows.lock().await;
        self.until_closed(remote_windows.recv()).await
    }

    /// Waits for `next`, or fails with `Closed` once the session has ended.
    /// Items that arrived before the end are still delivered first.
    async fn until_closed<T>(
        &self,
        next: impl std::future::Future<Output = Option<T>>,
    ) -> Result<T, TransportError> {
        let mut closed = self.closed.clone();
        tokio::select! {
            biased;
            item = next => item.ok_or(TransportError::Closed),
            _ = closed.wait_for(|closed| *closed) => Err(TransportError::Closed),
        }
    }

    /// Host side: starts sending `window` as its own video track on this
    /// session and renegotiates. Attaching a window that is already
    /// attached returns its existing track.
    pub async fn attach_window(&self, window: WindowId) -> Result<WindowTrack, TransportError> {
        let mut windows = self.windows.lock().await;
        if let Some(existing) = windows.get(&window) {
            return Ok(existing.clone());
        }
        let track = WindowTrack::add_to(&self.peer_connection, window).await?;
        self.renegotiate().await?;
        windows.insert(window, track.clone());
        Ok(track)
    }

    /// Host side: stops sending `window` and renegotiates. Returns whether
    /// the window was attached.
    pub async fn detach_window(&self, window: WindowId) -> Result<bool, TransportError> {
        let mut windows = self.windows.lock().await;
        let Some(track) = windows.remove(&window) else {
            return Ok(false);
        };
        self.peer_connection.remove_track(track.sender()).await?;
        self.renegotiate().await?;
        Ok(true)
    }

    pub async fn close(&self) -> Result<(), TransportError> {
        self.peer_connection.close().await?;
        Ok(())
    }

    async fn wait_control_open(&self) -> Result<(), TransportError> {
        let mut open = self.control_open.clone();
        tokio::time::timeout(NEGOTIATION_TIMEOUT, open.wait_for(|open| *open))
            .await
            .map_err(|_| TransportError::Timeout)?
            .map_err(|_| TransportError::Closed)?;
        Ok(())
    }

    /// Sends a fresh offer over the control channel and applies the
    /// client's answer.
    async fn renegotiate(&self) -> Result<(), TransportError> {
        let _one_at_a_time = self.negotiation.lock().await;
        let (answer_tx, answer_rx) = oneshot::channel();
        *self.pending_answer.lock().expect("pending answer lock") = Some(answer_tx);

        let offer = self.peer_connection.create_offer(None).await?;
        self.peer_connection
            .set_local_description(offer.clone())
            .await?;
        self.send_control(&ControlMessage::SessionDescription {
            kind: SdpKind::Offer,
            sdp: offer.sdp,
        })
        .await?;

        let answer = tokio::time::timeout(NEGOTIATION_TIMEOUT, answer_rx)
            .await
            .map_err(|_| TransportError::Timeout)?
            .map_err(|_| TransportError::Closed)?;
        self.peer_connection
            .set_remote_description(RTCSessionDescription::answer(answer)?)
            .await?;
        Ok(())
    }
}

/// Reads raw control-channel messages: answers renegotiation offers,
/// hands renegotiation answers to the waiting [`Session::renegotiate`],
/// and forwards everything else to [`Session::recv_control`]. Holds only
/// weak references, so it ends with the session instead of keeping it alive.
async fn dispatch_control(
    mut raw: mpsc::UnboundedReceiver<bytes::Bytes>,
    inbound: mpsc::UnboundedSender<ControlMessage>,
    peer_connection: Weak<RTCPeerConnection>,
    control_channel: Weak<RTCDataChannel>,
    pending_answer: PendingAnswer,
) {
    while let Some(data) = raw.recv().await {
        let message = match windowcast_protocol::decode(&data) {
            Ok(message) => message,
            Err(e) => {
                tracing::warn!("dropping undecodable control message: {e}");
                continue;
            }
        };
        match message {
            ControlMessage::SessionDescription {
                kind: SdpKind::Offer,
                sdp,
            } => {
                let (Some(pc), Some(dc)) = (peer_connection.upgrade(), control_channel.upgrade())
                else {
                    break;
                };
                if let Err(e) = answer_renegotiation(&pc, &dc, sdp).await {
                    tracing::warn!("renegotiation answer failed: {e}");
                }
            }
            ControlMessage::SessionDescription {
                kind: SdpKind::Answer,
                sdp,
            } => {
                let waiting = pending_answer.lock().expect("pending answer lock").take();
                match waiting {
                    Some(tx) => {
                        let _ = tx.send(sdp);
                    }
                    None => tracing::warn!("dropping an answer nobody asked for"),
                }
            }
            other => {
                if inbound.send(other).is_err() {
                    break;
                }
            }
        }
    }
}

async fn answer_renegotiation(
    pc: &RTCPeerConnection,
    dc: &RTCDataChannel,
    offer: String,
) -> Result<(), TransportError> {
    pc.set_remote_description(RTCSessionDescription::offer(offer)?)
        .await?;
    let answer = pc.create_answer(None).await?;
    pc.set_local_description(answer.clone()).await?;
    let bytes = windowcast_protocol::encode(&ControlMessage::SessionDescription {
        kind: SdpKind::Answer,
        sdp: answer.sdp,
    })?;
    dc.send(&bytes::Bytes::from(bytes)).await?;
    Ok(())
}

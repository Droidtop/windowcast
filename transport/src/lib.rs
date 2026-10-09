//! WebRTC session transport: one `PeerConnection` per client<->host
//! session, multiplexing every open window as a separate video track
//! within it, plus one data channel carrying `windowcast_protocol`
//! control/input messages. This is the native backend and the control
//! plane every other backend is negotiated over (docs/BACKENDS.md).
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
//!
//! Sockets: every session binds UDP on all interfaces and on 127.0.0.1, so
//! it also gets a loopback candidate. Two peers on one device (droidtop and
//! its own desktop container) connect over loopback even with no network
//! up; across machines the loopback pair simply never succeeds.

mod audio;
mod keyframes;
mod media;
pub mod signaling;

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use bytes::BytesMut;
use rtc::ice::mdns::MulticastDnsMode;
use rtc::peer_connection::configuration::setting_engine::SettingEngineBuilder;
use rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelInit};
use webrtc::media_stream::track_remote::TrackRemote;
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCConfigurationBuilder, RTCIceGatheringState, RTCIceServer, RTCPeerConnectionState,
    RTCSessionDescription,
};
use windowcast_protocol::{ControlMessage, SdpKind, StreamTarget, VideoCodec, WindowId};

pub use audio::{AudioPacket, AudioTrack, RemoteAudio};
pub use media::{is_keyframe, RemoteWindow, WindowFrame, WindowTrack};
pub use signaling::{accept, connect, ClientCredential, Established, HostCredential};

/// Label of the control data channel. Pre-negotiated with this id on both
/// sides (see module docs).
const CONTROL_CHANNEL_LABEL: &str = "windowcast-control";
const CONTROL_CHANNEL_ID: u16 = 0;

/// UDP sockets each session binds: every interface (expanded per address
/// by webrtc) plus loopback, for same-device sessions.
const UDP_BIND_ADDRS: [&str; 2] = ["0.0.0.0:0", "127.0.0.1:0"];

/// The one socket a [`Session::local_only`] binds.
const LOCAL_UDP_BIND_ADDRS: [&str; 1] = ["127.0.0.1:0"];

/// How long either side waits for ICE gathering, for the control channel
/// to open after the answer is applied, and for the client's answer to a
/// renegotiation. On a LAN all of these take well under a second; this
/// only bounds a peer that went away.
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(15);

/// Credentials for a TURN relay a directory operator can offer as a
/// fallback when direct P2P ICE fails (symmetric NAT, restrictive
/// firewalls). Deliberately a plain relay, not a terminating proxy: TURN
/// forwards opaque encrypted WebRTC/DTLS-SRTP traffic without being able
/// to decrypt it, so a directory offering this never sees window content
/// (docs/SECURITY.md, "Directory-mediated sessions").
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
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("webrtc error: {0}")]
    WebRtc(#[from] webrtc::error::Error),
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
    #[error("the negotiated session has no {0:?} codec")]
    CodecNotNegotiated(VideoCodec),
    #[error("the negotiated session has no Opus")]
    AudioNotNegotiated,
    #[error("timed out")]
    Timeout,
    #[error("the session is closed")]
    Closed,
}

/// Peer-connection events the session cares about, turned into channels.
struct Events {
    gathered: watch::Sender<bool>,
    closed: watch::Sender<bool>,
    remote_tracks: mpsc::UnboundedSender<Arc<dyn TrackRemote>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Events {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.send(true);
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if matches!(
            state,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
        ) {
            let _ = self.closed.send(true);
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let _ = self.remote_tracks.send(track);
    }
}

/// Wraps one client<->host WebRTC session. Windows are added/removed as
/// tracks on this same connection after it's established, so opening a
/// new window never repeats the DTLS/ECDHE handshake.
pub struct Session {
    peer_connection: Arc<dyn PeerConnection>,
    control_channel: Arc<dyn DataChannel>,
    gathered: watch::Receiver<bool>,
    control_open: watch::Receiver<bool>,
    closed: watch::Receiver<bool>,
    inbound: Mutex<mpsc::UnboundedReceiver<ControlMessage>>,
    remote_tracks: Mutex<mpsc::UnboundedReceiver<Arc<dyn TrackRemote>>>,
    pending_answer: PendingAnswer,
    ended_windows: EndedWindows,
    /// Serializes this side's own renegotiations: one offer in flight.
    negotiation: Mutex<()>,
    /// Host side: the window tracks this session is sending.
    windows: Mutex<HashMap<WindowId, WindowTrack>>,
    /// Host side: the windows' audio tracks.
    audio: Mutex<HashMap<WindowId, AudioTrack>>,
}

/// A track the host attached: a window's video or its audio.
pub enum RemoteTrack {
    Window(RemoteWindow),
    Audio(RemoteAudio),
}

type PendingAnswer = Arc<std::sync::Mutex<Option<oneshot::Sender<String>>>>;

/// Client side: flags per received window (its video and its audio), set
/// when the host says the window stopped. webrtc 0.21 never reports a
/// remote track ending (its track close events are declared but not
/// emitted), so the end of a window's tracks is signalled by the host's
/// `StreamStopped` instead.
type EndedWindows = Arc<std::sync::Mutex<HashMap<WindowId, Vec<watch::Sender<bool>>>>>;

impl Session {
    /// Builds a fresh peer connection plus the always-present control data
    /// channel, using no STUN/TURN servers — LAN-only and same-device
    /// sessions (droidtop's primary use case today) need nothing beyond
    /// host candidates. For a session that might cross a NAT/firewall, use
    /// [`Session::with_relay`] instead.
    pub async fn new() -> Result<Self, TransportError> {
        Self::build(vec![], &UDP_BIND_ADDRS).await
    }

    /// A session for two peers on this device only: it binds loopback and
    /// nothing else, so no other machine can reach it and no network port
    /// is opened. Hosts listening on a loopback address, and clients
    /// connecting to one, use this.
    pub async fn local_only() -> Result<Self, TransportError> {
        Self::build(vec![], &LOCAL_UDP_BIND_ADDRS).await
    }

    /// Same as [`Session::new`], but with a TURN relay available as an ICE
    /// candidate for when direct P2P connectivity fails. This does NOT
    /// force traffic through the relay — ICE still prefers a direct path
    /// when one works, falling back to relaying opaque encrypted traffic
    /// only when it doesn't. See [`RelayConfig`] for why this stays a
    /// blind relay rather than a terminating proxy.
    pub async fn with_relay(relay: &RelayConfig) -> Result<Self, TransportError> {
        Self::build(vec![relay.to_ice_server()], &UDP_BIND_ADDRS).await
    }

    async fn build(
        ice_servers: Vec<RTCIceServer>,
        udp_addrs: &[&'static str],
    ) -> Result<Self, TransportError> {
        let mut media_engine = MediaEngine::default();
        media_engine.register_default_codecs()?;
        let registry = keyframes::interceptor_registry(&mut media_engine)?;

        let (gathered_tx, gathered) = watch::channel(false);
        let (closed_tx, closed) = watch::channel(false);
        let (tracks_tx, remote_tracks) = mpsc::unbounded_channel();
        let events = Arc::new(Events {
            gathered: gathered_tx,
            closed: closed_tx,
            remote_tracks: tracks_tx,
        });

        let peer_connection: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_configuration(
                    RTCConfigurationBuilder::new()
                        .with_ice_servers(ice_servers)
                        .build(),
                )
                .with_media_engine(media_engine)
                .with_setting_engine(
                    // No mDNS: windowcast peers send IP candidates, and the
                    // mDNS socket cannot be opened on a device whose only
                    // interface is loopback (webrtc then fails the whole
                    // connection), which is exactly the same-device case.
                    SettingEngineBuilder::new()
                        .with_multicast_dns_mode(MulticastDnsMode::Disabled)
                        .build(),
                )
                .with_interceptor_registry(registry)
                .with_handler(Arc::clone(&events) as Arc<dyn PeerConnectionEventHandler>)
                .with_udp_addrs(udp_addrs.to_vec())
                .build()
                .await?,
        );

        let control_channel = peer_connection
            .create_data_channel(
                CONTROL_CHANNEL_LABEL,
                Some(RTCDataChannelInit {
                    negotiated: Some(CONTROL_CHANNEL_ID),
                    ..Default::default()
                }),
            )
            .await?;

        let (open_tx, control_open) = watch::channel(false);
        let (inbound_tx, inbound) = mpsc::unbounded_channel();
        let pending_answer: PendingAnswer = Arc::new(std::sync::Mutex::new(None));
        let ended_windows: EndedWindows = Arc::default();
        tokio::spawn(run_control_channel(
            Arc::clone(&control_channel),
            Arc::downgrade(&peer_connection),
            open_tx,
            events,
            inbound_tx,
            Arc::clone(&pending_answer),
            Arc::clone(&ended_windows),
        ));

        Ok(Session {
            peer_connection,
            control_channel,
            gathered,
            control_open,
            closed,
            inbound: Mutex::new(inbound),
            remote_tracks: Mutex::new(remote_tracks),
            pending_answer,
            ended_windows,
            negotiation: Mutex::new(()),
            windows: Mutex::new(HashMap::new()),
            audio: Mutex::new(HashMap::new()),
        })
    }

    pub async fn send_control(&self, message: &ControlMessage) -> Result<(), TransportError> {
        send_on(&*self.control_channel, message).await
    }

    /// The next control message from the peer. Renegotiation traffic is
    /// handled internally and never shows up here.
    pub async fn recv_control(&self) -> Result<ControlMessage, TransportError> {
        let mut inbound = self.inbound.lock().await;
        self.until_closed(inbound.recv()).await
    }

    /// Client side: the next window track the host attached. Tracks whose
    /// id is not a window's are skipped.
    pub async fn next_remote_window(&self) -> Result<RemoteWindow, TransportError> {
        loop {
            if let RemoteTrack::Window(window) = self.next_remote_track().await? {
                return Ok(window);
            }
        }
    }

    /// Client side: the next track the host attached, a window's video or
    /// its audio. Tracks that are neither are skipped.
    pub async fn next_remote_track(&self) -> Result<RemoteTrack, TransportError> {
        let mut remote_tracks = self.remote_tracks.lock().await;
        loop {
            let track = self.until_closed(remote_tracks.recv()).await?;
            let (ended_tx, ended) = watch::channel(false);
            let remote =
                if let Some(audio) = RemoteAudio::new(Arc::clone(&track), ended.clone()).await {
                    RemoteTrack::Audio(audio)
                } else if let Some(window) = RemoteWindow::new(track, ended).await {
                    RemoteTrack::Window(window)
                } else {
                    continue;
                };
            let window = match &remote {
                RemoteTrack::Window(w) => w.window(),
                RemoteTrack::Audio(a) => a.window(),
            };
            self.ended_windows
                .lock()
                .expect("ended windows")
                .entry(window)
                .or_default()
                .push(ended_tx);
            return Ok(remote);
        }
    }

    /// Host side: starts sending `window`'s audio as an Opus track and
    /// renegotiates. Attaching twice returns the existing track.
    pub async fn attach_audio(&self, window: WindowId) -> Result<AudioTrack, TransportError> {
        let mut audio = self.audio.lock().await;
        if let Some(existing) = audio.get(&window) {
            return Ok(existing.clone());
        }
        let pending = audio::PendingAudio::add_to(&*self.peer_connection, window).await?;
        self.renegotiate().await?;
        let track = pending.negotiated().await?;
        audio.insert(window, track.clone());
        Ok(track)
    }

    /// Host side: stops sending `window`'s audio and renegotiates. The
    /// client's end of it finishes with the window's `StreamStopped`.
    pub async fn detach_audio(&self, window: WindowId) -> Result<bool, TransportError> {
        let mut audio = self.audio.lock().await;
        let Some(track) = audio.remove(&window) else {
            return Ok(false);
        };
        self.peer_connection.remove_track(track.sender()).await?;
        self.renegotiate().await?;
        Ok(true)
    }

    /// Host side: starts sending `window` as its own video track in
    /// `codec` and renegotiates. Attaching a window that is already
    /// attached returns its existing track.
    pub async fn attach_window(
        &self,
        window: WindowId,
        codec: VideoCodec,
    ) -> Result<WindowTrack, TransportError> {
        let mut windows = self.windows.lock().await;
        if let Some(existing) = windows.get(&window) {
            return Ok(existing.clone());
        }
        let pending = WindowTrack::add_to(&*self.peer_connection, window, codec).await?;
        self.renegotiate().await?;
        let track = pending.negotiated().await?;
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
        self.send_control(&ControlMessage::StreamStopped(StreamTarget::Window(window)))
            .await?;
        Ok(true)
    }

    /// Ends the session. The peer is told with a `Goodbye` on the control
    /// channel, flushed before the connection closes; otherwise it would
    /// only notice when ICE consent times out, half a minute later.
    pub async fn close(&self) -> Result<(), TransportError> {
        if self.send_control(&ControlMessage::Goodbye).await.is_ok() {
            let flushed = async {
                while self.control_channel.outstanding_bytes().await.unwrap_or(0) > 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            };
            let _ = tokio::time::timeout(Duration::from_secs(1), flushed).await;
        }
        self.peer_connection.close().await?;
        Ok(())
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

    async fn wait_until(flag: &watch::Receiver<bool>) -> Result<(), TransportError> {
        let mut flag = flag.clone();
        tokio::time::timeout(NEGOTIATION_TIMEOUT, flag.wait_for(|set| *set))
            .await
            .map_err(|_| TransportError::Timeout)?
            .map_err(|_| TransportError::Closed)?;
        Ok(())
    }

    async fn wait_control_open(&self) -> Result<(), TransportError> {
        Self::wait_until(&self.control_open).await
    }

    /// Creates the offer or answer and waits for ICE gathering, so the SDP
    /// that gets signed carries every candidate.
    ///
    /// The initial offer (the client's) carries a receive-only video
    /// section. webrtc fixes a session's video codecs at the first
    /// negotiation that has video, from that section's codec list; without
    /// this, the first window track would pin the session to its own codec
    /// and a later window in another codec could not be added. The host's
    /// first window track reuses this section.
    async fn gathered_local_description(&self, kind: SdpKind) -> Result<String, TransportError> {
        let pc = &self.peer_connection;
        let description = match kind {
            SdpKind::Offer => {
                pc.add_transceiver_from_kind(
                    RtpCodecKind::Video,
                    Some(RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Recvonly,
                        streams: vec![],
                        send_encodings: vec![],
                    }),
                )
                .await?;
                pc.create_offer(None).await?
            }
            SdpKind::Answer => pc.create_answer(None).await?,
        };
        pc.set_local_description(description).await?;
        Self::wait_until(&self.gathered).await?;
        pc.local_description()
            .await
            .map(|description| description.sdp)
            .ok_or(TransportError::Closed)
    }

    async fn set_remote_description(
        &self,
        kind: SdpKind,
        sdp: String,
    ) -> Result<(), TransportError> {
        let description = match kind {
            SdpKind::Offer => RTCSessionDescription::offer(sdp)?,
            SdpKind::Answer => RTCSessionDescription::answer(sdp)?,
        };
        self.peer_connection
            .set_remote_description(description)
            .await?;
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
        self.set_remote_description(SdpKind::Answer, answer).await
    }
}

async fn send_on(
    channel: &dyn DataChannel,
    message: &ControlMessage,
) -> Result<(), TransportError> {
    let bytes = windowcast_protocol::encode(message)?;
    channel.send(BytesMut::from(&bytes[..])).await?;
    Ok(())
}

/// Drives the control channel: reports it open, answers renegotiation
/// offers, hands renegotiation answers to the waiting
/// [`Session::renegotiate`], forwards everything else to
/// [`Session::recv_control`], and ends the session when the channel
/// closes. Holds the peer connection only weakly, so it ends with the
/// session instead of keeping it alive.
async fn run_control_channel(
    channel: Arc<dyn DataChannel>,
    peer_connection: Weak<dyn PeerConnection>,
    open: watch::Sender<bool>,
    events: Arc<Events>,
    inbound: mpsc::UnboundedSender<ControlMessage>,
    pending_answer: PendingAnswer,
    ended_windows: EndedWindows,
) {
    while let Some(event) = channel.poll().await {
        let data = match event {
            DataChannelEvent::OnOpen => {
                let _ = open.send(true);
                continue;
            }
            DataChannelEvent::OnClose => break,
            DataChannelEvent::OnMessage(message) => message.data,
            _ => continue,
        };
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
                let Some(pc) = peer_connection.upgrade() else {
                    break;
                };
                if let Err(e) = answer_renegotiation(&*pc, &*channel, sdp).await {
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
            ControlMessage::Goodbye => break,
            other => {
                if let ControlMessage::StreamStopped(StreamTarget::Window(window)) = &other {
                    let ended = ended_windows.lock().expect("ended windows").remove(window);
                    for flag in ended.into_iter().flatten() {
                        let _ = flag.send(true);
                    }
                }
                let _ = inbound.send(other);
            }
        }
    }
    let _ = events.closed.send(true);
}

async fn answer_renegotiation(
    pc: &dyn PeerConnection,
    channel: &dyn DataChannel,
    offer: String,
) -> Result<(), TransportError> {
    pc.set_remote_description(RTCSessionDescription::offer(offer)?)
        .await?;
    let answer = pc.create_answer(None).await?;
    pc.set_local_description(answer.clone()).await?;
    send_on(
        channel,
        &ControlMessage::SessionDescription {
            kind: SdpKind::Answer,
            sdp: answer.sdp,
        },
    )
    .await
}

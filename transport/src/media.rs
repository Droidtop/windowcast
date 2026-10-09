//! Per-window video tracks: one H.264 track per streamed window, on the
//! session's one `PeerConnection`.
//!
//! The host hands each [`WindowTrack`] encoded access units (Annex-B, as
//! every hardware encoder emits them); webrtc packetizes them into RTP. The
//! client's [`RemoteWindow`] reassembles RTP back into whole access units
//! at the marker bit, so a frame is delivered the moment its last packet
//! arrives, with no wait for the next frame. A frame with a sequence gap is
//! dropped, and so is everything after it until the next IDR, with a
//! picture-loss request sent to the host so the encoder produces one.

use std::sync::{Arc, Weak};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::sync::Notify;
use webrtc::api::media_engine::MIME_TYPE_H264;
use webrtc::media::Sample;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp::codecs::h264::H264Packet;
use webrtc::rtp::packetizer::Depacketizer;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_remote::TrackRemote;
use windowcast_protocol::WindowId;

use crate::TransportError;

/// Constrained Baseline 3.1, packetization mode 1: what every hardware
/// decoder takes, and one of the profiles webrtc's default codec table
/// registers.
const H264_FMTP: &str = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";
const H264_CLOCK_RATE: u32 = 90_000;
const STREAM_ID: &str = "windowcast";
const TRACK_ID_PREFIX: &str = "window-";

pub(crate) fn track_id_for(window: WindowId) -> String {
    format!("{TRACK_ID_PREFIX}{}", window.0)
}

pub(crate) fn window_for_track_id(id: &str) -> Option<WindowId> {
    id.strip_prefix(TRACK_ID_PREFIX)?.parse().ok().map(WindowId)
}

/// Host side: the sending end of one window's video track.
#[derive(Clone)]
pub struct WindowTrack {
    window: WindowId,
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<RTCRtpSender>,
    keyframe_requests: Arc<Notify>,
}

impl WindowTrack {
    pub(crate) async fn add_to(
        peer_connection: &RTCPeerConnection,
        window: WindowId,
    ) -> Result<Self, TransportError> {
        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: H264_CLOCK_RATE,
                channels: 0,
                sdp_fmtp_line: H264_FMTP.to_owned(),
                rtcp_feedback: vec![],
            },
            track_id_for(window),
            STREAM_ID.to_owned(),
        ));
        let sender = peer_connection
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;

        // RTCP has to be read for the interceptors (NACK, reports) to work
        // at all; picture-loss and full-intra requests are what the encoder
        // needs from it.
        let keyframe_requests = Arc::new(Notify::new());
        let rtcp_sender = Arc::clone(&sender);
        let notify = Arc::clone(&keyframe_requests);
        tokio::spawn(async move {
            while let Ok((packets, _)) = rtcp_sender.read_rtcp().await {
                let wants_keyframe = packets.iter().any(|p| {
                    let any = p.as_any();
                    any.is::<PictureLossIndication>() || any.is::<FullIntraRequest>()
                });
                if wants_keyframe {
                    notify.notify_one();
                }
            }
        });

        Ok(WindowTrack {
            window,
            track,
            sender,
            keyframe_requests,
        })
    }

    pub fn window(&self) -> WindowId {
        self.window
    }

    /// The track id the client sees, and what `StreamStartResponse::track_id`
    /// carries.
    pub fn track_id(&self) -> String {
        track_id_for(self.window)
    }

    pub(crate) fn sender(&self) -> &Arc<RTCRtpSender> {
        &self.sender
    }

    /// Sends one encoded access unit (Annex-B NAL units with start codes),
    /// shown for `duration`. The first frame after attaching, and the first
    /// after [`WindowTrack::keyframe_requested`] fires, must be an IDR with
    /// its SPS and PPS.
    pub async fn write_frame(&self, data: Bytes, duration: Duration) -> Result<(), TransportError> {
        self.track
            .write_sample(&Sample {
                data,
                duration,
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    /// Resolves when the client has asked for a keyframe (it lost a frame,
    /// or just started decoding). The encoder should emit an IDR next.
    pub async fn keyframe_requested(&self) {
        self.keyframe_requests.notified().await
    }
}

/// One encoded access unit received for a window, in Annex-B form, ready
/// for a hardware decoder (MediaCodec, VideoToolbox, Media Foundation).
#[derive(Debug, Clone)]
pub struct WindowFrame {
    pub data: Bytes,
    /// The RTP timestamp (90 kHz clock) the host sent this frame with.
    pub rtp_timestamp: u32,
}

/// Client side: the receiving end of one window's video track.
pub struct RemoteWindow {
    window: WindowId,
    track: Arc<TrackRemote>,
    peer_connection: Weak<RTCPeerConnection>,
    depacketizer: H264Packet,
    frame: BytesMut,
    last_sequence: Option<u16>,
    frame_damaged: bool,
    /// Set at start and after any loss: frames are dropped until an IDR.
    awaiting_keyframe: bool,
}

impl RemoteWindow {
    pub(crate) fn new(
        window: WindowId,
        track: Arc<TrackRemote>,
        peer_connection: Weak<RTCPeerConnection>,
    ) -> Self {
        RemoteWindow {
            window,
            track,
            peer_connection,
            depacketizer: H264Packet::default(),
            frame: BytesMut::new(),
            last_sequence: None,
            frame_damaged: false,
            awaiting_keyframe: true,
        }
    }

    pub fn window(&self) -> WindowId {
        self.window
    }

    pub fn track_id(&self) -> String {
        self.track.id()
    }

    /// The next complete, decodable frame. Fails once the track ends (the
    /// host detached the window or the session closed).
    pub async fn next_frame(&mut self) -> Result<WindowFrame, TransportError> {
        loop {
            let (packet, _) = self.track.read_rtp().await?;
            let sequence = packet.header.sequence_number;
            if let Some(last) = self.last_sequence {
                if sequence != last.wrapping_add(1) {
                    self.frame_damaged = true;
                }
            }
            self.last_sequence = Some(sequence);

            if !packet.payload.is_empty() {
                match self.depacketizer.depacketize(&packet.payload) {
                    Ok(nal_units) => self.frame.extend_from_slice(&nal_units),
                    Err(_) => self.frame_damaged = true,
                }
            }
            if !packet.header.marker {
                continue;
            }

            let data = self.frame.split().freeze();
            if std::mem::take(&mut self.frame_damaged) {
                self.awaiting_keyframe = true;
            }
            if data.is_empty() {
                continue;
            }
            if self.awaiting_keyframe {
                if !contains_idr(&data) {
                    self.request_keyframe().await;
                    continue;
                }
                self.awaiting_keyframe = false;
            }
            return Ok(WindowFrame {
                data,
                rtp_timestamp: packet.header.timestamp,
            });
        }
    }

    /// Asks the host's encoder for a fresh IDR (RTCP picture-loss indication).
    pub async fn request_keyframe(&self) {
        let Some(pc) = self.peer_connection.upgrade() else {
            return;
        };
        let pli = PictureLossIndication {
            sender_ssrc: 0,
            media_ssrc: self.track.ssrc(),
        };
        if let Err(e) = pc.write_rtcp(&[Box::new(pli)]).await {
            tracing::debug!("keyframe request not sent: {e}");
        }
    }
}

/// Whether an Annex-B access unit contains an IDR slice (NAL type 5).
fn contains_idr(annex_b: &[u8]) -> bool {
    nal_unit_types(annex_b).any(|t| t == 5)
}

/// NAL unit types in an Annex-B buffer (each NAL follows a 00 00 01 or
/// 00 00 00 01 start code).
fn nal_unit_types(annex_b: &[u8]) -> impl Iterator<Item = u8> + '_ {
    annex_b
        .windows(4)
        .filter(|w| w[0] == 0 && w[1] == 0 && w[2] == 1)
        .map(|w| w[3] & 0x1f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_ids_round_trip_and_ignore_foreign_tracks() {
        let id = track_id_for(WindowId(42));
        assert_eq!(window_for_track_id(&id), Some(WindowId(42)));
        assert_eq!(window_for_track_id("audio-1"), None);
        assert_eq!(window_for_track_id("window-x"), None);
    }

    #[test]
    fn finds_idr_slices_behind_either_start_code() {
        let idr = [
            0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88,
        ];
        let non_idr = [0, 0, 0, 1, 0x41, 0x9a, 0x02];
        assert!(contains_idr(&idr));
        assert!(!contains_idr(&non_idr));
    }
}

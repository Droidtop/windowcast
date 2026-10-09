//! Window audio: one Opus track per streamed window that has sound, beside
//! its video track on the same session; and the client's microphone, one
//! Opus track from the client to the host. Each RTP packet carries one Opus
//! packet (RFC 7587), so there is nothing to reassemble; the receiving end
//! hands packets on as they come and the client decodes them.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::MIME_TYPE_OPUS;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use rtc::rtp_transceiver::PayloadType;
use tokio::sync::watch;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::PeerConnection;
use webrtc::rtp_transceiver::RtpSender;
use windowcast_protocol::WindowId;

use crate::TransportError;

const STREAM_ID: &str = "windowcast";
const AUDIO_TRACK_PREFIX: &str = "audio-";
const MICROPHONE_TRACK_PREFIX: &str = "mic-";

/// Opus at 48 kHz in stereo: the entry webrtc's default codec table
/// registers, so negotiation matches it.
fn opus() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48_000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
        rtcp_feedback: vec![],
    }
}

/// `audio-<window>-<ssrc>`, unique per attach like window tracks.
fn track_id_for(window: WindowId, ssrc: u32) -> String {
    format!("{AUDIO_TRACK_PREFIX}{}-{ssrc:08x}", window.0)
}

/// `mic-<ssrc>`: the client's microphone.
fn microphone_track_id(ssrc: u32) -> String {
    format!("{MICROPHONE_TRACK_PREFIX}{ssrc:08x}")
}

pub(crate) fn is_microphone_track(id: &str) -> bool {
    id.starts_with(MICROPHONE_TRACK_PREFIX)
}

pub(crate) fn window_for_track_id(id: &str) -> Option<WindowId> {
    let (window, _ssrc) = id.strip_prefix(AUDIO_TRACK_PREFIX)?.split_once('-')?;
    window.parse().ok().map(WindowId)
}

/// Host side: an audio track added, its codec not yet negotiated.
pub(crate) struct PendingAudio {
    window: WindowId,
    ssrc: u32,
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
}

impl PendingAudio {
    /// A window's sound (host side), or with `window` None the
    /// microphone (client side).
    pub(crate) async fn add_to(
        peer_connection: &dyn PeerConnection,
        window: Option<WindowId>,
    ) -> Result<Self, TransportError> {
        let ssrc = rand_core::RngCore::next_u32(&mut rand_core::OsRng);
        let track_id = match window {
            Some(window) => track_id_for(window, ssrc),
            None => microphone_track_id(ssrc),
        };
        let window = window.unwrap_or(MICROPHONE);
        let track = Arc::new(TrackLocalStaticSample::new(
            Instant::now(),
            MediaStreamTrack::new(
                STREAM_ID.to_owned(),
                track_id.clone(),
                track_id,
                RtpCodecKind::Audio,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(ssrc),
                        ..Default::default()
                    },
                    codec: opus(),
                    ..Default::default()
                }],
            ),
        )?);
        let sender = peer_connection
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
            .await?;
        Ok(PendingAudio {
            window,
            ssrc,
            track,
            sender,
        })
    }

    pub(crate) async fn negotiated(self) -> Result<AudioTrack, TransportError> {
        let negotiated = self.sender.get_parameters().await?.rtp_parameters.codecs;
        let payload_type = negotiated
            .iter()
            .find(|c| c.rtp_codec.mime_type.eq_ignore_ascii_case(MIME_TYPE_OPUS))
            .map(|codec| codec.payload_type)
            .ok_or(TransportError::AudioNotNegotiated)?;
        Ok(AudioTrack {
            window: self.window,
            ssrc: self.ssrc,
            payload_type,
            track: self.track,
            sender: self.sender,
        })
    }
}

/// The window id a microphone track is filed under.
pub(crate) const MICROPHONE: WindowId = WindowId(u64::MAX);

/// The sending end of one window's audio (host side), or of the
/// microphone (client side).
#[derive(Clone)]
pub struct AudioTrack {
    window: WindowId,
    ssrc: u32,
    payload_type: PayloadType,
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
}

impl AudioTrack {
    pub fn window(&self) -> WindowId {
        self.window
    }

    pub(crate) fn sender(&self) -> &Arc<dyn RtpSender> {
        &self.sender
    }

    /// Sends one Opus packet lasting `duration`.
    pub async fn write_packet(
        &self,
        data: Bytes,
        duration: Duration,
    ) -> Result<(), TransportError> {
        self.track
            .sample_writer(self.ssrc, self.payload_type)
            .write_sample(&Sample {
                data,
                duration,
                ..Sample::new(Instant::now())
            })
            .await?;
        Ok(())
    }
}

/// One Opus packet received for a window.
#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub data: Bytes,
    /// The RTP timestamp (48 kHz clock) the host sent it with.
    pub rtp_timestamp: u32,
    /// Packets were lost just before this one.
    pub after_loss: bool,
}

/// Client side: the receiving end of one window's audio track.
pub struct RemoteAudio {
    window: WindowId,
    track: Arc<dyn TrackRemote>,
    ended: watch::Receiver<bool>,
    last_sequence: Option<u16>,
}

impl RemoteAudio {
    /// The client's microphone, on the host.
    pub(crate) async fn microphone(
        track: Arc<dyn TrackRemote>,
        ended: watch::Receiver<bool>,
    ) -> Option<Self> {
        is_microphone_track(&track.track_id().await).then_some(RemoteAudio {
            window: MICROPHONE,
            track,
            ended,
            last_sequence: None,
        })
    }

    /// `None` for a track that is not a window's audio.
    pub(crate) async fn new(
        track: Arc<dyn TrackRemote>,
        ended: watch::Receiver<bool>,
    ) -> Option<Self> {
        let window = window_for_track_id(&track.track_id().await)?;
        Some(RemoteAudio {
            window,
            track,
            ended,
            last_sequence: None,
        })
    }

    pub fn window(&self) -> WindowId {
        self.window
    }

    /// The next Opus packet. Fails once the track ends (the host stopped
    /// the window or the session closed).
    pub async fn next_packet(&mut self) -> Result<AudioPacket, TransportError> {
        loop {
            let event = tokio::select! {
                event = self.track.poll() => event,
                _ = self.ended.wait_for(|ended| *ended) => None,
            };
            let packet = match event {
                Some(TrackRemoteEvent::OnRtpPacket(packet)) => packet,
                Some(TrackRemoteEvent::OnEnded) | None => return Err(TransportError::Closed),
                Some(_) => continue,
            };
            let sequence = packet.header.sequence_number;
            let after_loss = self
                .last_sequence
                .is_some_and(|last| sequence != last.wrapping_add(1));
            self.last_sequence = Some(sequence);
            if packet.payload.is_empty() {
                continue;
            }
            return Ok(AudioPacket {
                data: packet.payload.clone(),
                rtp_timestamp: packet.header.timestamp,
                after_loss,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_track_ids_round_trip() {
        let id = track_id_for(WindowId(42), 7);
        assert_eq!(window_for_track_id(&id), Some(WindowId(42)));
        assert_eq!(window_for_track_id("window-42-00000007"), None);
    }
}

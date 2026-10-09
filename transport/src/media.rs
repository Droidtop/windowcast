//! Per-window video tracks: one track per streamed window, on the
//! session's one `PeerConnection`, in the codec negotiated for that window
//! (H.264, H.265 or AV1).
//!
//! The host hands each [`WindowTrack`] encoded access units (Annex-B for
//! H.264/H.265 as hardware encoders emit them; low-overhead OBUs for AV1);
//! webrtc packetizes them into RTP. The client's [`RemoteWindow`]
//! reassembles RTP back into whole access units at the marker bit, so a
//! frame is delivered the moment its last packet arrives, with no wait for
//! the next frame. A frame with a sequence gap is dropped, and so is
//! everything after it until the next keyframe, with a picture-loss request
//! sent to the host so the encoder produces one.

use std::io::Write;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use rtc::media::io::h26x_writer::H26xWriter;
use rtc::media::io::Writer;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::{
    MIME_TYPE_AV1, MIME_TYPE_H264, MIME_TYPE_HEVC,
};
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp::codec::av1::Av1Depacketizer;
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp::packetizer::Depacketizer;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use rtc::rtp_transceiver::PayloadType;
use tokio::sync::{watch, Notify};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::PeerConnection;
use webrtc::rtp_transceiver::RtpSender;
use windowcast_protocol::{VideoCodec, WindowId};

use crate::keyframes::is_keyframe_request;
use crate::TransportError;

const VIDEO_CLOCK_RATE: u32 = 90_000;
const STREAM_ID: &str = "windowcast";
const TRACK_ID_PREFIX: &str = "window-";

/// `window-<id>-<ssrc>`: unique per attach, because webrtc-rs keys remote
/// tracks by id and never forgets one (it emits no close event), so a
/// window streamed a second time under the same id would never arrive.
pub(crate) fn track_id_for(window: WindowId, ssrc: u32) -> String {
    format!("{TRACK_ID_PREFIX}{}-{ssrc:08x}", window.0)
}

pub(crate) fn window_for_track_id(id: &str) -> Option<WindowId> {
    let (window, _ssrc) = id.strip_prefix(TRACK_ID_PREFIX)?.split_once('-')?;
    window.parse().ok().map(WindowId)
}

/// The RTP codec for each windowcast codec: the same entries webrtc's
/// default codec table registers, so negotiation matches them. H.264 is
/// Constrained Baseline 3.1, packetization mode 1, which every hardware
/// decoder takes.
fn rtp_codec(codec: VideoCodec) -> RTCRtpCodec {
    let (mime_type, sdp_fmtp_line) = match codec {
        VideoCodec::H264 => (
            MIME_TYPE_H264,
            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
        ),
        VideoCodec::H265 => (MIME_TYPE_HEVC, ""),
        VideoCodec::Av1 => (MIME_TYPE_AV1, "profile-id=0"),
    };
    RTCRtpCodec {
        mime_type: mime_type.to_owned(),
        clock_rate: VIDEO_CLOCK_RATE,
        channels: 0,
        sdp_fmtp_line: sdp_fmtp_line.to_owned(),
        rtcp_feedback: vec![],
    }
}

fn codec_for_mime(mime_type: &str) -> Option<VideoCodec> {
    [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1]
        .into_iter()
        .find(|codec| rtp_codec(*codec).mime_type.eq_ignore_ascii_case(mime_type))
}

/// A track added to the peer connection whose codec is not negotiated yet.
pub(crate) struct PendingTrack {
    window: WindowId,
    codec: VideoCodec,
    ssrc: u32,
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
}

impl PendingTrack {
    /// Once the renegotiation that announced the track is done: the payload
    /// type the peer agreed to, and the keyframe-request watcher.
    pub(crate) async fn negotiated(self) -> Result<WindowTrack, TransportError> {
        // The sender lists every codec negotiated for its media section;
        // ours is the one with our MIME type, the exact fmtp line first.
        let wanted = rtp_codec(self.codec);
        let negotiated = self.sender.get_parameters().await?.rtp_parameters.codecs;
        let same_mime = |c: &&rtc::rtp_transceiver::rtp_sender::RTCRtpCodecParameters| {
            c.rtp_codec
                .mime_type
                .eq_ignore_ascii_case(&wanted.mime_type)
        };
        let payload_type = negotiated
            .iter()
            .filter(same_mime)
            .find(|c| c.rtp_codec.sdp_fmtp_line == wanted.sdp_fmtp_line)
            .or_else(|| negotiated.iter().find(same_mime))
            .map(|codec| codec.payload_type)
            .ok_or(TransportError::CodecNotNegotiated(self.codec))?;

        // Keyframe requests from the client arrive as track events (see
        // `keyframes`); the encoder waits on `keyframe_requested`.
        let keyframe_requests = Arc::new(Notify::new());
        let track = Arc::clone(&self.track);
        let notify = Arc::clone(&keyframe_requests);
        tokio::spawn(async move {
            while let Some(event) = track.poll().await {
                if let TrackLocalEvent::OnRtcpPacket(packets) = event {
                    if packets.iter().any(|p| is_keyframe_request(p.as_ref())) {
                        notify.notify_one();
                    }
                }
            }
        });

        Ok(WindowTrack {
            window: self.window,
            codec: self.codec,
            ssrc: self.ssrc,
            payload_type,
            track: self.track,
            sender: self.sender,
            keyframe_requests,
        })
    }
}

/// Host side: the sending end of one window's video track.
#[derive(Clone)]
pub struct WindowTrack {
    window: WindowId,
    codec: VideoCodec,
    ssrc: u32,
    payload_type: PayloadType,
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    keyframe_requests: Arc<Notify>,
}

impl WindowTrack {
    pub(crate) async fn add_to(
        peer_connection: &dyn PeerConnection,
        window: WindowId,
        codec: VideoCodec,
    ) -> Result<PendingTrack, TransportError> {
        let ssrc = rand_core::RngCore::next_u32(&mut rand_core::OsRng);
        let track_id = track_id_for(window, ssrc);
        let track = Arc::new(TrackLocalStaticSample::new(
            Instant::now(),
            MediaStreamTrack::new(
                STREAM_ID.to_owned(),
                track_id.clone(),
                track_id,
                RtpCodecKind::Video,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(ssrc),
                        ..Default::default()
                    },
                    codec: rtp_codec(codec),
                    ..Default::default()
                }],
            ),
        )?);
        let sender = peer_connection
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
            .await?;
        Ok(PendingTrack {
            window,
            codec,
            ssrc,
            track,
            sender,
        })
    }

    pub fn window(&self) -> WindowId {
        self.window
    }

    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// The track id the client sees, and what `StreamStartResponse::track_id`
    /// carries.
    pub fn track_id(&self) -> String {
        track_id_for(self.window, self.ssrc)
    }

    pub(crate) fn sender(&self) -> &Arc<dyn RtpSender> {
        &self.sender
    }

    /// Sends one encoded access unit, shown for `duration`: Annex-B NAL
    /// units with start codes for H.264 and H.265, a temporal unit of
    /// low-overhead OBUs for AV1. The first frame after attaching, and the
    /// first after [`WindowTrack::keyframe_requested`] fires, must be a
    /// keyframe with its parameter sets (sequence header for AV1).
    pub async fn write_frame(&self, data: Bytes, duration: Duration) -> Result<(), TransportError> {
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

    /// Resolves when the client has asked for a keyframe (it lost a frame,
    /// or just started decoding). The encoder should emit one next.
    pub async fn keyframe_requested(&self) {
        self.keyframe_requests.notified().await
    }
}

/// One encoded access unit received for a window, ready for a hardware
/// decoder (MediaCodec, VideoToolbox, Media Foundation): Annex-B for
/// H.264/H.265, low-overhead OBUs for AV1.
#[derive(Debug, Clone)]
pub struct WindowFrame {
    pub data: Bytes,
    pub codec: VideoCodec,
    /// The RTP timestamp (90 kHz clock) the host sent this frame with.
    pub rtp_timestamp: u32,
    pub keyframe: bool,
}

/// A `Write` sink the H.265 reassembler writes into, drained per packet.
#[derive(Clone, Default)]
struct SharedBuffer(Arc<StdMutex<Vec<u8>>>);

impl Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("frame buffer").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// RTP payloads to codec bitstream, per codec.
enum Reassembler {
    H264(H264Packet),
    /// webrtc's H.265 depacketizer only parses; its h26x writer turns the
    /// parsed units into Annex-B, so the writer does the job here.
    H265(Box<H26xWriter<SharedBuffer>>, SharedBuffer),
    Av1(Av1Depacketizer),
}

impl Reassembler {
    fn new(codec: VideoCodec) -> Self {
        match codec {
            VideoCodec::H264 => Reassembler::H264(H264Packet::default()),
            VideoCodec::H265 => {
                let out = SharedBuffer::default();
                Reassembler::H265(Box::new(H26xWriter::new(out.clone(), true)), out)
            }
            VideoCodec::Av1 => Reassembler::Av1(Av1Depacketizer::new()),
        }
    }

    /// Appends one packet's bitstream to `frame`; false if the packet
    /// could not be parsed.
    fn push(&mut self, packet: &rtc::rtp::Packet, frame: &mut BytesMut) -> bool {
        let bitstream = match self {
            Reassembler::H264(depacketizer) => depacketizer.depacketize(&packet.payload),
            Reassembler::Av1(depacketizer) => depacketizer.depacketize(&packet.payload),
            Reassembler::H265(writer, out) => {
                let written = writer.write_rtp(packet);
                let bytes = std::mem::take(&mut *out.0.lock().expect("frame buffer"));
                written.map(|()| Bytes::from(bytes))
            }
        };
        match bitstream {
            Ok(bitstream) => {
                frame.extend_from_slice(&bitstream);
                true
            }
            Err(_) => false,
        }
    }
}

/// Client side: the receiving end of one window's video track.
pub struct RemoteWindow {
    window: WindowId,
    codec: VideoCodec,
    media_ssrc: u32,
    track: Arc<dyn TrackRemote>,
    ended: watch::Receiver<bool>,
    reassembler: Reassembler,
    frame: BytesMut,
    last_sequence: Option<u16>,
    frame_damaged: bool,
    /// Set at start and after any loss: frames are dropped until a keyframe.
    awaiting_keyframe: bool,
}

impl RemoteWindow {
    /// `None` for a track that is not a window's, or not in a video codec
    /// windowcast uses.
    pub(crate) async fn new(
        track: Arc<dyn TrackRemote>,
        ended: watch::Receiver<bool>,
    ) -> Option<Self> {
        let window = window_for_track_id(&track.track_id().await)?;
        let media_ssrc = *track.ssrcs().await.first()?;
        let codec = codec_for_mime(&track.codec(media_ssrc).await?.mime_type)?;
        Some(RemoteWindow {
            window,
            codec,
            media_ssrc,
            track,
            ended,
            reassembler: Reassembler::new(codec),
            frame: BytesMut::new(),
            last_sequence: None,
            frame_damaged: false,
            awaiting_keyframe: true,
        })
    }

    pub fn window(&self) -> WindowId {
        self.window
    }

    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// The next complete, decodable frame. Fails once the track ends (the
    /// host stopped the window or the session closed).
    pub async fn next_frame(&mut self) -> Result<WindowFrame, TransportError> {
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
            if let Some(last) = self.last_sequence {
                if sequence != last.wrapping_add(1) {
                    self.frame_damaged = true;
                }
            }
            self.last_sequence = Some(sequence);

            if !packet.payload.is_empty() && !self.reassembler.push(&packet, &mut self.frame) {
                self.frame_damaged = true;
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
            let keyframe = is_keyframe(self.codec, &data);
            if self.awaiting_keyframe {
                if !keyframe {
                    self.request_keyframe().await;
                    continue;
                }
                self.awaiting_keyframe = false;
            }
            return Ok(WindowFrame {
                data,
                codec: self.codec,
                rtp_timestamp: packet.header.timestamp,
                keyframe,
            });
        }
    }

    /// Asks the host's encoder for a fresh keyframe (RTCP picture-loss
    /// indication).
    pub async fn request_keyframe(&self) {
        let pli = PictureLossIndication {
            sender_ssrc: 0,
            media_ssrc: self.media_ssrc,
        };
        if let Err(e) = self.track.write_rtcp(vec![Box::new(pli)]).await {
            tracing::debug!("keyframe request not sent: {e}");
        }
    }
}

/// Whether an access unit starts a decodable sequence: an IDR slice for
/// H.264, an IRAP picture for H.265, a sequence header OBU for AV1.
pub fn is_keyframe(codec: VideoCodec, data: &[u8]) -> bool {
    match codec {
        VideoCodec::H264 => annex_b_headers(data).any(|header| header & 0x1f == 5),
        VideoCodec::H265 => {
            annex_b_headers(data).any(|header| (16..=21).contains(&((header >> 1) & 0x3f)))
        }
        VideoCodec::Av1 => obu_types(data).any(|obu_type| obu_type == 1),
    }
}

/// The first byte after each Annex-B start code (00 00 01, or 00 00 00 01).
fn annex_b_headers(annex_b: &[u8]) -> impl Iterator<Item = u8> + '_ {
    annex_b
        .windows(4)
        .filter(|w| w[0] == 0 && w[1] == 0 && w[2] == 1)
        .map(|w| w[3])
}

/// OBU types in a low-overhead AV1 bitstream (every OBU carries its size;
/// one without a size runs to the end).
fn obu_types(mut data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    std::iter::from_fn(move || {
        let header = *data.first()?;
        let obu_type = (header >> 3) & 0x0f;
        let mut offset = 1 + usize::from(header & 0x04 != 0);
        if header & 0x02 == 0 {
            data = &[];
            return Some(obu_type);
        }
        let mut size = 0usize;
        for i in 0..8 {
            let byte = *data.get(offset)?;
            offset += 1;
            size |= usize::from(byte & 0x7f) << (7 * i);
            if byte & 0x80 == 0 {
                break;
            }
        }
        data = data.get(offset + size..).unwrap_or(&[]);
        Some(obu_type)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_ids_round_trip_and_ignore_foreign_tracks() {
        let id = track_id_for(WindowId(42), 0x1234_abcd);
        assert_eq!(window_for_track_id(&id), Some(WindowId(42)));
        assert_eq!(window_for_track_id("audio-1"), None);
        assert_eq!(window_for_track_id("window-x-1"), None);
        assert_eq!(window_for_track_id("window-42"), None);
    }

    #[test]
    fn finds_keyframes_in_each_codec() {
        let h264_idr = [
            0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88,
        ];
        let h264_p = [0, 0, 0, 1, 0x41, 0x9a, 0x02];
        assert!(is_keyframe(VideoCodec::H264, &h264_idr));
        assert!(!is_keyframe(VideoCodec::H264, &h264_p));

        // VPS (type 32), then an IDR_W_RADL slice (19); a TRAIL_R slice (1).
        let h265_idr = [0, 0, 0, 1, 0x40, 0x01, 0x0c, 0, 0, 0, 1, 0x26, 0x01, 0xaf];
        let h265_p = [0, 0, 0, 1, 0x02, 0x01, 0xd0];
        assert!(is_keyframe(VideoCodec::H265, &h265_idr));
        assert!(!is_keyframe(VideoCodec::H265, &h265_p));

        // Temporal delimiter (type 2, size 0), sequence header (type 1,
        // size 2); a temporal delimiter then a frame OBU (type 6, size 1).
        let av1_key = [0x12, 0x00, 0x0a, 0x02, 0xaa, 0xbb];
        let av1_delta = [0x12, 0x00, 0x32, 0x01, 0xcc];
        assert!(is_keyframe(VideoCodec::Av1, &av1_key));
        assert!(!is_keyframe(VideoCodec::Av1, &av1_delta));
    }

    #[test]
    fn every_codec_maps_back_from_its_mime_type() {
        for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
            assert_eq!(codec_for_mime(&rtp_codec(codec).mime_type), Some(codec));
        }
    }
}

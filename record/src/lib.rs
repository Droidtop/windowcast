//! Local recording (Droidtop/tracker#429): a watched window's video and its
//! sound, written to a Matroska file exactly as they were received. Nothing
//! is decoded or encoded again, so recording costs a file write, not a
//! second encoder, which matters on a handheld.
//!
//! The file is written as it goes: the segment and each cluster have an
//! unknown size (Matroska's live form), so a recording that stops part way
//! (the app killed, the battery out) still plays up to its last cluster.
//! A cluster starts at every keyframe, so a player can seek to each one.
//!
//! Video: H.264, the codec every windowcast host sends by default. H.265
//! and AV1 are refused with [`RecordError::Unsupported`] until their
//! decoder configuration records (hvcC, av1C) are written here.
//! Audio: Opus, as windowcast sends it (48 kHz, stereo).
//!
//! Timing: video and audio RTP clocks start at unrelated random values, so
//! each stream is placed by the wall clock at its first packet and then by
//! its own RTP clock, which keeps both in step without a sender report.

use std::io::{self, Write};
use std::time::Instant;

use windowcast_protocol::VideoCodec;

/// Why a recording could not start or go on.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("recording {0:?} is not supported yet; H.264 is")]
    Unsupported(VideoCodec),
    #[error(transparent)]
    Io(#[from] io::Error),
}

const VIDEO_TRACK: u64 = 1;
const AUDIO_TRACK: u64 = 2;
/// Milliseconds per Matroska tick ([`TIMESTAMP_SCALE`] nanoseconds).
const TIMESTAMP_SCALE: u64 = 1_000_000;
/// A block's time is an i16 from its cluster's: a cluster lasts at most this.
const MAX_CLUSTER_MS: u64 = 30_000;
/// Opus's standard encoder delay at 48 kHz, in samples (RFC 7845).
const OPUS_PRE_SKIP: u16 = 312;

/// What is being recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tracks {
    pub codec: VideoCodec,
    /// The window's size in pixels, as the host said it.
    pub width: u32,
    pub height: u32,
    /// The window's sound is recorded too.
    pub audio: bool,
}

/// A recording in progress. Feed it what the session receives, then
/// [`Recorder::finish`] it.
pub struct Recorder<W: Write> {
    out: W,
    tracks: Tracks,
    /// The headers are written at the first keyframe, which carries the
    /// parameter sets the video track's configuration is made from.
    started: bool,
    /// Where the recording starts on the wall clock.
    origin: Option<Instant>,
    video: Clock,
    audio: Clock,
    /// The open cluster's time, in milliseconds from the start.
    cluster: Option<u64>,
    frames: u64,
}

impl<W: Write> Recorder<W> {
    /// Starts a recording into `out`. Nothing is written until the first
    /// keyframe.
    pub fn new(out: W, tracks: Tracks) -> Result<Self, RecordError> {
        if tracks.codec != VideoCodec::H264 {
            return Err(RecordError::Unsupported(tracks.codec));
        }
        Ok(Recorder {
            out,
            tracks,
            started: false,
            origin: None,
            video: Clock::new(90_000),
            audio: Clock::new(48_000),
            cluster: None,
            frames: 0,
        })
    }

    /// One received video frame (Annex B, as `WindowFrame::data` holds it)
    /// with its RTP timestamp. Frames before the first keyframe are dropped:
    /// nothing before it can be decoded.
    pub fn video(
        &mut self,
        frame: &[u8],
        keyframe: bool,
        rtp_timestamp: u32,
    ) -> Result<(), RecordError> {
        self.video_at(frame, keyframe, rtp_timestamp, Instant::now())
    }

    /// One received Opus packet with its RTP timestamp. Packets before the
    /// video's first keyframe are dropped, so the recording starts with a
    /// picture.
    pub fn audio(&mut self, packet: &[u8], rtp_timestamp: u32) -> Result<(), RecordError> {
        self.audio_at(packet, rtp_timestamp, Instant::now())
    }

    /// How many video frames are in the file.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Ends the recording: flushes what is written and hands the output back.
    pub fn finish(mut self) -> Result<W, RecordError> {
        self.out.flush()?;
        Ok(self.out)
    }

    fn video_at(
        &mut self,
        frame: &[u8],
        keyframe: bool,
        rtp: u32,
        now: Instant,
    ) -> Result<(), RecordError> {
        if !self.started {
            if !keyframe {
                return Ok(());
            }
            let units = nal_units(frame);
            let sps = units
                .iter()
                .find(|u| u.first().map(|b| b & 0x1f) == Some(7));
            let pps = units
                .iter()
                .find(|u| u.first().map(|b| b & 0x1f) == Some(8));
            let (Some(sps), Some(pps)) = (sps, pps) else {
                // A keyframe without its parameter sets cannot start a file; the next one will.
                return Ok(());
            };
            self.write_headers(&avc_configuration(sps, pps))?;
            self.started = true;
            self.origin = Some(now);
        }
        let origin = self.origin.expect("started");
        let at = self.video.ms(rtp, now, origin);
        if keyframe
            || self
                .cluster
                .is_none_or(|c| at.saturating_sub(c) >= MAX_CLUSTER_MS)
        {
            self.open_cluster(at)?;
        }
        let payload = length_prefixed(frame);
        self.block(VIDEO_TRACK, at, keyframe, &payload)?;
        self.frames += 1;
        Ok(())
    }

    fn audio_at(&mut self, packet: &[u8], rtp: u32, now: Instant) -> Result<(), RecordError> {
        if !self.started || !self.tracks.audio {
            return Ok(());
        }
        let origin = self.origin.expect("started");
        let at = self.audio.ms(rtp, now, origin);
        let cluster = self
            .cluster
            .expect("a cluster opens with the first keyframe");
        // A packet before its cluster (it arrived late) or too far after it starts a cluster of its own.
        if at < cluster || at - cluster >= MAX_CLUSTER_MS {
            self.open_cluster(at)?;
        }
        self.block(AUDIO_TRACK, at, true, packet)?;
        Ok(())
    }

    fn write_headers(&mut self, avcc: &[u8]) -> io::Result<()> {
        let header = element(
            0x1A45_DFA3,
            &[
                uint(0x4286, 1),
                uint(0x42F7, 1),
                uint(0x42F2, 4),
                uint(0x42F3, 8),
                string(0x4282, "matroska"),
                uint(0x4287, 4),
                uint(0x4285, 2),
            ]
            .concat(),
        );
        self.out.write_all(&header)?;
        // The segment, of unknown size: written as it goes.
        self.out.write_all(&id_bytes(0x1853_8067))?;
        self.out.write_all(&UNKNOWN_SIZE)?;
        let info = element(
            0x1549_A966,
            &[
                uint(0x2A_D7B1, TIMESTAMP_SCALE),
                string(0x4D80, "windowcast"),
                string(0x5741, "windowcast"),
            ]
            .concat(),
        );
        self.out.write_all(&info)?;
        let mut tracks = element(
            0xAE,
            &[
                uint(0xD7, VIDEO_TRACK),
                uint(0x73C5, VIDEO_TRACK),
                uint(0x83, 1),
                string(0x86, "V_MPEG4/ISO/AVC"),
                element(0x63A2, avcc),
                element(
                    0xE0,
                    &[
                        uint(0xB0, u64::from(self.tracks.width)),
                        uint(0xBA, u64::from(self.tracks.height)),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        );
        if self.tracks.audio {
            tracks.extend(element(
                0xAE,
                &[
                    uint(0xD7, AUDIO_TRACK),
                    uint(0x73C5, AUDIO_TRACK),
                    uint(0x83, 2),
                    string(0x86, "A_OPUS"),
                    element(0x63A2, &opus_head()),
                    // Pre-skip in nanoseconds, and Opus's 80 ms seek pre-roll.
                    uint(0x56AA, u64::from(OPUS_PRE_SKIP) * 1_000_000_000 / 48_000),
                    uint(0x56BB, 80_000_000),
                    element(0xE1, &[float(0xB5, 48_000.0), uint(0x9F, 2)].concat()),
                ]
                .concat(),
            ));
        }
        self.out.write_all(&element(0x1654_AE6B, &tracks))
    }

    fn open_cluster(&mut self, at: u64) -> io::Result<()> {
        self.out.write_all(&id_bytes(0x1F43_B675))?;
        self.out.write_all(&UNKNOWN_SIZE)?;
        self.out.write_all(&uint(0xE7, at))?;
        self.cluster = Some(at);
        Ok(())
    }

    fn block(&mut self, track: u64, at: u64, keyframe: bool, data: &[u8]) -> io::Result<()> {
        let cluster = self.cluster.expect("a cluster is open");
        let relative = i16::try_from(at.saturating_sub(cluster)).unwrap_or(i16::MAX);
        let mut body = vint(track);
        body.extend_from_slice(&relative.to_be_bytes());
        body.push(if keyframe { 0x80 } else { 0x00 });
        body.extend_from_slice(data);
        self.out.write_all(&element(0xA3, &body))
    }
}

/// One stream's RTP clock, placed on the wall clock by its first packet.
struct Clock {
    rate: u64,
    /// The first packet's RTP time and its wall-clock offset from the start, in ms.
    first: Option<(u32, u64)>,
    /// RTP time unwrapped past `u32` overflow: the last raw value and the wraps.
    last: u32,
    wraps: u64,
}

impl Clock {
    fn new(rate: u64) -> Self {
        Clock {
            rate,
            first: None,
            last: 0,
            wraps: 0,
        }
    }

    /// Milliseconds from the recording's start for a packet at `rtp`.
    fn ms(&mut self, rtp: u32, now: Instant, origin: Instant) -> u64 {
        let Some((first, offset)) = self.first else {
            let offset = now.saturating_duration_since(origin).as_millis() as u64;
            self.first = Some((rtp, offset));
            self.last = rtp;
            return offset;
        };
        // A step back by more than half the range is the counter wrapping.
        if rtp < self.last && self.last - rtp > u32::MAX / 2 {
            self.wraps += 1;
        }
        self.last = rtp;
        let ticks = (self.wraps << 32) + u64::from(rtp);
        offset + ticks.saturating_sub(u64::from(first)) * 1000 / self.rate
    }
}

/// The NAL units of an Annex B access unit, without their start codes.
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (n, &start) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(data.len(), |next| next - 3);
        // A four-byte start code's leading zero belongs to it, not to this unit.
        while end > start && data[end - 1] == 0 && n + 1 < starts.len() {
            end -= 1;
        }
        if end > start {
            units.push(&data[start..end]);
        }
    }
    units
}

/// The access unit as Matroska stores AVC: each NAL unit after its length
/// (four bytes), access unit delimiters left out.
fn length_prefixed(frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len() + 16);
    for unit in nal_units(frame) {
        if unit[0] & 0x1f == 9 {
            continue;
        }
        out.extend_from_slice(&(unit.len() as u32).to_be_bytes());
        out.extend_from_slice(unit);
    }
    out
}

/// The AVC decoder configuration record (ISO/IEC 14496-15) for one SPS and
/// one PPS, with four-byte NAL lengths.
fn avc_configuration(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut out = vec![
        1,
        sps.get(1).copied().unwrap_or(0),
        sps.get(2).copied().unwrap_or(0),
        sps.get(3).copied().unwrap_or(0),
        0xFF,
        0xE1,
    ];
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1);
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    out
}

/// Opus's identification header (RFC 7845 section 5.1): stereo, 48 kHz.
fn opus_head() -> Vec<u8> {
    let mut out = b"OpusHead".to_vec();
    out.push(1);
    out.push(2);
    out.extend_from_slice(&OPUS_PRE_SKIP.to_le_bytes());
    out.extend_from_slice(&48_000u32.to_le_bytes());
    out.extend_from_slice(&0i16.to_le_bytes());
    out.push(0);
    out
}

/// EBML's "unknown" size, eight bytes long.
const UNKNOWN_SIZE: [u8; 8] = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

/// An element ID as written: its bytes, the length marker included.
fn id_bytes(id: u32) -> Vec<u8> {
    let bytes = id.to_be_bytes();
    let skip = bytes.iter().take_while(|b| **b == 0).count();
    bytes[skip..].to_vec()
}

/// A size or track number as an EBML variable-length integer, as short as
/// it can be.
fn vint(value: u64) -> Vec<u8> {
    for len in 1..=8u32 {
        // All ones is reserved for "unknown".
        if value < (1u64 << (7 * len)) - 1 {
            let marked = value | (1u64 << (7 * len));
            return marked.to_be_bytes()[(8 - len as usize)..].to_vec();
        }
    }
    panic!("an EBML size past 2^56");
}

fn element(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = id_bytes(id);
    out.extend(vint(body.len() as u64));
    out.extend_from_slice(body);
    out
}

fn uint(id: u32, value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let skip = bytes.iter().take_while(|b| **b == 0).count().min(7);
    element(id, &bytes[skip..])
}

fn float(id: u32, value: f64) -> Vec<u8> {
    element(id, &value.to_be_bytes())
}

fn string(id: u32, value: &str) -> Vec<u8> {
    element(id, value.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn keyframe() -> Vec<u8> {
        [
            &[0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1E, 0xAA][..],
            &[0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80][..],
            &[0, 0, 1, 0x65, 0x88, 0x84, 0x00][..],
        ]
        .concat()
    }

    #[test]
    fn sizes_are_the_shortest_variable_integers() {
        assert_eq!(vint(0), vec![0x80]);
        assert_eq!(vint(126), vec![0xFE]);
        // 127 is all ones in one byte, which means unknown: it takes two.
        assert_eq!(vint(127), vec![0x40, 0x7F]);
        assert_eq!(vint(16_382), vec![0x7F, 0xFE]);
        assert_eq!(id_bytes(0x1A45_DFA3), vec![0x1A, 0x45, 0xDF, 0xA3]);
        assert_eq!(uint(0xD7, 1), vec![0xD7, 0x81, 0x01]);
        assert_eq!(uint(0xE7, 0), vec![0xE7, 0x81, 0x00]);
    }

    #[test]
    fn annex_b_becomes_length_prefixed_and_the_configuration_names_the_parameter_sets() {
        let frame = [&[0, 0, 0, 1, 0x09, 0xF0][..], &keyframe()].concat();
        let units = nal_units(&frame);
        assert_eq!(units.len(), 4);
        assert_eq!(units[1], &[0x67, 0x42, 0xC0, 0x1E, 0xAA]);
        let out = length_prefixed(&frame);
        // The delimiter is left out; three units with their lengths remain.
        assert_eq!(&out[..9], &[0, 0, 0, 5, 0x67, 0x42, 0xC0, 0x1E, 0xAA]);
        assert_eq!(out.len(), 4 + 5 + 4 + 4 + 4 + 4);
        let avcc = avc_configuration(units[1], units[2]);
        assert_eq!(&avcc[..8], &[1, 0x42, 0xC0, 0x1E, 0xFF, 0xE1, 0, 5]);
    }

    #[test]
    fn nothing_is_written_before_a_keyframe_with_its_parameter_sets() {
        let tracks = Tracks {
            codec: VideoCodec::H264,
            width: 64,
            height: 64,
            audio: true,
        };
        let mut r = Recorder::new(Vec::new(), tracks).unwrap();
        r.audio(&[0xFC, 1, 2], 1000).unwrap();
        r.video(&[0, 0, 0, 1, 0x41, 1, 2], false, 9000).unwrap();
        r.video(&[0, 0, 0, 1, 0x65, 1, 2], true, 18000).unwrap();
        assert_eq!(r.frames(), 0);
        r.video(&keyframe(), true, 27000).unwrap();
        assert_eq!(r.frames(), 1);
        let out = r.finish().unwrap();
        assert_eq!(&out[..4], &[0x1A, 0x45, 0xDF, 0xA3]);
    }

    #[test]
    fn other_codecs_are_refused_with_a_reason() {
        let tracks = Tracks {
            codec: VideoCodec::Av1,
            width: 1,
            height: 1,
            audio: false,
        };
        let refused = Recorder::new(Vec::new(), tracks).err().unwrap();
        assert!(refused.to_string().contains("Av1"));
    }

    /// Access units of an Annex B stream from one slice per picture: each
    /// starts with the parameter sets or SEI before its slice.
    fn access_units(stream: &[u8]) -> Vec<(Vec<u8>, bool)> {
        let mut out: Vec<(Vec<u8>, bool)> = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        for unit in nal_units(stream) {
            pending.extend_from_slice(&[0, 0, 0, 1]);
            pending.extend_from_slice(unit);
            let kind = unit[0] & 0x1f;
            if kind == 1 || kind == 5 {
                out.push((std::mem::take(&mut pending), kind == 5));
            }
        }
        out
    }

    /// A real recording, read back by ffprobe: libx264's frames and libopus's
    /// packets, as a host would send them. Skipped where ffmpeg is missing.
    #[test]
    fn ffprobe_reads_the_recording_back() {
        let dir = std::env::temp_dir().join(format!("windowcast-record-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let h264 = dir.join("in.h264");
        let made = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=10",
                "-t",
                "2",
            ])
            .args(["-c:v", "libx264", "-g", "10", "-bf", "0", "-f", "h264"])
            .arg(&h264)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("ffmpeg with libx264 is not here: skipped");
            return;
        }
        let units = access_units(&std::fs::read(&h264).unwrap());
        assert_eq!(units.len(), 20);

        let mut encoder =
            opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio).unwrap();
        let tracks = Tracks {
            codec: VideoCodec::H264,
            width: 64,
            height: 48,
            audio: true,
        };
        let mut r = Recorder::new(Vec::new(), tracks).unwrap();
        let origin = Instant::now();
        let (video_base, audio_base) = (123_456u32, 987_654u32);
        let mut packet = vec![0u8; 4000];
        // Two seconds: a picture every 100 ms, an Opus packet every 20 ms.
        for ms in (0..2000u32).step_by(20) {
            let now = origin + Duration::from_millis(u64::from(ms));
            if ms % 100 == 0 {
                let (frame, key) = &units[(ms / 100) as usize];
                r.video_at(frame, *key, video_base + ms * 90, now).unwrap();
            }
            let pcm: Vec<f32> = (0..960 * 2)
                .map(|i| {
                    ((f64::from(ms) / 1000.0 + f64::from(i / 2) / 48_000.0)
                        * 440.0
                        * std::f64::consts::TAU)
                        .sin() as f32
                        * 0.2
                })
                .collect();
            let n = encoder.encode_float(&pcm, &mut packet).unwrap();
            r.audio_at(&packet[..n], audio_base + ms * 48, now).unwrap();
        }
        assert_eq!(r.frames(), 20);
        let mkv = dir.join("out.mkv");
        std::fs::write(&mkv, r.finish().unwrap()).unwrap();

        let probe = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-count_frames", "-show_entries"])
            .args([
                "stream=codec_name,width,height,nb_read_frames,channels,sample_rate",
                "-of",
                "compact=p=0:nk=0",
            ])
            .arg(&mkv)
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&probe.stdout);
        let errors = String::from_utf8_lossy(&probe.stderr);
        assert!(
            probe.status.success() && errors.trim().is_empty(),
            "ffprobe: {errors}"
        );
        assert!(
            said.contains("codec_name=h264|width=64|height=48"),
            "{said}"
        );
        assert!(said.contains("nb_read_frames=20"), "{said}");
        assert!(
            said.contains("codec_name=opus") && said.contains("sample_rate=48000|channels=2"),
            "{said}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_stream_keeps_its_own_clock_from_its_first_packet() {
        let origin = Instant::now();
        let mut video = Clock::new(90_000);
        assert_eq!(video.ms(4_294_800_000, origin, origin), 0);
        assert_eq!(video.ms(4_294_890_000, origin, origin), 1000);
        // Across the u32 wrap.
        let wrapped = (4_294_980_000u64 - (1u64 << 32)) as u32;
        assert_eq!(video.ms(wrapped, origin, origin), 2000);
        let mut audio = Clock::new(48_000);
        let later = origin + Duration::from_millis(250);
        assert_eq!(audio.ms(77, later, origin), 250);
        assert_eq!(audio.ms(77 + 48_000, later, origin), 1250);
    }
}

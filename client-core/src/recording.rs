//! Local recording of a shown window (Droidtop/tracker#429): what
//! [`ClientSession::next_frame`](crate::ClientSession::next_frame) and
//! [`ClientSession::next_audio`](crate::ClientSession::next_audio) hand out
//! is also written, as received, by `windowcast-record` on a thread of its
//! own, so the caller's decode thread never waits on the file.

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windowcast_protocol::VideoCodec;
use windowcast_record::{Recorder, Tracks};
use windowcast_transport::{AudioPacket, WindowFrame};

enum Item {
    Video(WindowFrame),
    Audio(AudioPacket),
}

/// One window's recording: the writer thread and what it was told.
pub(crate) struct Recording {
    items: Option<Sender<Item>>,
    writer: Option<JoinHandle<Result<u64, String>>>,
    /// The writer's failure, as soon as it happens, so frames stop going to it.
    failed: Arc<Mutex<Option<String>>>,
}

impl Recording {
    /// Starts recording into `path` (created, or emptied): `width` by
    /// `height` pictures, and the window's sound. The file is made when
    /// the first frame comes, in that frame's codec.
    pub(crate) fn start(path: PathBuf, width: u32, height: u32) -> std::io::Result<Self> {
        let file = File::create(&path)?;
        let (items, received) = mpsc::channel::<Item>();
        let failed: Arc<Mutex<Option<String>>> = Arc::default();
        let failure = Arc::clone(&failed);
        let writer = std::thread::Builder::new()
            .name("windowcast-record".into())
            .spawn(move || {
                let mut out = Some(BufWriter::new(file));
                let mut recorder: Option<Recorder<BufWriter<File>>> = None;
                let fail = |why: String| {
                    *failure.lock().expect("recording failure") = Some(why.clone());
                    why
                };
                while let Ok(item) = received.recv() {
                    match item {
                        Item::Video(frame) => {
                            if recorder.is_none() {
                                let tracks = Tracks {
                                    codec: frame.codec,
                                    width,
                                    height,
                                    audio: true,
                                };
                                let made = Recorder::new(out.take().expect("one recorder"), tracks)
                                    .map_err(|e| fail(e.to_string()))?;
                                recorder = Some(made);
                            }
                            let r = recorder.as_mut().expect("made above");
                            r.video(&frame.data, frame.keyframe, frame.rtp_timestamp)
                                .map_err(|e| fail(e.to_string()))?;
                        }
                        Item::Audio(packet) => {
                            if let Some(r) = recorder.as_mut() {
                                r.audio(&packet.data, packet.rtp_timestamp)
                                    .map_err(|e| fail(e.to_string()))?;
                            }
                        }
                    }
                }
                match recorder {
                    Some(r) => {
                        let frames = r.frames();
                        r.finish().map_err(|e| fail(e.to_string()))?;
                        Ok(frames)
                    }
                    None => Ok(0),
                }
            })?;
        Ok(Recording {
            items: Some(items),
            writer: Some(writer),
            failed,
        })
    }

    pub(crate) fn video(&self, frame: &WindowFrame) {
        if self.failed.lock().expect("recording failure").is_none() {
            if let Some(items) = &self.items {
                let _ = items.send(Item::Video(frame.clone()));
            }
        }
    }

    pub(crate) fn audio(&self, packet: &AudioPacket) {
        if self.failed.lock().expect("recording failure").is_none() {
            if let Some(items) = &self.items {
                let _ = items.send(Item::Audio(packet.clone()));
            }
        }
    }

    /// Ends the recording, waiting for the file to be written: the frames
    /// in it, or why it failed.
    pub(crate) fn stop(mut self) -> Result<u64, String> {
        self.items = None;
        match self.writer.take().map(JoinHandle::join) {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err("the recording's writer stopped unexpectedly".into()),
            None => Ok(0),
        }
    }
}

/// Whether frames in `codec` can be recorded.
pub fn can_record(codec: VideoCodec) -> bool {
    codec == VideoCodec::H264
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn frame(codec: VideoCodec, data: &[u8], keyframe: bool, rtp: u32) -> WindowFrame {
        WindowFrame {
            data: Bytes::copy_from_slice(data),
            codec,
            rtp_timestamp: rtp,
            keyframe,
        }
    }

    const KEYFRAME: &[u8] = &[
        0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1E, 0xAA, 0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80, 0, 0, 1,
        0x65, 0x88, 0x84, 0x00,
    ];

    #[test]
    fn a_recording_writes_its_frames_and_says_how_many() {
        let path =
            std::env::temp_dir().join(format!("windowcast-recording-{}.mkv", std::process::id()));
        let r = Recording::start(path.clone(), 64, 48).unwrap();
        // Before the keyframe: not recorded.
        r.video(&frame(VideoCodec::H264, &[0, 0, 0, 1, 0x41, 9], false, 0));
        r.video(&frame(VideoCodec::H264, KEYFRAME, true, 9000));
        r.video(&frame(
            VideoCodec::H264,
            &[0, 0, 0, 1, 0x41, 9, 8],
            false,
            18000,
        ));
        assert_eq!(r.stop(), Ok(2));
        let written = std::fs::read(&path).unwrap();
        assert_eq!(&written[..4], &[0x1A, 0x45, 0xDF, 0xA3]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_codec_that_cannot_be_recorded_is_said_at_stop() {
        let path = std::env::temp_dir().join(format!(
            "windowcast-recording-av1-{}.mkv",
            std::process::id()
        ));
        let r = Recording::start(path.clone(), 64, 48).unwrap();
        r.video(&frame(VideoCodec::Av1, &[0x12, 0x00], true, 0));
        let said = r.stop().unwrap_err();
        assert!(said.contains("Av1"), "{said}");
        assert!(!can_record(VideoCodec::Av1) && can_record(VideoCodec::H264));
        let _ = std::fs::remove_file(&path);
    }
}

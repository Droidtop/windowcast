//! A windowcast host's windows as GameStream apps: each window the host's
//! `WindowSource` lists is an app Moonlight can launch, streamed with the
//! host's own capture and H.264 encoder.

use std::sync::Arc;

use windowcast_host::WindowSource;
use windowcast_protocol::{VideoCodec, WindowId};

use crate::client::App;
use crate::stream::{Apps, StreamConfig, VideoFrame, VideoSource};

/// The host's windows, as apps.
pub struct WindowApps(pub Arc<dyn WindowSource>);

/// A window's app ID: GameStream's are positive 32-bit numbers.
pub fn app_id(window: WindowId) -> u32 {
    let id = (window.0 ^ (window.0 >> 32)) as u32 & 0x7fff_ffff;
    id.max(1)
}

impl Apps for WindowApps {
    fn apps(&self) -> Vec<App> {
        self.0
            .list_windows()
            .into_iter()
            .map(|window| App {
                id: app_id(window.id),
                title: window.title,
                hdr: false,
            })
            .collect()
    }

    fn open(&self, app: u32, _config: &StreamConfig) -> Result<Box<dyn VideoSource>, String> {
        let window = self
            .0
            .list_windows()
            .into_iter()
            .find(|w| app_id(w.id) == app)
            .ok_or("that window is gone")?;
        let frames = self.0.open(window.id, VideoCodec::H264)?;
        Ok(Box::new(WindowVideo(frames)))
    }
}

struct WindowVideo(Box<dyn windowcast_host::FrameSource>);

impl VideoSource for WindowVideo {
    fn next_frame(&mut self, keyframe: bool) -> Option<VideoFrame> {
        let frame = self.0.next_frame(keyframe)?;
        let idr = windowcast_transport::is_keyframe(VideoCodec::H264, &frame.data);
        Some(VideoFrame {
            data: frame.data,
            idr,
        })
    }
}

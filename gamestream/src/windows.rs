//! A windowcast host's windows as GameStream apps: each window the host's
//! `WindowSource` lists is an app Moonlight can launch, streamed with the
//! host's own capture and H.264 encoder. The client's input goes to that
//! window through the host's own input delivery, as a windowcast
//! session's does, and its sound is the window's own.

use std::sync::Arc;

use std::sync::mpsc::Sender;

use windowcast_host::WindowSource;
use windowcast_protocol::{InputEvent, VideoCodec, WindowId};

use crate::client::App;
use crate::input::Input;
use crate::stream::{Apps, InputSink, StreamConfig, VideoFrame, VideoSource};

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
        let window = self.window(app).ok_or("that window is gone")?;
        let frames = self.0.open(window, VideoCodec::H264)?;
        Ok(Box::new(WindowVideo(frames)))
    }

    fn audio(&self, app: u32) -> Option<Box<dyn windowcast_host::audio::AudioSource>> {
        match self.0.open_audio(self.window(app)?)? {
            Ok(sound) => Some(sound),
            Err(e) => {
                eprintln!("gamestream: no sound: {e}");
                None
            }
        }
    }

    fn input(&self, app: u32, config: &StreamConfig) -> Option<Box<dyn InputSink>> {
        Some(Box::new(WindowInput {
            window: self.window(app)?,
            deliver: windowcast_host::deliver_input(std::sync::Arc::clone(&self.0)),
            size: (config.width.max(1) as f32, config.height.max(1) as f32),
            pointer: (0.5, 0.5),
            pads: 0,
        }))
    }
}

impl WindowApps {
    fn window(&self, app: u32) -> Option<WindowId> {
        self.0
            .list_windows()
            .into_iter()
            .map(|w| w.id)
            .find(|id| app_id(*id) == app)
    }
}

/// One stream's input, into its window.
struct WindowInput {
    window: WindowId,
    deliver: Sender<(InputEvent, Option<WindowId>)>,
    /// The client's picture, which relative mouse moves are in pixels of.
    size: (f32, f32),
    /// Where the pointer is, kept here for relative moves.
    pointer: (f32, f32),
    /// The pads the client has (bit per pad).
    pads: u16,
}

impl WindowInput {
    fn send(&self, event: InputEvent) {
        let _ = self.deliver.send((event, Some(self.window)));
    }

    fn pointer_to(&mut self, x: f32, y: f32) {
        self.pointer = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
        self.send(InputEvent::PointerMove {
            window: self.window,
            x: self.pointer.0,
            y: self.pointer.1,
        });
    }
}

impl InputSink for WindowInput {
    fn input(&mut self, event: Input) {
        let window = self.window;
        match event {
            Input::PointerTo { x, y } => self.pointer_to(x, y),
            Input::PointerBy { dx, dy } => self.pointer_to(
                self.pointer.0 + f32::from(dx) / self.size.0,
                self.pointer.1 + f32::from(dy) / self.size.1,
            ),
            Input::Button { button, pressed } => self.send(InputEvent::PointerButton {
                window,
                button,
                pressed,
            }),
            Input::Scroll { dx, dy } => self.send(InputEvent::PointerScroll { window, dx, dy }),
            Input::Key { keycode, pressed } => self.send(InputEvent::Key { keycode, pressed }),
            Input::Text(text) => self.send(InputEvent::Text { text }),
            Input::Touch { id, x, y, phase } => self.send(InputEvent::Touch {
                window,
                id,
                x,
                y,
                phase,
            }),
            Input::Gamepad { pad, active, state } => {
                // Pads that left the active mask are unplugged.
                for gone in 0..16u8 {
                    if self.pads & !active & (1 << gone) != 0 {
                        self.send(InputEvent::GamepadGone { pad: gone });
                    }
                }
                self.pads = active;
                if active & (1 << (pad & 0x0f)) != 0 {
                    self.send(InputEvent::Gamepad { pad, state });
                }
            }
        }
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

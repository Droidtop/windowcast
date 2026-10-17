//! The agent's `WindowSource`: the desktop's windows, and per streamed
//! window a capture plus encoder on the stream's own thread. The options
//! can change while streams run: each stream reopens its encoder with the
//! new ones at its next picture.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use windowcast_host::quality::Quality;
use windowcast_host::{EncodedFrame, FrameSource, WindowSource};
use windowcast_protocol::{
    BackendKind, ContentHint, HostState, InputEvent, VideoCodec, WindowId, WindowInfo, WindowKind,
    SECURE_DESKTOP_WINDOW_ID,
};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

use crate::capture::{self, Capture};
use crate::clipboard;
use crate::convert;
use crate::encoder::{self, Encoder, EncoderChoice, Settings};
use crate::input::Injector;
use crate::lock;
use crate::windows_list;

/// Agent options, from the command line or a host application.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub encoder: EncoderChoice,
    /// Offer only this codec; `None` offers every codec the encoder has.
    pub codec: Option<VideoCodec>,
    pub fps: u32,
    /// Bits per second at 1920x1080; scaled by area for other sizes.
    pub bitrate_1080p: u32,
    /// The output device a client's microphone plays into (part of its
    /// name); `None` picks a known virtual audio cable.
    pub microphone: Option<String>,
    /// Offer clients a private session (docs/BACKENDS.md, "A private
    /// session"); off unless the host's owner turns it on.
    pub private_session: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            encoder: EncoderChoice::Auto,
            codec: None,
            fps: 30,
            bitrate_1080p: 8_000_000,
            microphone: None,
            private_session: false,
        }
    }
}

/// The options in force, and a counter that changes with them.
struct Current {
    options: Options,
    codecs: Vec<VideoCodec>,
    generation: u64,
}

pub struct WindowsSource {
    current: Arc<RwLock<Current>>,
    injector: Injector,
    private: crate::private::Private,
    secure: windowcast_secure_helper::Client,
    secure_size: Arc<std::sync::Mutex<(u32, u32)>>,
    secure_notice: std::sync::atomic::AtomicU8,
}

impl WindowsSource {
    /// Probes the encoders once; fails when this Windows cannot capture
    /// windows at all (Windows.Graphics.Capture needs Windows 10 1903).
    pub fn new(options: Options) -> Result<Self, String> {
        if !capture::supported() {
            return Err("Windows.Graphics.Capture is not supported on this system".into());
        }
        let codecs = codecs_for(&options)?;
        Ok(WindowsSource {
            current: Arc::new(RwLock::new(Current {
                options,
                codecs,
                generation: 0,
            })),
            injector: Injector::default(),
            private: Default::default(),
            secure: Default::default(),
            secure_size: Default::default(),
            secure_notice: std::sync::atomic::AtomicU8::new(0),
        })
    }

    pub fn options(&self) -> Options {
        self.current.read().expect("options").options.clone()
    }

    /// Changes the options. Running streams take the new encoder, frame
    /// rate and bitrate at their next picture; a codec change applies to
    /// streams started afterwards (a track keeps its codec).
    pub fn set_options(&self, options: Options) -> Result<(), String> {
        let codecs = codecs_for(&options)?;
        let mut current = self.current.write().expect("options");
        if current.options != options {
            current.options = options;
            current.codecs = codecs;
            current.generation += 1;
        }
        Ok(())
    }

    fn open_stream(&self, window: WindowId, codec: VideoCodec, desktop: bool) -> WindowStream {
        WindowStream {
            window,
            codec,
            desktop,
            current: Arc::clone(&self.current),
            generation: u64::MAX,
            state: None,
            next_at: Instant::now(),
            last_sent: Instant::now(),
            frames: 0,
            pending: Default::default(),
            announce: None,
            encoder_name: String::new(),
            timings: std::env::var_os("WINDOWCAST_TIMINGS").map(|_| Timings::new()),
            quality: Quality::default(),
            settings: None,
            scaled: Vec::new(),
        }
    }
}

fn codecs_for(options: &Options) -> Result<Vec<VideoCodec>, String> {
    let codecs: Vec<VideoCodec> = encoder::available_codecs(options.encoder)
        .into_iter()
        .filter(|codec| options.codec.is_none_or(|only| only == *codec))
        .collect();
    if codecs.is_empty() {
        return Err(match options.codec {
            Some(codec) => format!("{} cannot encode {codec:?}", options.encoder.name()),
            None => format!("no encoder available for {}", options.encoder.name()),
        });
    }
    Ok(codecs)
}

fn input_targets_secure(event: &InputEvent, focus: Option<WindowId>) -> bool {
    match event {
        InputEvent::PointerMove { window, .. }
        | InputEvent::PointerButton { window, .. }
        | InputEvent::PointerScroll { window, .. }
        | InputEvent::Touch { window, .. } => *window == SECURE_DESKTOP_WINDOW_ID,
        InputEvent::Text { .. } | InputEvent::Key { .. } => focus == Some(SECURE_DESKTOP_WINDOW_ID),
        InputEvent::Gamepad { .. } | InputEvent::GamepadGone { .. } => false,
    }
}

fn route_secure_input(
    client: &windowcast_secure_helper::Client,
    size: &std::sync::Mutex<(u32, u32)>,
    event: &InputEvent,
) {
    use windowcast_secure_helper::operations::{MouseAction, MouseButton, Operation};
    let operation = match event {
        InputEvent::PointerMove { x, y, .. } => {
            let (width, height) = *size.lock().expect("secure size");
            if width == 0 || height == 0 {
                return;
            }
            let x = (x.clamp(0.0, 1.0) * (width.saturating_sub(1)) as f32).round() as i32;
            let y = (y.clamp(0.0, 1.0) * (height.saturating_sub(1)) as f32).round() as i32;
            Operation::SendInput {
                text: None,
                mouse: Some(MouseAction::Move { x, y }),
            }
        }
        InputEvent::PointerButton {
            button, pressed, ..
        } => {
            let button = match button {
                windowcast_protocol::PointerButton::Left => MouseButton::Left,
                windowcast_protocol::PointerButton::Right => MouseButton::Right,
                windowcast_protocol::PointerButton::Middle => MouseButton::Middle,
                // The privileged allowlist intentionally has no extra mouse buttons.
                windowcast_protocol::PointerButton::Back
                | windowcast_protocol::PointerButton::Forward => return,
            };
            Operation::SendInput {
                text: None,
                mouse: Some(MouseAction::Button {
                    button,
                    pressed: *pressed,
                }),
            }
        }
        InputEvent::Text { text } => Operation::SendInput {
            text: Some(text.clone()),
            mouse: None,
        },
        // Keys, wheel, touch and gamepads are outside the helper's owner-approved operation set.
        _ => return,
    };
    if let Err(error) = client.request(&operation) {
        eprintln!("secure desktop input was refused: {error}");
    }
}

impl WindowSource for WindowsSource {
    fn list_windows(&self) -> Vec<WindowInfo> {
        let mut windows = windows_list::list();
        use windowcast_secure_helper::SecureScreenStatus;
        let status = self.secure.permission_status();
        let notice = match status {
            SecureScreenStatus::Enabled => 1,
            SecureScreenStatus::Disabled => 2,
            SecureScreenStatus::Absent | SecureScreenStatus::NotAvailable => 3,
        };
        if self
            .secure_notice
            .swap(notice, std::sync::atomic::Ordering::Relaxed)
            != notice
        {
            match status {
                SecureScreenStatus::Disabled => {
                    eprintln!("Secure screen is unavailable: the installed helper is disabled.")
                }
                SecureScreenStatus::Absent => {
                    eprintln!("Secure screen is unavailable: the helper is not installed.")
                }
                SecureScreenStatus::NotAvailable => {
                    eprintln!("Secure screen is not available on this platform.")
                }
                SecureScreenStatus::Enabled => {}
            }
        }
        if status == SecureScreenStatus::Enabled {
            let (width, height) = *self.secure_size.lock().expect("secure size");
            windows.push(WindowInfo {
                id: SECURE_DESKTOP_WINDOW_ID,
                title: "Secure desktop".into(),
                app_id: "secure-desktop".into(),
                width,
                height,
                focused: false,
                content: ContentHint::General,
                owner: None,
                kind: WindowKind::Normal,
                position: Some((0, 0)),
            });
        }
        windows
    }

    fn cursor(&self) -> Option<windowcast_host::CursorState> {
        crate::cursor::now()
    }

    fn cursor_image(&self, shape: u64) -> Option<windowcast_protocol::CursorImage> {
        crate::cursor::image(shape)
    }

    fn encoders(&self) -> Vec<VideoCodec> {
        self.current.read().expect("options").codecs.clone()
    }

    fn backends(&self) -> Vec<BackendKind> {
        vec![BackendKind::Native, BackendKind::Desktop]
    }

    fn input(&self, event: &InputEvent, focus: Option<WindowId>) {
        if input_targets_secure(event, focus) {
            route_secure_input(&self.secure, &self.secure_size, event);
            return;
        }
        self.injector.inject(event, focus);
    }

    fn clipboard(&self) -> Option<(u64, String)> {
        Some((clipboard::sequence(), clipboard::text().unwrap_or_default()))
    }

    fn set_clipboard(&self, text: &str) {
        clipboard::set_text(text);
    }

    fn host_state(&self) -> HostState {
        HostState {
            locked: lock::session_locked(),
            private_session: self.private.state(
                self.current
                    .read()
                    .expect("options")
                    .options
                    .private_session,
            ),
        }
    }

    fn set_private_session(
        &self,
        start: bool,
        password: Option<&str>,
    ) -> Result<(), windowcast_host::PrivateRefusal> {
        if start && !self.options().private_session {
            return Err(windowcast_host::PrivateRefusal::new(
                "This host does not offer a private session.",
            ));
        }
        self.private.set(start, password)
    }

    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        if window == SECURE_DESKTOP_WINDOW_ID {
            return Ok(Box::new(SecureStream::new(
                self.secure.clone(),
                Arc::clone(&self.secure_size),
                Arc::clone(&self.current),
                codec,
            )));
        }
        check_window(window)?;
        Ok(Box::new(self.open_stream(window, codec, false)))
    }

    fn open_desktop(
        &self,
        window: WindowId,
        codec: VideoCodec,
    ) -> Result<Box<dyn FrameSource>, String> {
        if window == SECURE_DESKTOP_WINDOW_ID {
            return self.open(window, codec);
        }
        check_window(window)?;
        Ok(Box::new(self.open_stream(window, codec, true)))
    }

    fn gamepads(&self) -> Option<Result<Box<dyn windowcast_host::gamepad::GamepadSink>, String>> {
        Some(crate::gamepad::WindowsPads::open().map(|pads| Box::new(pads) as _))
    }

    fn open_pictures(
        &self,
        window: WindowId,
    ) -> Option<Result<Box<dyn windowcast_host::PictureSource>, String>> {
        if window == SECURE_DESKTOP_WINDOW_ID {
            return Some(Err(
                "the secure desktop is available only through its helper stream".into(),
            ));
        }
        if let Err(e) = check_window(window) {
            return Some(Err(e));
        }
        let fps = self.current.read().expect("options").options.fps.max(1);
        Some(Ok(Box::new(WindowPictures {
            window,
            capture: None,
            frame_time: frame_time(fps),
            next_at: Instant::now(),
            last_sent: None,
        })))
    }

    fn microphone(
        &self,
    ) -> Option<Result<Box<dyn windowcast_host::audio::MicrophoneSink>, String>> {
        let named = self.options().microphone;
        Some(
            crate::microphone::WindowsMicrophone::open(named.as_deref())
                .map(|mic| Box::new(mic) as _),
        )
    }

    fn open_audio(
        &self,
        window: WindowId,
    ) -> Option<Result<Box<dyn windowcast_host::audio::AudioSource>, String>> {
        Some(crate::audio::ProcessAudio::open(window).map(|audio| Box::new(audio) as _))
    }
}

/// Windows stream whether or not the PC is locked: their own capture goes
/// on while the lock screen is up (docs/BACKENDS.md, "A locked host").
fn check_window(window: WindowId) -> Result<(), String> {
    if !unsafe { IsWindow(Some(windows_list::hwnd(window))) }.as_bool() {
        return Err("no such window".into());
    }
    Ok(())
}

struct SecureStream {
    client: windowcast_secure_helper::Client,
    size: Arc<std::sync::Mutex<(u32, u32)>>,
    current: Arc<RwLock<Current>>,
    codec: VideoCodec,
    encoder: Option<Box<dyn Encoder>>,
    encoder_name: String,
    encoded_size: (usize, usize),
    generation: u64,
    settings: Option<Settings>,
    quality: Quality,
    frames: u64,
    pending: std::collections::VecDeque<Vec<u8>>,
    announce: Option<(u32, u32)>,
    last_sent: Instant,
    next_at: Instant,
}

impl SecureStream {
    fn new(
        client: windowcast_secure_helper::Client,
        size: Arc<std::sync::Mutex<(u32, u32)>>,
        current: Arc<RwLock<Current>>,
        codec: VideoCodec,
    ) -> Self {
        Self {
            client,
            size,
            current,
            codec,
            encoder: None,
            encoder_name: String::new(),
            encoded_size: (0, 0),
            generation: u64::MAX,
            settings: None,
            quality: Quality::default(),
            frames: 0,
            pending: Default::default(),
            announce: None,
            last_sent: Instant::now(),
            next_at: Instant::now(),
        }
    }

    fn take_pending(&mut self) -> Option<EncodedFrame> {
        let data = self.pending.pop_front()?;
        let duration = self.last_sent.elapsed();
        self.last_sent = Instant::now();
        Some(EncodedFrame {
            data,
            duration,
            size: self.announce.take(),
        })
    }
}

// The encoder is constructed and used only on the host's stream thread.
unsafe impl Send for SecureStream {}

impl FrameSource for SecureStream {
    fn describe(&self) -> String {
        self.encoder_name.clone()
    }

    fn frame_rate(&self) -> u32 {
        self.current.read().expect("options").options.fps
    }

    fn set_quality(&mut self, quality: Quality) {
        self.quality = quality;
    }

    fn next_frame(&mut self, mut keyframe: bool) -> Option<EncodedFrame> {
        if !keyframe {
            if let Some(frame) = self.take_pending() {
                return Some(frame);
            }
        }
        loop {
            let (options, generation) = {
                let current = self.current.read().expect("options");
                (current.options.clone(), current.generation)
            };
            let interval = frame_time(fps(&options, &self.quality));
            std::thread::sleep(self.next_at.saturating_duration_since(Instant::now()));
            let response = match self
                .client
                .request(&windowcast_secure_helper::operations::Operation::CaptureFrame)
            {
                Ok(response) => response,
                Err(error) => {
                    eprintln!("secure desktop capture stopped: {error}");
                    return None;
                }
            };
            let windowcast_secure_helper::operations::Response::Frame {
                width,
                height,
                stride,
                bgra,
            } = response
            else {
                eprintln!("secure desktop helper returned the wrong response");
                return None;
            };
            *self.size.lock().expect("secure size") = (width, height);
            let picture = convert::Picture::Bgra(convert::Bgra {
                data: &bgra,
                width: width as usize,
                height: height as usize,
                stride: stride as usize,
            });
            let size = convert::even(picture.width(), picture.height());
            if size.0 < 2 || size.1 < 2 {
                continue;
            }
            let wanted = settings(&options, &self.quality, self.codec, size.0, size.1);
            let reopen = self.encoder.is_none()
                || size != self.encoded_size
                || generation != self.generation
                || self.settings.is_some_and(|old| old.fps != wanted.fps);
            if reopen {
                let opened = encoder::open(options.encoder, &wanted)
                    .or_else(|_| encoder::open(EncoderChoice::Auto, &wanted));
                let encoder = match opened {
                    Ok(encoder) => encoder,
                    Err(error) => {
                        eprintln!("secure desktop encoder failed: {error}");
                        return None;
                    }
                };
                self.encoder_name = encoder.describe();
                self.encoder = Some(encoder);
                self.encoded_size = size;
                self.generation = generation;
                self.settings = Some(wanted);
                self.announce = Some((size.0 as u32, size.1 as u32));
                self.pending.clear();
                keyframe = true;
            } else if self
                .settings
                .is_some_and(|old| old.bitrate != wanted.bitrate)
            {
                if self
                    .encoder
                    .as_mut()
                    .is_some_and(|encoder| encoder.set_bitrate(wanted.bitrate))
                {
                    self.settings = Some(wanted);
                } else {
                    self.encoder = None;
                    continue;
                }
            }
            self.next_at = Instant::now() + interval;
            let time = interval * self.frames as u32;
            match self
                .encoder
                .as_mut()
                .expect("secure encoder")
                .encode(&picture, keyframe, time)
            {
                Ok(units) => {
                    self.frames += 1;
                    self.pending.extend(units);
                    if let Some(frame) = self.take_pending() {
                        return Some(frame);
                    }
                }
                Err(error) => {
                    eprintln!("secure desktop encoding failed: {error}");
                    return None;
                }
            }
        }
    }
}

struct State {
    capture: Capture,
    /// Whether `capture` is of the whole screen (a desktop stream while
    /// the input desktop is in reach).
    screen: bool,
    /// When the desktop backend last looked at whether the screen is
    /// locked.
    checked: Instant,
    encoder: Box<dyn Encoder>,
    size: (usize, usize),
}

/// One streamed window. Its capture and encoder are created on, and only
/// used from, the stream's thread (see host-core's `feed`).
struct WindowStream {
    window: WindowId,
    codec: VideoCodec,
    /// Cut out of a whole-screen capture (the desktop backend).
    desktop: bool,
    current: Arc<RwLock<Current>>,
    /// The options generation the encoder was opened with.
    generation: u64,
    state: Option<State>,
    next_at: Instant,
    last_sent: Instant,
    frames: u64,
    pending: std::collections::VecDeque<Vec<u8>>,
    /// The picture size to announce with the next frame sent.
    announce: Option<(u32, u32)>,
    encoder_name: String,
    /// Per-stage timings, printed every few seconds when the environment
    /// sets WINDOWCAST_TIMINGS (for measuring the capture pipeline).
    timings: Option<Timings>,
    /// What adaptive quality holds the stream to.
    quality: Quality,
    /// What the encoder was opened with or last changed to.
    settings: Option<Settings>,
    /// A read-back picture scaled down, when the GPU does not scale.
    scaled: Vec<u8>,
}

struct Timings {
    since: Instant,
    frames: u32,
    readback: Duration,
    encode: Duration,
    /// Keyframes asked of the encoder (first frame, size changes and the
    /// client's requests); the rest it makes on its own interval.
    forced: u32,
}

impl Timings {
    fn add(&mut self, window: WindowId, readback: Duration, encode: Duration, forced: bool) {
        self.frames += 1;
        self.forced += u32::from(forced);
        self.readback += readback;
        self.encode += encode;
        let elapsed = self.since.elapsed();
        if elapsed >= Duration::from_secs(5) {
            let per = |total: Duration| total.as_secs_f64() * 1000.0 / f64::from(self.frames);
            println!(
                "window {}: {:.1} fps; capture and read back {:.2} ms, convert and encode {:.2} ms a frame; {} keyframes asked for",
                window.0,
                f64::from(self.frames) / elapsed.as_secs_f64(),
                per(self.readback),
                per(self.encode),
                self.forced
            );
            *self = Timings::new();
        }
    }

    fn new() -> Self {
        Timings {
            since: Instant::now(),
            frames: 0,
            readback: Duration::ZERO,
            encode: Duration::ZERO,
            forced: 0,
        }
    }
}

/// How often a desktop stream looks at whether the screen is locked.
const LOCK_CHECK: Duration = Duration::from_secs(1);

impl WindowStream {
    /// Whether this stream's pictures come from the whole screen: the
    /// desktop backend, while the screen shows the desktop.
    fn wants_screen(&self) -> bool {
        self.desktop && lock::input_desktop_reachable()
    }

    /// A new capture of the screen or of the window on its own.
    fn capture(&self, screen: bool) -> Option<Capture> {
        let hwnd = windows_list::hwnd(self.window);
        let capture = if screen {
            Capture::desktop(hwnd)
        } else {
            Capture::window(hwnd)
        };
        match capture {
            Ok(mut capture) => {
                capture.set_scale(self.quality.scale);
                Some(capture)
            }
            Err(e) => {
                eprintln!("capture of window {} failed: {e}", self.window.0);
                None
            }
        }
    }

    fn take_pending(&mut self) -> Option<EncodedFrame> {
        let data = self.pending.pop_front()?;
        let duration = self.last_sent.elapsed();
        self.last_sent = Instant::now();
        Some(EncodedFrame {
            data,
            duration,
            size: self.announce.take(),
        })
    }

    fn options(&self) -> (Options, u64) {
        let current = self.current.read().expect("options");
        (current.options.clone(), current.generation)
    }
}

// Every Windows object in `state` is created on the stream's own thread
// after the move, in the multithreaded apartment.
unsafe impl Send for WindowStream {}

/// A static window still gets a frame this often, so a client that just
/// joined or lost a packet is never left without a picture.
const REFRESH: Duration = Duration::from_secs(1);

/// A window's pictures as captured, read back as BGRA, for backends that
/// encode their own way: one when the window changes (and once a second
/// regardless), never faster than the frame rate. The capture starts on
/// the thread that asks for pictures, as Windows.Graphics.Capture wants.
struct WindowPictures {
    window: WindowId,
    capture: Option<Capture>,
    frame_time: Duration,
    next_at: Instant,
    last_sent: Option<Instant>,
}

// The capture is created on the thread that asks for pictures, after the
// move, in the multithreaded apartment.
unsafe impl Send for WindowPictures {}

impl windowcast_host::PictureSource for WindowPictures {
    fn next_picture(&mut self) -> Option<windowcast_host::Picture> {
        capture::init_thread();
        let hwnd = windows_list::hwnd(self.window);
        if self.capture.is_none() {
            match Capture::window(hwnd) {
                Ok(mut capture) => {
                    capture.set_bgra_output();
                    self.capture = Some(capture);
                }
                Err(e) => {
                    eprintln!("capture of window {} failed: {e}", self.window.0);
                    return None;
                }
            }
        }
        let capture = self.capture.as_mut().expect("capture");
        loop {
            if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                return None;
            }
            std::thread::sleep(self.next_at.saturating_duration_since(Instant::now()));
            let changed = match capture.poll(self.frame_time) {
                Ok(changed) => changed,
                Err(e) => {
                    eprintln!("capture of window {} stopped: {e}", self.window.0);
                    return None;
                }
            };
            let due = self.last_sent.is_none_or(|sent| sent.elapsed() >= REFRESH);
            if !(changed || due) {
                continue;
            }
            let Some(convert::Picture::Bgra(picture)) = capture.picture() else {
                continue;
            };
            let row = picture.width * 4;
            let mut data = Vec::with_capacity(row * picture.height);
            for y in 0..picture.height {
                data.extend_from_slice(&picture.data[y * picture.stride..][..row]);
            }
            self.next_at = Instant::now() + self.frame_time;
            self.last_sent = Some(Instant::now());
            return Some(windowcast_host::Picture {
                width: picture.width as u32,
                height: picture.height as u32,
                stride: row,
                data,
            });
        }
    }
}

/// The frame rate in force: the setting, or less under adaptive quality.
fn fps(options: &Options, quality: &Quality) -> u32 {
    quality
        .fps
        .map_or(options.fps, |fps| fps.min(options.fps))
        .max(1)
}

fn frame_time(fps: u32) -> Duration {
    Duration::from_nanos(1_000_000_000 / u64::from(fps.max(1)))
}

/// Encoder settings for a picture size; the bitrate scales with area, and
/// adaptive quality may hold it and the frame rate lower.
fn settings(
    options: &Options,
    quality: &Quality,
    codec: VideoCodec,
    width: usize,
    height: usize,
) -> Settings {
    let area = (width * height) as u64;
    let nominal =
        (u64::from(options.bitrate_1080p) * area / (1920 * 1080)).clamp(500_000, 50_000_000) as u32;
    Settings {
        codec,
        width,
        height,
        fps: fps(options, quality),
        bitrate: quality.bitrate.map_or(nominal, |held| held.min(nominal)),
    }
}

impl FrameSource for WindowStream {
    fn describe(&self) -> String {
        self.encoder_name.clone()
    }

    fn frame_rate(&self) -> u32 {
        self.current.read().expect("options").options.fps
    }

    fn set_quality(&mut self, quality: Quality) {
        self.quality = quality;
        if let Some(state) = &mut self.state {
            state.capture.set_scale(quality.scale);
        }
    }

    fn next_frame(&mut self, mut keyframe: bool) -> Option<EncodedFrame> {
        // An encoder that caught up gave more than one frame last time.
        if !keyframe {
            if let Some(frame) = self.take_pending() {
                return Some(frame);
            }
        }
        capture::init_thread();
        let hwnd = windows_list::hwnd(self.window);
        if self.state.is_none() {
            let screen = self.wants_screen();
            let capture = self.capture(screen)?;
            // The encoder is opened at the first picture's size.
            self.state = Some(State {
                capture,
                screen,
                checked: Instant::now(),
                encoder: Box::new(NoEncoder),
                size: (0, 0),
            });
        }
        loop {
            if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                return None;
            }
            // A cut of the screen would show the lock screen: while the PC
            // is locked the desktop backend serves the window's own
            // capture, and the screen's again once it is unlocked.
            let due = self
                .state
                .as_ref()
                .is_some_and(|s| self.desktop && s.checked.elapsed() >= LOCK_CHECK);
            if due {
                let screen = self.wants_screen();
                let state = self.state.as_mut().expect("state");
                state.checked = Instant::now();
                if screen != state.screen {
                    println!(
                        "window {}: {}",
                        self.window.0,
                        if screen {
                            "the screen is unlocked; cutting the window out of it again"
                        } else {
                            "the screen is locked; capturing the window on its own"
                        }
                    );
                    if let Some(capture) = self.capture(screen) {
                        let state = self.state.as_mut().expect("state");
                        state.capture = capture;
                        state.screen = screen;
                        // The encoder opens again for the new capture (its
                        // size, and whether it takes textures from it).
                        state.size = (0, 0);
                    }
                }
            }
            let (options, generation) = self.options();
            let frame_time = frame_time(fps(&options, &self.quality));
            // Never faster than the frame rate: wait out the frame time, then
            // up to one more frame time for a new picture.
            std::thread::sleep(self.next_at.saturating_duration_since(Instant::now()));
            let state = self.state.as_mut().expect("state");
            let changed = match state.capture.poll(frame_time) {
                Ok(changed) => changed,
                Err(e) => {
                    eprintln!("capture of window {} stopped: {e}", self.window.0);
                    return None;
                }
            };

            let Some(mut picture) = state.capture.picture() else {
                continue;
            };
            if !(changed || keyframe || self.last_sent.elapsed() >= REFRESH) {
                continue;
            }
            // A picture read back into memory is scaled here; the GPU path
            // scales during conversion.
            if let convert::Picture::Bgra(bgra) = &picture {
                let to = convert::scaled(bgra.width, bgra.height, self.quality.scale);
                if to != convert::even(bgra.width, bgra.height) {
                    picture =
                        convert::Picture::Bgra(convert::scale_bgra(bgra, to, &mut self.scaled));
                }
            }
            let size = convert::even(picture.width(), picture.height());
            let mut texture_output = None;
            let wanted = settings(&options, &self.quality, self.codec, size.0, size.1);
            // A new bitrate alone goes to the running encoder; anything else
            // (or an encoder that cannot) opens a new one.
            let reopen = size != state.size
                || generation != self.generation
                || self.settings.is_some_and(|s| s.fps != wanted.fps);
            if !reopen && self.settings.is_some_and(|s| s.bitrate != wanted.bitrate) {
                if state.encoder.set_bitrate(wanted.bitrate) {
                    self.settings = Some(wanted);
                } else {
                    state.size = (0, 0);
                }
            }
            if size != state.size || reopen {
                let settings = wanted;
                // A live change to an encoder that cannot make this
                // stream's codec keeps the stream going on the best one
                // that can.
                let device = state.capture.device();
                let opened = encoder::open_on(options.encoder, &settings, device.as_ref())
                    .or_else(|_| encoder::open_on(EncoderChoice::Auto, &settings, device.as_ref()));
                state.encoder = match opened {
                    Ok(encoder) => {
                        println!(
                            "window {}: {}x{} {:?} with {}",
                            self.window.0,
                            size.0,
                            size.1,
                            self.codec,
                            encoder.describe()
                        );
                        encoder
                    }
                    Err(e) => {
                        eprintln!("window {}: {e}", self.window.0);
                        return None;
                    }
                };
                self.encoder_name = state.encoder.describe();
                self.settings = Some(settings);
                // From the next picture on (this one is already read).
                texture_output = Some(state.encoder.takes_textures());
                self.generation = generation;
                state.size = size;
                self.announce = Some((size.0 as u32, size.1 as u32));
                self.pending.clear();
                keyframe = true;
            }
            self.next_at = Instant::now() + frame_time;
            let time = frame_time * self.frames as u32;
            let started = Instant::now();
            let encoded = state.encoder.encode(&picture, keyframe, time);
            if let Some(on) = texture_output {
                state.capture.set_texture_output(on);
            }
            if let Some(timings) = &mut self.timings {
                timings.add(
                    self.window,
                    state.capture.readback,
                    started.elapsed(),
                    keyframe,
                );
            }
            match encoded {
                Ok(units) => {
                    self.frames += 1;
                    self.pending.extend(units);
                    if let Some(frame) = self.take_pending() {
                        return Some(frame);
                    }
                }
                Err(e) => {
                    eprintln!("window {}: encoding failed: {e}", self.window.0);
                    return None;
                }
            }
        }
    }
}

/// Stands in until the first picture gives the encoder its size.
struct NoEncoder;

impl Encoder for NoEncoder {
    fn encode(
        &mut self,
        _: &convert::Picture<'_>,
        _: bool,
        _: Duration,
    ) -> Result<Vec<Vec<u8>>, String> {
        Ok(Vec::new())
    }

    fn describe(&self) -> String {
        "none yet".into()
    }
}

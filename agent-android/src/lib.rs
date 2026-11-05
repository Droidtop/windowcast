//! windowcast's Android host (Droidtop/tracker#615): each app on a virtual
//! display of its own, drawn straight into a MediaCodec encoder's input
//! surface, and served by windowcast's host as one window per app.
//!
//! The Android side (`dev.windowcast.host.AndroidHost` in the Android
//! library, running as the shell user through Shizuku) owns the displays and
//! the encoders. This crate is the host's half: a [`WindowSource`] whose
//! windows and frames the Android side hands in through the C interface
//! below, and which asks for what it needs (start or stop encoding a window,
//! a keyframe, a bitrate) as [`Command`]s the Android side takes in turn.
//! Nothing here calls up into Java, so the bridge is plain C calls on the
//! Android side's own threads.

use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use windowcast_host::{
    audio::AudioSource, quality::Quality, EncodedFrame, FrameSource, WindowSource,
};
use windowcast_protocol::{ContentHint, InputEvent, VideoCodec, WindowId, WindowInfo, WindowKind};

/// Frames waiting for the host to send them; a stream that falls behind
/// drops the newest rather than the host growing a queue.
const FRAMES_QUEUED: usize = 8;
/// Sound waiting to be encoded: a few hundred milliseconds of buffers.
const SOUND_QUEUED: usize = 32;

/// What the host asks of the Android side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// Start encoding `window` in `codec` ("h264", "h265", "av1").
    Open { window: u64, codec: String },
    /// Stop encoding `window`: nobody watches it any more.
    Close { window: u64 },
    /// The next frame of `window` must be a keyframe with its parameter sets.
    Keyframe { window: u64 },
    /// Hold `window`'s encoder to `bitrate` bits a second.
    Bitrate { window: u64, bitrate: u32 },
    /// Inject `event` (an `InputEvent` in serde's form) into `window`'s
    /// display: the window it names, or the session's focus for keys and
    /// text.
    Input {
        window: u64,
        event: serde_json::Value,
    },
    /// Start capturing `window`'s app's sound.
    OpenAudio { window: u64 },
    /// Stop capturing it.
    CloseAudio { window: u64 },
}

/// One app's window, as the Android side lists it.
#[derive(Debug, Clone, Deserialize)]
pub struct ListedWindow {
    pub id: u64,
    /// The app's label.
    pub title: String,
    /// Its package.
    pub package: String,
    pub width: u32,
    pub height: u32,
}

/// The state both halves share.
#[derive(Default)]
pub struct Bridge {
    windows: Mutex<Vec<WindowInfo>>,
    encoders: Mutex<Vec<VideoCodec>>,
    streams: Mutex<HashMap<u64, SyncSender<EncodedFrame>>>,
    sounds: Mutex<HashMap<u64, SyncSender<Vec<i16>>>>,
    commands: Mutex<Option<Sender<Command>>>,
    pending: Mutex<Option<Receiver<Command>>>,
}

impl Bridge {
    pub fn new() -> Arc<Self> {
        let (tx, rx) = mpsc::channel();
        let bridge = Bridge::default();
        *bridge.commands.lock().expect("commands") = Some(tx);
        *bridge.pending.lock().expect("pending") = Some(rx);
        // H.264 is every Android encoder's; the Android side says what else it has.
        *bridge.encoders.lock().expect("encoders") = vec![VideoCodec::H264];
        Arc::new(bridge)
    }

    /// The windows the Android side has now: one per app on its own display.
    pub fn set_windows(&self, listed: &[ListedWindow]) {
        let windows = listed
            .iter()
            .map(|w| WindowInfo {
                id: WindowId(w.id),
                title: w.title.clone(),
                app_id: w.package.clone(),
                width: w.width,
                height: w.height,
                focused: false,
                content: ContentHint::General,
                owner: None,
                kind: WindowKind::Normal,
                position: None,
            })
            .collect();
        *self.windows.lock().expect("windows") = windows;
    }

    /// The codecs the device's encoders offer, most preferred first.
    pub fn set_encoders(&self, codecs: Vec<VideoCodec>) {
        if !codecs.is_empty() {
            *self.encoders.lock().expect("encoders") = codecs;
        }
    }

    /// One encoded frame of `window` from MediaCodec. False when nobody
    /// streams it (the Android side should stop its encoder).
    pub fn frame(&self, window: u64, frame: EncodedFrame) -> bool {
        let sender = self.streams.lock().expect("streams").get(&window).cloned();
        match sender.map(|s| s.try_send(frame)) {
            Some(Ok(())) | Some(Err(TrySendError::Full(_))) => true,
            Some(Err(TrySendError::Disconnected(_))) | None => false,
        }
    }

    /// `window`'s app went away: its stream ends.
    pub fn ended(&self, window: u64) {
        self.streams.lock().expect("streams").remove(&window);
        self.sounds.lock().expect("sounds").remove(&window);
    }

    /// Interleaved stereo 16-bit samples at 48 kHz of `window`'s app. False
    /// when nobody listens (stop capturing).
    pub fn sound(&self, window: u64, samples: Vec<i16>) -> bool {
        let sender = self.sounds.lock().expect("sounds").get(&window).cloned();
        match sender.map(|s| s.try_send(samples)) {
            Some(Ok(())) | Some(Err(TrySendError::Full(_))) => true,
            Some(Err(TrySendError::Disconnected(_))) | None => false,
        }
    }

    /// `window`'s sound cannot be captured (the app opted out, or the
    /// device refused): its audio ends; the picture goes on.
    pub fn sound_ended(&self, window: u64) {
        self.sounds.lock().expect("sounds").remove(&window);
    }

    /// The next thing the host asks, waiting up to `timeout`.
    pub fn next_command(&self, timeout: Duration) -> Option<Command> {
        let pending = self.pending.lock().expect("pending");
        pending.as_ref()?.recv_timeout(timeout).ok()
    }

    fn ask(&self, command: Command) {
        if let Some(tx) = self.commands.lock().expect("commands").as_ref() {
            let _ = tx.send(command);
        }
    }
}

fn codec_name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::H265 => "h265",
        VideoCodec::Av1 => "av1",
    }
}

/// The host's view of the Android side.
pub struct AndroidSource(pub Arc<Bridge>);

impl WindowSource for AndroidSource {
    fn list_windows(&self) -> Vec<WindowInfo> {
        self.0.windows.lock().expect("windows").clone()
    }

    fn encoders(&self) -> Vec<VideoCodec> {
        self.0.encoders.lock().expect("encoders").clone()
    }

    fn open_audio(&self, window: WindowId) -> Option<Result<Box<dyn AudioSource>, String>> {
        if !self.list_windows().iter().any(|w| w.id == window) {
            return Some(Err(
                "that app is not on a display of this device's any more".into(),
            ));
        }
        let (tx, samples) = mpsc::sync_channel(SOUND_QUEUED);
        self.0.sounds.lock().expect("sounds").insert(window.0, tx);
        self.0.ask(Command::OpenAudio { window: window.0 });
        Some(Ok(Box::new(AndroidSound {
            bridge: Arc::clone(&self.0),
            window: window.0,
            samples,
        })))
    }

    fn input(&self, event: &InputEvent, focus: Option<WindowId>) {
        let named = match event {
            InputEvent::PointerMove { window, .. }
            | InputEvent::PointerButton { window, .. }
            | InputEvent::PointerScroll { window, .. }
            | InputEvent::Touch { window, .. } => Some(*window),
            InputEvent::Key { .. } | InputEvent::Text { .. } => focus,
            // Gamepads go to WindowSource::gamepads, which this host does not have yet.
            _ => None,
        };
        let (Some(window), Ok(event)) = (named, serde_json::to_value(event)) else {
            return;
        };
        self.0.ask(Command::Input {
            window: window.0,
            event,
        });
    }

    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        if !self.list_windows().iter().any(|w| w.id == window) {
            return Err("that app is not on a display of this device's any more".into());
        }
        if !self.encoders().contains(&codec) {
            return Err(format!("this device cannot encode {codec:?}"));
        }
        let (tx, frames) = mpsc::sync_channel(FRAMES_QUEUED);
        self.0.streams.lock().expect("streams").insert(window.0, tx);
        self.0.ask(Command::Open {
            window: window.0,
            codec: codec_name(codec).into(),
        });
        Ok(Box::new(AndroidStream {
            bridge: Arc::clone(&self.0),
            window: window.0,
            frames,
            codec,
        }))
    }
}

/// One window being encoded on the Android side.
struct AndroidStream {
    bridge: Arc<Bridge>,
    window: u64,
    frames: Receiver<EncodedFrame>,
    codec: VideoCodec,
}

impl FrameSource for AndroidStream {
    fn next_frame(&mut self, keyframe: bool) -> Option<EncodedFrame> {
        if keyframe {
            self.bridge.ask(Command::Keyframe {
                window: self.window,
            });
        }
        self.frames.recv().ok()
    }

    fn describe(&self) -> String {
        format!("Android MediaCodec ({})", codec_name(self.codec))
    }

    fn frame_rate(&self) -> u32 {
        60
    }

    fn set_quality(&mut self, quality: Quality) {
        if let Some(bitrate) = quality.bitrate {
            self.bridge.ask(Command::Bitrate {
                window: self.window,
                bitrate,
            });
        }
    }
}

impl Drop for AndroidStream {
    fn drop(&mut self) {
        self.bridge
            .streams
            .lock()
            .expect("streams")
            .remove(&self.window);
        self.bridge.ask(Command::Close {
            window: self.window,
        });
    }
}

/// One app's sound, captured on the Android side.
struct AndroidSound {
    bridge: Arc<Bridge>,
    window: u64,
    samples: Receiver<Vec<i16>>,
}

impl AudioSource for AndroidSound {
    fn next_samples(&mut self) -> Option<Vec<i16>> {
        match self.samples.recv_timeout(Duration::from_millis(100)) {
            Ok(samples) => Some(samples),
            // Quiet: no samples this time.
            Err(mpsc::RecvTimeoutError::Timeout) => Some(Vec::new()),
            Err(mpsc::RecvTimeoutError::Disconnected) => None,
        }
    }
}

impl Drop for AndroidSound {
    fn drop(&mut self) {
        self.bridge
            .sounds
            .lock()
            .expect("sounds")
            .remove(&self.window);
        self.bridge.ask(Command::CloseAudio {
            window: self.window,
        });
    }
}

/// A running host: the bridge and the runtime serving it.
pub struct AndroidHost {
    bridge: Arc<Bridge>,
    /// The host's windowcast id (hex), which a client pins.
    id: String,
    _runtime: tokio::runtime::Runtime,
}

impl AndroidHost {
    /// Serves this device's app windows on `listen` (`ADDR:PORT`; loopback
    /// unless the person shares this device's apps), with the identity and
    /// trusted devices in `data_dir`. PIN pairing is open only when `pairing`.
    pub fn start(data_dir: &str, listen: &str, pairing: bool) -> std::io::Result<Self> {
        let bridge = Bridge::new();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("windowcast-android-host")
            .build()?;
        let config = windowcast_host::HostConfig {
            listen: listen.to_string(),
            pairing,
            data_dir: data_dir.into(),
        };
        let control = windowcast_host::HostControl::open(&config)?;
        let id = control.peer_id().to_hex();
        let listener = runtime.block_on(tokio::net::TcpListener::bind(listen))?;
        let source = Arc::new(AndroidSource(Arc::clone(&bridge)));
        runtime.spawn(async move {
            if let Err(e) = windowcast_host::serve_with(listener, control, source).await {
                eprintln!("windowcast Android host stopped: {e}");
            }
        });
        Ok(AndroidHost {
            bridge,
            id,
            _runtime: runtime,
        })
    }

    pub fn bridge(&self) -> &Arc<Bridge> {
        &self.bridge
    }

    /// The host's windowcast id, in hex.
    pub fn id(&self) -> &str {
        &self.id
    }
}

// ---- the C interface the Android side's JNI calls -------------------------

/// Copies `text` and a NUL into `out` (capacity `cap`), truncating.
///
/// # Safety
/// `out` must be null or valid for `cap` writable bytes.
unsafe fn write_text(text: &str, out: *mut c_char, cap: usize) -> usize {
    if out.is_null() || cap == 0 {
        return 0;
    }
    let len = text.len().min(cap - 1);
    std::ptr::copy_nonoverlapping(text.as_ptr(), out as *mut u8, len);
    *out.add(len) = 0;
    len
}

/// # Safety
/// `s` must be null or a valid NUL-terminated string.
unsafe fn str_arg<'a>(s: *const c_char) -> Option<&'a str> {
    if s.is_null() {
        None
    } else {
        CStr::from_ptr(s).to_str().ok()
    }
}

/// Starts the host; null on failure, with the reason in `error`.
///
/// # Safety
/// The strings must be valid NUL-terminated strings, and `error` null or
/// valid for `error_cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn windowcast_android_host_start(
    data_dir: *const c_char,
    listen: *const c_char,
    pairing: bool,
    error: *mut c_char,
    error_cap: usize,
) -> *mut AndroidHost {
    let (Some(dir), Some(listen)) = (str_arg(data_dir), str_arg(listen)) else {
        write_text("no data folder or address", error, error_cap);
        return std::ptr::null_mut();
    };
    match AndroidHost::start(dir, listen, pairing) {
        Ok(host) => Box::into_raw(Box::new(host)),
        Err(e) => {
            write_text(&e.to_string(), error, error_cap);
            std::ptr::null_mut()
        }
    }
}

/// The host's windowcast id (hex) in `out`; its length.
///
/// # Safety
/// `host` must be valid and `out` valid for `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn windowcast_android_host_id(
    host: *const AndroidHost,
    out: *mut c_char,
    cap: usize,
) -> i64 {
    match host.as_ref() {
        Some(host) => write_text(host.id(), out, cap) as i64,
        None => 0,
    }
}

/// Stops the host and frees it.
///
/// # Safety
/// `host` must be null or from [`windowcast_android_host_start`], once.
#[no_mangle]
pub unsafe extern "C" fn windowcast_android_host_free(host: *mut AndroidHost) {
    if !host.is_null() {
        drop(Box::from_raw(host));
    }
}

/// The app windows now, as JSON: `[{"id":1,"title":"Notes","package":"…","width":1280,"height":720}]`.
/// 0, or -1 for malformed JSON.
///
/// # Safety
/// `host` must be valid and `json` a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn windowcast_android_host_set_windows(
    host: *const AndroidHost,
    json: *const c_char,
) -> i64 {
    let (Some(host), Some(json)) = (host.as_ref(), str_arg(json)) else {
        return -1;
    };
    match serde_json::from_str::<Vec<ListedWindow>>(json) {
        Ok(listed) => {
            host.bridge.set_windows(&listed);
            0
        }
        Err(_) => -1,
    }
}

/// The codecs the device can encode (1 H.264, 2 H.265, 3 AV1, as the
/// client's C interface numbers them), most preferred first.
///
/// # Safety
/// `host` must be valid and `codecs` valid for `count` values.
#[no_mangle]
pub unsafe extern "C" fn windowcast_android_host_set_encoders(
    host: *const AndroidHost,
    codecs: *const u32,
    count: usize,
) {
    let Some(host) = host.as_ref() else { return };
    if codecs.is_null() {
        return;
    }
    let list = std::slice::from_raw_parts(codecs, count)
        .iter()
        .filter_map(|id| match id {
            1 => Some(VideoCodec::H264),
            2 => Some(VideoCodec::H265),
            3 => Some(VideoCodec::Av1),
            _ => None,
        })
        .collect();
    host.bridge.set_encoders(list);
}

/// One encoded frame of `window` (Annex B or OBUs, as MediaCodec gives
/// them), shown for `duration_us`; `width` and `height` when the picture's
/// size is new, else 0. 1 when it was taken, 0 when nobody streams the
/// window (stop its encoder).
///
/// # Safety
/// `host` must be valid and `data` valid for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn windowcast_android_host_frame(
    host: *const AndroidHost,
    window: u64,
    data: *const u8,
    len: usize,
    duration_us: u64,
    width: u32,
    height: u32,
) -> i64 {
    let Some(host) = host.as_ref() else { return 0 };
    if data.is_null() {
        return 0;
    }
    let frame = EncodedFrame {
        data: std::slice::from_raw_parts(data, len).to_vec(),
        duration: Duration::from_micros(duration_us),
        size: (width > 0 && height > 0).then_some((width, height)),
    };
    i64::from(host.bridge.frame(window, frame))
}

/// `window`'s app is gone: its stream ends.
///
/// # Safety
/// `host` must be valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_android_host_ended(host: *const AndroidHost, window: u64) {
    if let Some(host) = host.as_ref() {
        host.bridge.ended(window);
    }
}

/// `count` interleaved stereo 16-bit samples at 48 kHz of `window`'s app.
/// 1 taken, 0 nobody listens (stop capturing).
///
/// # Safety
/// `host` must be valid and `samples` valid for `count` values.
#[no_mangle]
pub unsafe extern "C" fn windowcast_android_host_sound(
    host: *const AndroidHost,
    window: u64,
    samples: *const i16,
    count: usize,
) -> i64 {
    let Some(host) = host.as_ref() else { return 0 };
    if samples.is_null() {
        return 0;
    }
    let samples = std::slice::from_raw_parts(samples, count).to_vec();
    i64::from(host.bridge.sound(window, samples))
}

/// `window`'s sound cannot be captured: its audio ends, its picture goes on.
///
/// # Safety
/// `host` must be valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_android_host_sound_ended(
    host: *const AndroidHost,
    window: u64,
) {
    if let Some(host) = host.as_ref() {
        host.bridge.sound_ended(window);
    }
}

/// The next command as JSON in `out` (see [`Command`]), waiting up to
/// `timeout_ms`: its length, or 0 when none came.
///
/// # Safety
/// `host` must be valid and `out` valid for `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn windowcast_android_host_next_command(
    host: *const AndroidHost,
    timeout_ms: u32,
    out: *mut c_char,
    cap: usize,
) -> i64 {
    let Some(host) = host.as_ref() else { return 0 };
    match host
        .bridge
        .next_command(Duration::from_millis(u64::from(timeout_ms)))
    {
        Some(command) => {
            let json = serde_json::to_string(&command).expect("commands serialize");
            write_text(&json, out, cap) as i64
        }
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed() -> Vec<ListedWindow> {
        serde_json::from_str(
            r#"[{"id":4,"title":"Notes","package":"org.notes","width":1280,"height":720}]"#,
        )
        .unwrap()
    }

    #[test]
    fn a_window_is_opened_encoded_asked_for_keyframes_and_closed() {
        let bridge = Bridge::new();
        bridge.set_windows(&listed());
        let source = AndroidSource(Arc::clone(&bridge));
        assert_eq!(source.list_windows()[0].app_id, "org.notes");
        assert!(source.open(WindowId(9), VideoCodec::H264).is_err());
        assert!(source.open(WindowId(4), VideoCodec::Av1).is_err());
        // Nobody streams it yet: the Android side is told to stop.
        assert!(!bridge.frame(
            4,
            EncodedFrame {
                data: vec![1],
                duration: Duration::ZERO,
                size: None
            }
        ));

        let mut stream = source.open(WindowId(4), VideoCodec::H264).unwrap();
        let wait = Duration::from_secs(1);
        assert_eq!(
            bridge.next_command(wait),
            Some(Command::Open {
                window: 4,
                codec: "h264".into()
            })
        );
        assert!(bridge.frame(
            4,
            EncodedFrame {
                data: vec![0, 0, 0, 1, 0x65],
                duration: Duration::from_millis(16),
                size: Some((1280, 720))
            }
        ));
        let frame = stream.next_frame(true).unwrap();
        assert_eq!(frame.size, Some((1280, 720)));
        assert_eq!(
            bridge.next_command(wait),
            Some(Command::Keyframe { window: 4 })
        );
        stream.set_quality(Quality {
            bitrate: Some(2_000_000),
            fps: None,
            scale: 1.0,
        });
        assert_eq!(
            bridge.next_command(wait),
            Some(Command::Bitrate {
                window: 4,
                bitrate: 2_000_000
            })
        );
        drop(stream);
        assert_eq!(
            bridge.next_command(wait),
            Some(Command::Close { window: 4 })
        );
        assert!(!bridge.frame(
            4,
            EncodedFrame {
                data: vec![1],
                duration: Duration::ZERO,
                size: None
            }
        ));
    }

    #[test]
    fn an_app_that_goes_away_ends_its_stream() {
        let bridge = Bridge::new();
        bridge.set_windows(&listed());
        let mut stream = AndroidSource(Arc::clone(&bridge))
            .open(WindowId(4), VideoCodec::H264)
            .unwrap();
        bridge.ended(4);
        assert!(stream.next_frame(false).is_none());
    }

    #[test]
    fn input_goes_to_the_window_it_names_or_the_focus() {
        let bridge = Bridge::new();
        let source = AndroidSource(Arc::clone(&bridge));
        let wait = Duration::from_secs(1);
        source.input(
            &InputEvent::PointerMove {
                window: WindowId(4),
                x: 0.5,
                y: 0.25,
            },
            Some(WindowId(9)),
        );
        let Some(Command::Input { window, event }) = bridge.next_command(wait) else {
            panic!("no input")
        };
        assert_eq!(window, 4);
        assert_eq!(
            event.to_string(),
            r#"{"PointerMove":{"window":4,"x":0.5,"y":0.25}}"#
        );
        source.input(
            &InputEvent::Key {
                keycode: 30,
                pressed: true,
            },
            Some(WindowId(9)),
        );
        assert!(matches!(
            bridge.next_command(wait),
            Some(Command::Input { window: 9, .. })
        ));
        // A key with no focus has nowhere to go.
        source.input(
            &InputEvent::Key {
                keycode: 30,
                pressed: true,
            },
            None,
        );
        assert_eq!(bridge.next_command(Duration::from_millis(50)), None);
    }

    #[test]
    fn an_apps_sound_is_asked_for_carried_and_let_go() {
        let bridge = Bridge::new();
        bridge.set_windows(&listed());
        let source = AndroidSource(Arc::clone(&bridge));
        let wait = Duration::from_secs(1);
        let mut sound = source.open_audio(WindowId(4)).unwrap().unwrap();
        assert_eq!(
            bridge.next_command(wait),
            Some(Command::OpenAudio { window: 4 })
        );
        assert!(bridge.sound(4, vec![1, -1, 2, -2]));
        assert_eq!(sound.next_samples(), Some(vec![1, -1, 2, -2]));
        // Quiet is no samples, not the end.
        assert_eq!(sound.next_samples(), Some(Vec::new()));
        drop(sound);
        assert_eq!(
            bridge.next_command(wait),
            Some(Command::CloseAudio { window: 4 })
        );
        assert!(!bridge.sound(4, vec![0, 0]));
        // An app whose sound cannot be captured ends its audio only.
        let mut again = source.open_audio(WindowId(4)).unwrap().unwrap();
        bridge.sound_ended(4);
        assert_eq!(again.next_samples(), None);
    }

    #[test]
    fn commands_are_json_the_android_side_reads() {
        let json = serde_json::to_string(&Command::Open {
            window: 4,
            codec: "h264".into(),
        })
        .unwrap();
        assert_eq!(json, r#"{"command":"open","window":4,"codec":"h264"}"#);
    }
}

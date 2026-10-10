//! The C interface (`include/windowcast.h`): a thin layer over [`Client`]
//! and [`ClientSession`]. Every call is blocking and safe to make from any
//! thread; nothing calls back into the embedder.
//!
//! Strings are UTF-8 and NUL-terminated. Output buffers come with their
//! capacity; a call that needs more room returns
//! [`WINDOWCAST_BUFFER_TOO_SMALL`] and reports the size it needs, and the
//! data stays queued for the next call.

use std::ffi::{c_char, CStr};
use std::path::Path;
use std::time::Duration;

use windowcast_protocol::{VideoCodec, WindowId};

use windowcast_accounts::oidc::BrowserSignIn;
use windowcast_protocol::OidcProviderInfo;

use crate::{Client, ClientSession, FramePoll, SignIn};

pub const WINDOWCAST_TIMEOUT: i64 = 0;
pub const WINDOWCAST_ENDED: i64 = -1;
pub const WINDOWCAST_BUFFER_TOO_SMALL: i64 = -2;
pub const WINDOWCAST_ERROR: i64 = -3;

pub const WINDOWCAST_CODEC_H264: u32 = 0;
pub const WINDOWCAST_CODEC_H265: u32 = 1;
pub const WINDOWCAST_CODEC_AV1: u32 = 2;

/// Describes the frame [`windowcast_session_next_frame`] returned.
#[repr(C)]
pub struct WindowcastFrameInfo {
    /// One of the `WINDOWCAST_CODEC_*` values.
    pub codec: u32,
    /// Non-zero for a keyframe.
    pub keyframe: u32,
    /// RTP timestamp, 90 kHz clock.
    pub rtp_timestamp: u32,
    /// The frame's size in bytes (also when the buffer was too small).
    pub size: u64,
}

fn codec_id(codec: VideoCodec) -> u32 {
    match codec {
        VideoCodec::H264 => WINDOWCAST_CODEC_H264,
        VideoCodec::H265 => WINDOWCAST_CODEC_H265,
        VideoCodec::Av1 => WINDOWCAST_CODEC_AV1,
    }
}

fn codec_from_id(id: u32) -> Option<VideoCodec> {
    match id {
        WINDOWCAST_CODEC_H264 => Some(VideoCodec::H264),
        WINDOWCAST_CODEC_H265 => Some(VideoCodec::H265),
        WINDOWCAST_CODEC_AV1 => Some(VideoCodec::Av1),
        _ => None,
    }
}

/// # Safety
/// `s` must be null or a valid NUL-terminated string.
pub(crate) unsafe fn str_arg<'a>(s: *const c_char) -> Option<&'a str> {
    if s.is_null() {
        None
    } else {
        CStr::from_ptr(s).to_str().ok()
    }
}

/// Copies `text` and a NUL into `out` (capacity `cap`), truncating.
///
/// # Safety
/// `out` must be null or valid for `cap` writable bytes.
pub(crate) unsafe fn write_text(text: &str, out: *mut c_char, cap: usize) {
    if out.is_null() || cap == 0 {
        return;
    }
    let len = text.len().min(cap - 1);
    std::ptr::copy_nonoverlapping(text.as_ptr(), out as *mut u8, len);
    *out.add(len) = 0;
}

/// Opens the client identity stored in `data_dir` (created on first use).
/// Returns null on failure.
///
/// # Safety
/// `data_dir` must be a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn windowcast_client_new(data_dir: *const c_char) -> *mut Client {
    let Some(dir) = str_arg(data_dir) else {
        return std::ptr::null_mut();
    };
    match Client::new(Path::new(dir)) {
        Ok(client) => Box::into_raw(Box::new(client)),
        Err(_) => std::ptr::null_mut(),
    }
}

/// # Safety
/// `client` must come from [`windowcast_client_new`], not yet freed, with
/// every session from it freed first.
#[no_mangle]
pub unsafe extern "C" fn windowcast_client_free(client: *mut Client) {
    if !client.is_null() {
        drop(Box::from_raw(client));
    }
}

/// Writes this client's identity (64 hex digits) into `out`.
///
/// # Safety
/// `client` must be valid; `out` valid for `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn windowcast_client_peer_id(
    client: *const Client,
    out: *mut c_char,
    cap: usize,
) {
    if let Some(client) = client.as_ref() {
        write_text(&client.peer_id(), out, cap);
    }
}

/// Connects to the host agent at `address` (`HOST:PORT`), pairing with
/// `pin` the first time (pass null to resume with the pinned identity).
/// Blocks until connected. Returns null on failure, with the reason in
/// `error` (capacity `error_cap`).
///
/// # Safety
/// `client` must be valid; strings NUL-terminated (`pin` may be null);
/// `error` null or valid for `error_cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn windowcast_connect(
    client: *const Client,
    address: *const c_char,
    pin: *const c_char,
    error: *mut c_char,
    error_cap: usize,
) -> *mut ClientSession {
    let (Some(client), Some(address)) = (client.as_ref(), str_arg(address)) else {
        write_text("invalid arguments", error, error_cap);
        return std::ptr::null_mut();
    };
    match client.connect(address, str_arg(pin)) {
        Ok(session) => Box::into_raw(Box::new(session)),
        Err(e) => {
            write_text(&e.to_string(), error, error_cap);
            std::ptr::null_mut()
        }
    }
}

/// Connects to a paired host away from the LAN, by its identity (64 hex
/// digits), through Syncthing's global discovery and STUN with their
/// default servers; it must have told this client its discovery ID on an
/// earlier session. Blocks, up to a minute and a half. Returns null on
/// failure, with the reason in `error`.
///
/// # Safety
/// As [`windowcast_connect`].
#[no_mangle]
pub unsafe extern "C" fn windowcast_connect_away(
    client: *const Client,
    host_id: *const c_char,
    error: *mut c_char,
    error_cap: usize,
) -> *mut ClientSession {
    let (Some(client), Some(host_id)) = (client.as_ref(), str_arg(host_id)) else {
        write_text("invalid arguments", error, error_cap);
        return std::ptr::null_mut();
    };
    let config = windowcast_transport::remote::RemoteConfig::default();
    match client.connect_away(host_id, &config) {
        Ok(session) => Box::into_raw(Box::new(session)),
        Err(e) => {
            write_text(&e.to_string(), error, error_cap);
            std::ptr::null_mut()
        }
    }
}

/// Closes the session (the host is told at once) and frees it.
///
/// # Safety
/// `session` must come from [`windowcast_connect`] and not be in use by
/// another thread.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_free(session: *mut ClientSession) {
    if !session.is_null() {
        drop(Box::from_raw(session));
    }
}

/// Writes the host's identity into `out`. Returns 1 if this connection
/// paired by PIN, 0 if it resumed.
///
/// # Safety
/// `session` must be valid; `out` valid for `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_host(
    session: *const ClientSession,
    out: *mut c_char,
    cap: usize,
) -> i32 {
    match session.as_ref() {
        Some(session) => {
            write_text(session.host_id(), out, cap);
            i32::from(session.paired())
        }
        None => 0,
    }
}

/// Asks for the window list (arrives as a `windows` event). Returns 0, or
/// [`WINDOWCAST_ERROR`] if the session is gone.
///
/// # Safety
/// `session` must be valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_request_windows(session: *const ClientSession) -> i64 {
    match session.as_ref().map(ClientSession::request_windows) {
        Some(Ok(())) => 0,
        _ => WINDOWCAST_ERROR,
    }
}

/// Asks to stream `window`, decodable in the `count` codecs at `codecs`
/// (`WINDOWCAST_CODEC_*`, most preferred first). The answer arrives as a
/// `stream_started` or `stream_refused` event.
///
/// # Safety
/// `session` must be valid; `codecs` valid for `count` values.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_start_window(
    session: *const ClientSession,
    window: u64,
    codecs: *const u32,
    count: usize,
) -> i64 {
    let Some(session) = session.as_ref() else {
        return WINDOWCAST_ERROR;
    };
    let codecs: Vec<VideoCodec> = if codecs.is_null() {
        Vec::new()
    } else {
        std::slice::from_raw_parts(codecs, count)
            .iter()
            .filter_map(|id| codec_from_id(*id))
            .collect()
    };
    match session.start_window(WindowId(window), &codecs) {
        Ok(()) => 0,
        Err(_) => WINDOWCAST_ERROR,
    }
}

/// # Safety
/// `session` must be valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_stop_window(
    session: *const ClientSession,
    window: u64,
) -> i64 {
    match session.as_ref().map(|s| s.stop_window(WindowId(window))) {
        Some(Ok(())) => 0,
        _ => WINDOWCAST_ERROR,
    }
}

/// Sends one input event, given as JSON in serde's form of
/// `windowcast_protocol::InputEvent`, e.g.
/// `{"PointerMove":{"window":7,"x":0.5,"y":0.25}}`,
/// `{"PointerButton":{"window":7,"button":"Left","pressed":true}}`,
/// `{"Key":{"keycode":30,"pressed":true}}` (evdev keycodes),
/// `{"Text":{"text":"é"}}`,
/// `{"Touch":{"window":7,"id":0,"x":0.5,"y":0.5,"phase":"Start"}}`,
/// `{"Gamepad":{"pad":0,"state":{"buttons":4096,"left_x":0,"left_y":0,"right_x":0,"right_y":0,"left_trigger":0,"right_trigger":0}}}`.
/// Returns 0, or [`WINDOWCAST_ERROR`] for malformed JSON or a gone session.
///
/// # Safety
/// `session` must be valid; `json` a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_send_input(
    session: *const ClientSession,
    json: *const c_char,
) -> i64 {
    let (Some(session), Some(json)) = (session.as_ref(), str_arg(json)) else {
        return WINDOWCAST_ERROR;
    };
    match serde_json::from_str(json).map(|event| session.send_input(event)) {
        Ok(Ok(())) => 0,
        _ => WINDOWCAST_ERROR,
    }
}

/// Gives the host this client's clipboard text. The host's own clipboard
/// changes arrive as `clipboard` events.
///
/// # Safety
/// `session` must be valid; `text` a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_set_clipboard(
    session: *const ClientSession,
    text: *const c_char,
) -> i64 {
    match (session.as_ref(), str_arg(text)) {
        (Some(session), Some(text)) if session.set_clipboard(text).is_ok() => 0,
        _ => WINDOWCAST_ERROR,
    }
}

/// Waits up to `timeout_ms` for the next session event and writes it as
/// JSON (`{"type": ...}`, see [`Event`]) into `out`. Returns its length,
/// [`WINDOWCAST_TIMEOUT`], or [`WINDOWCAST_BUFFER_TOO_SMALL`] with the
/// length needed in `needed` (the event is then lost: give room for a few
/// kilobytes). A `closed` event is the last.
///
/// # Safety
/// `session` must be valid; `out` valid for `cap` bytes; `needed` null or
/// valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_next_event(
    session: *const ClientSession,
    timeout_ms: u32,
    out: *mut u8,
    cap: usize,
    needed: *mut usize,
) -> i64 {
    let Some(session) = session.as_ref() else {
        return WINDOWCAST_ERROR;
    };
    let Some(event) = session.next_event(Duration::from_millis(u64::from(timeout_ms))) else {
        return WINDOWCAST_TIMEOUT;
    };
    let json = serde_json::to_vec(&event).unwrap_or_else(|_| br#"{"type":"closed"}"#.to_vec());
    if !needed.is_null() {
        *needed = json.len();
    }
    if json.len() > cap || out.is_null() {
        return WINDOWCAST_BUFFER_TOO_SMALL;
    }
    std::ptr::copy_nonoverlapping(json.as_ptr(), out, json.len());
    json.len() as i64
}

/// Waits up to `timeout_ms` for the next frame of `window` and copies it
/// into `out`. Returns its length, [`WINDOWCAST_TIMEOUT`],
/// [`WINDOWCAST_ENDED`] once the window's stream is over, or
/// [`WINDOWCAST_BUFFER_TOO_SMALL`] (the frame stays queued; `info.size`
/// says how much room it needs). Call it from the thread that feeds the
/// decoder.
///
/// # Safety
/// `session` must be valid; `out` valid for `cap` bytes; `info` valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_next_frame(
    session: *const ClientSession,
    window: u64,
    timeout_ms: u32,
    out: *mut u8,
    cap: usize,
    info: *mut WindowcastFrameInfo,
) -> i64 {
    let (Some(session), Some(info)) = (session.as_ref(), info.as_mut()) else {
        return WINDOWCAST_ERROR;
    };
    let window = WindowId(window);
    let frame = match session.next_frame(window, Duration::from_millis(u64::from(timeout_ms))) {
        FramePoll::Frame(frame) => frame,
        FramePoll::Timeout => return WINDOWCAST_TIMEOUT,
        FramePoll::Ended => return WINDOWCAST_ENDED,
    };
    *info = WindowcastFrameInfo {
        codec: codec_id(frame.codec),
        keyframe: u32::from(frame.keyframe),
        rtp_timestamp: frame.rtp_timestamp,
        size: frame.data.len() as u64,
    };
    if frame.data.len() > cap || out.is_null() {
        session.hold_frame(window, frame);
        return WINDOWCAST_BUFFER_TOO_SMALL;
    }
    std::ptr::copy_nonoverlapping(frame.data.as_ptr(), out, frame.data.len());
    frame.data.len() as i64
}

/// Says this client shows RGBA pictures (non-zero) or not (0, the
/// default): only then are windows its rules send to RDP streamed over
/// RDP. Returns 0 or WINDOWCAST_ERROR.
///
/// # Safety
/// `session` must be valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_accept_pictures(
    session: *const ClientSession,
    on: u32,
) -> i64 {
    let Some(session) = session.as_ref() else {
        return WINDOWCAST_ERROR;
    };
    session.accept_pictures(on != 0);
    0
}

/// Waits up to `timeout_ms` for the next picture of a window streamed over
/// RDP (a "stream_started" event with backend "Rdp") and copies it into
/// `out`: RGBA, rows from the top, `width * 4` bytes each. Returns its
/// length, [`WINDOWCAST_TIMEOUT`] (the window has not changed),
/// [`WINDOWCAST_ENDED`], or [`WINDOWCAST_BUFFER_TOO_SMALL`] (the picture
/// stays queued; `width` and `height` say its size).
///
/// # Safety
/// `session` must be valid; `out` valid for `cap` bytes; `width` and
/// `height` valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_next_picture(
    session: *const ClientSession,
    window: u64,
    timeout_ms: u32,
    out: *mut u8,
    cap: usize,
    width: *mut u32,
    height: *mut u32,
) -> i64 {
    let (Some(session), Some(width), Some(height)) =
        (session.as_ref(), width.as_mut(), height.as_mut())
    else {
        return WINDOWCAST_ERROR;
    };
    let window = WindowId(window);
    let picture = match session.next_picture(window, Duration::from_millis(u64::from(timeout_ms))) {
        crate::PicturePoll::Picture(picture) => picture,
        crate::PicturePoll::Timeout => return WINDOWCAST_TIMEOUT,
        crate::PicturePoll::Ended => return WINDOWCAST_ENDED,
    };
    *width = picture.width;
    *height = picture.height;
    if picture.data.len() > cap || out.is_null() {
        session.hold_picture(window, picture);
        return WINDOWCAST_BUFFER_TOO_SMALL;
    }
    std::ptr::copy_nonoverlapping(picture.data.as_ptr(), out, picture.data.len());
    picture.data.len() as i64
}

/// 0 for success, WINDOWCAST_ERROR for a failure.
fn status<T, E>(result: Result<T, E>) -> i64 {
    match result {
        Ok(_) => 0,
        Err(_) => WINDOWCAST_ERROR,
    }
}

/// Starts sending this client's microphone to the host. Returns 0 or
/// WINDOWCAST_ERROR.
///
/// # Safety
/// `session` must be valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_start_microphone(session: *const ClientSession) -> i64 {
    let Some(session) = session.as_ref() else {
        return WINDOWCAST_ERROR;
    };
    status(session.start_microphone())
}

/// Sends microphone sound: `count` interleaved stereo 16-bit samples at
/// 48 kHz (any amount; encoded to Opus here). Returns 0 or
/// WINDOWCAST_ERROR.
///
/// # Safety
/// `session` must be valid; `samples` valid for `count` samples.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_send_microphone(
    session: *const ClientSession,
    samples: *const i16,
    count: usize,
) -> i64 {
    let Some(session) = session.as_ref() else {
        return WINDOWCAST_ERROR;
    };
    if samples.is_null() {
        return WINDOWCAST_ERROR;
    }
    status(session.send_microphone(std::slice::from_raw_parts(samples, count)))
}

/// Sets this client's ceilings for a window's stream (kept for its next
/// start, sent at once to a running one); 0 means no limit. The host adapts
/// below them; its reports arrive as `stream_quality` events. Returns 0 or
/// WINDOWCAST_ERROR.
///
/// # Safety
/// `session` must be valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_set_stream_limits(
    session: *const ClientSession,
    window: u64,
    max_bitrate_kbps: u32,
    max_fps: u32,
    max_height: u32,
) -> i64 {
    let Some(session) = session.as_ref() else {
        return WINDOWCAST_ERROR;
    };
    let some = |n: u32| (n > 0).then_some(n);
    status(session.set_stream_limits(
        WindowId(window),
        windowcast_protocol::StreamLimits {
            max_bitrate_kbps: some(max_bitrate_kbps),
            max_fps: some(max_fps),
            max_height: some(max_height),
        },
    ))
}

/// Stops sending the microphone. Returns 0 or WINDOWCAST_ERROR.
///
/// # Safety
/// `session` must be valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_stop_microphone(session: *const ClientSession) -> i64 {
    let Some(session) = session.as_ref() else {
        return WINDOWCAST_ERROR;
    };
    status(session.stop_microphone())
}

/// Next Opus packet (48 kHz, stereo, 20 ms) of a window's sound. Returns
/// its length, WINDOWCAST_TIMEOUT (quiet, or no audio yet),
/// WINDOWCAST_ENDED when the window's audio is over, or
/// WINDOWCAST_BUFFER_TOO_SMALL (the packet is dropped; Opus packets are
/// under 1500 bytes). `rtp_timestamp` gets the packet's 48 kHz timestamp.
///
/// # Safety
/// `session` must be valid; `out` valid for `cap` bytes; `rtp_timestamp`
/// null or valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_next_audio(
    session: *const ClientSession,
    window: u64,
    timeout_ms: u32,
    out: *mut u8,
    cap: usize,
    rtp_timestamp: *mut u32,
) -> i64 {
    let Some(session) = session.as_ref() else {
        return WINDOWCAST_ERROR;
    };
    let packet = match session.next_audio(
        WindowId(window),
        Duration::from_millis(u64::from(timeout_ms)),
    ) {
        crate::AudioPoll::Packet(packet) => packet,
        crate::AudioPoll::Timeout => return WINDOWCAST_TIMEOUT,
        crate::AudioPoll::Ended => return WINDOWCAST_ENDED,
    };
    if let Some(timestamp) = rtp_timestamp.as_mut() {
        *timestamp = packet.rtp_timestamp;
    }
    if packet.data.len() > cap || out.is_null() {
        return WINDOWCAST_BUFFER_TOO_SMALL;
    }
    std::ptr::copy_nonoverlapping(packet.data.as_ptr(), out, packet.data.len());
    packet.data.len() as i64
}

/// Asks the host at `address` which account sign-ins it takes, without
/// signing in, and writes them into `out` as JSON (`host_id`,
/// `fingerprint`, `trusted`, `methods`, `providers`, `kerberos_service`).
/// Returns the JSON's length, [`WINDOWCAST_BUFFER_TOO_SMALL`], or
/// [`WINDOWCAST_ERROR`] with the reason in `out`.
///
/// # Safety
/// `client` must be valid; `address` NUL-terminated; `out` valid for `cap`
/// bytes.
#[no_mangle]
pub unsafe extern "C" fn windowcast_sign_in_options(
    client: *const Client,
    address: *const c_char,
    out: *mut c_char,
    cap: usize,
) -> i64 {
    let (Some(client), Some(address)) = (client.as_ref(), str_arg(address)) else {
        write_text("invalid arguments", out, cap);
        return WINDOWCAST_ERROR;
    };
    match client.sign_in_options(address) {
        Ok(options) => {
            let json = serde_json::to_string(&options).unwrap_or_default();
            if json.len() >= cap {
                return WINDOWCAST_BUFFER_TOO_SMALL;
            }
            write_text(&json, out, cap);
            json.len() as i64
        }
        Err(e) => {
            write_text(&e.to_string(), out, cap);
            WINDOWCAST_ERROR
        }
    }
}

/// Signs in to the host at `address` with an account and connects.
/// `sign_in` is JSON: `{"password":{"username":"..","password":".."}}`,
/// `{"oidc":{"provider":"..","id_token":".."}}` or `"kerberos"`. The
/// credential goes only to a host this client trusts or whose identity
/// (64 hex digits) the user confirmed as `accept_host` (may be null);
/// otherwise this fails with an error naming the host's identity. Returns
/// null on failure, with the reason in `error`.
///
/// # Safety
/// As [`windowcast_connect`]; `accept_host` may be null.
#[no_mangle]
pub unsafe extern "C" fn windowcast_connect_account(
    client: *const Client,
    address: *const c_char,
    sign_in: *const c_char,
    accept_host: *const c_char,
    error: *mut c_char,
    error_cap: usize,
) -> *mut ClientSession {
    let (Some(client), Some(address), Some(sign_in)) =
        (client.as_ref(), str_arg(address), str_arg(sign_in))
    else {
        write_text("invalid arguments", error, error_cap);
        return std::ptr::null_mut();
    };
    let sign_in: SignIn = match serde_json::from_str(sign_in) {
        Ok(sign_in) => sign_in,
        Err(e) => {
            write_text(&format!("sign-in: {e}"), error, error_cap);
            return std::ptr::null_mut();
        }
    };
    match client.connect_account(address, &sign_in, str_arg(accept_host)) {
        Ok(session) => Box::into_raw(Box::new(session)),
        Err(e) => {
            write_text(&e.to_string(), error, error_cap);
            std::ptr::null_mut()
        }
    }
}

/// Starts signing in with an OpenID Connect provider in the user's
/// browser. `provider` is one of the providers
/// [`windowcast_sign_in_options`] listed, as JSON. Writes the page to open
/// into `url` and returns a handle for [`windowcast_oidc_browser_finish`];
/// null on failure, with the reason in `url`.
///
/// # Safety
/// `client` must be valid; `provider` NUL-terminated; `url` valid for
/// `url_cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn windowcast_oidc_browser_start(
    client: *const Client,
    provider: *const c_char,
    url: *mut c_char,
    url_cap: usize,
) -> *mut BrowserSignIn {
    let (Some(client), Some(provider)) = (client.as_ref(), str_arg(provider)) else {
        write_text("invalid arguments", url, url_cap);
        return std::ptr::null_mut();
    };
    let provider: OidcProviderInfo = match serde_json::from_str(provider) {
        Ok(provider) => provider,
        Err(e) => {
            write_text(&format!("provider: {e}"), url, url_cap);
            return std::ptr::null_mut();
        }
    };
    match client.oidc_browser(&provider) {
        Ok(sign_in) if sign_in.url().len() < url_cap => {
            write_text(sign_in.url(), url, url_cap);
            Box::into_raw(Box::new(sign_in))
        }
        Ok(_) => {
            write_text("the URL does not fit", url, url_cap);
            std::ptr::null_mut()
        }
        Err(e) => {
            write_text(&e.to_string(), url, url_cap);
            std::ptr::null_mut()
        }
    }
}

/// Waits up to `timeout_ms` for the browser to come back and writes the ID
/// token into `token` (for the `oidc` sign-in). Frees `sign_in` whatever
/// happens. Returns the token's length, [`WINDOWCAST_BUFFER_TOO_SMALL`],
/// or [`WINDOWCAST_ERROR`] with the reason in `token`.
///
/// # Safety
/// `sign_in` must come from [`windowcast_oidc_browser_start`] and not be
/// used again; `token` valid for `token_cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn windowcast_oidc_browser_finish(
    sign_in: *mut BrowserSignIn,
    timeout_ms: u32,
    token: *mut c_char,
    token_cap: usize,
) -> i64 {
    if sign_in.is_null() {
        write_text("invalid arguments", token, token_cap);
        return WINDOWCAST_ERROR;
    }
    let sign_in = Box::from_raw(sign_in);
    match sign_in.finish(Duration::from_millis(u64::from(timeout_ms))) {
        Ok(id_token) if id_token.len() < token_cap => {
            write_text(&id_token, token, token_cap);
            id_token.len() as i64
        }
        Ok(_) => WINDOWCAST_BUFFER_TOO_SMALL,
        Err(e) => {
            write_text(&e.to_string(), token, token_cap);
            WINDOWCAST_ERROR
        }
    }
}

/// Asks the host for an SSH user certificate for `public_key` (an OpenSSH
/// public key line), for the account this client signed in with; it
/// arrives as an `ssh_certificate` event. Returns 0, or
/// [`WINDOWCAST_ERROR`] if the session is gone.
///
/// # Safety
/// `session` must be valid; `public_key` NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_request_ssh_certificate(
    session: *const ClientSession,
    public_key: *const c_char,
) -> i64 {
    match (session.as_ref(), str_arg(public_key)) {
        (Some(session), Some(key)) => match session.request_ssh_certificate(key) {
            Ok(()) => 0,
            Err(_) => WINDOWCAST_ERROR,
        },
        _ => WINDOWCAST_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Event;

    #[test]
    fn events_serialize_with_a_type_tag() {
        let json = serde_json::to_string(&Event::StreamStopped { window: 4 }).unwrap();
        assert_eq!(json, r#"{"type":"stream_stopped","window":4}"#);
        let json = serde_json::to_string(&Event::Closed).unwrap();
        assert_eq!(json, r#"{"type":"closed"}"#);
    }

    #[test]
    fn input_json_is_the_documented_form() {
        use windowcast_protocol::{InputEvent, PointerButton, WindowId};
        let event: InputEvent = serde_json::from_str(
            r#"{"PointerButton":{"window":7,"button":"Left","pressed":true}}"#,
        )
        .unwrap();
        assert_eq!(
            event,
            InputEvent::PointerButton {
                window: WindowId(7),
                button: PointerButton::Left,
                pressed: true
            }
        );
        let event: InputEvent = serde_json::from_str(
            r#"{"Gamepad":{"pad":0,"state":{"buttons":4096,"left_x":0,"left_y":0,"right_x":0,"right_y":0,"left_trigger":0,"right_trigger":0}}}"#,
        )
        .unwrap();
        assert!(matches!(event, InputEvent::Gamepad { pad: 0, .. }));
    }

    #[test]
    fn codec_ids_round_trip() {
        for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
            assert_eq!(codec_from_id(codec_id(codec)), Some(codec));
        }
        assert_eq!(codec_from_id(9), None);
    }

    #[test]
    fn text_is_truncated_and_terminated() {
        let mut buf = [0x7f as c_char; 4];
        unsafe { write_text("abcdef", buf.as_mut_ptr(), buf.len()) };
        assert_eq!(buf, [b'a' as c_char, b'b' as c_char, b'c' as c_char, 0]);
    }
}

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

use crate::{Client, ClientSession, FramePoll};

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
unsafe fn str_arg<'a>(s: *const c_char) -> Option<&'a str> {
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
unsafe fn write_text(text: &str, out: *mut c_char, cap: usize) {
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

//! Reusable client SDK, exposed as a small C ABI so any embedder (droidtop
//! via JNI, a future desktop client, a VR runtime) can link against it
//! without depending on Rust directly. Deliberately minimal today: it
//! stands up a session and exposes the raw control-channel + fingerprint
//! primitives pairing needs, but does NOT yet do full SDP offer/answer
//! signaling, per-window video-track attach, or decoded-frame delivery —
//! those are real, separately-scoped follow-up work (see the project
//! plan's v1 phasing), not silently missing pieces pretending to be done.

use std::ffi::c_void;
use std::os::raw::c_int;
use std::sync::Arc;

use tokio::runtime::Runtime;
use windowcast_transport::Session;

pub struct WindowcastClient {
    // Kept alive for as long as the client exists — dropping it would shut
    // down the tokio runtime backing `session`'s background tasks. Never
    // read directly, so it needs an explicit allow rather than looking
    // like an oversight.
    #[allow(dead_code)]
    runtime: Runtime,
    session: Arc<Session>,
}

/// Creates a new client session (spins up its own single-threaded tokio
/// runtime — embedders don't need their own async runtime just to use
/// this). Returns null on failure. Caller owns the returned pointer and
/// must pass it to [`windowcast_client_free`] exactly once.
#[no_mangle]
pub extern "C" fn windowcast_client_new() -> *mut WindowcastClient {
    let runtime = match Runtime::new() {
        Ok(rt) => rt,
        Err(_) => return std::ptr::null_mut(),
    };
    let session = match runtime.block_on(Session::new()) {
        Ok(session) => Arc::new(session),
        Err(_) => return std::ptr::null_mut(),
    };
    Box::into_raw(Box::new(WindowcastClient { runtime, session }))
}

/// # Safety
/// `client` must be a pointer previously returned by
/// [`windowcast_client_new`] and not yet freed.
#[no_mangle]
pub unsafe extern "C" fn windowcast_client_free(client: *mut WindowcastClient) {
    if !client.is_null() {
        drop(Box::from_raw(client));
    }
}

/// Copies this session's local DTLS fingerprint into `out_buf` (capacity
/// `out_buf_len`). Returns the fingerprint's actual length, or -1 on
/// error/if `out_buf` is too small to hold it. Callers feed this into
/// `windowcast-pairing`'s fingerprint authentication before trusting the
/// connection — see the pairing crate for why that step is not optional.
///
/// # Safety
/// `client` must be a valid, non-null pointer from
/// [`windowcast_client_new`]. `out_buf` must be valid for `out_buf_len`
/// writable bytes.
#[no_mangle]
pub unsafe extern "C" fn windowcast_client_local_fingerprint(
    client: *const WindowcastClient,
    out_buf: *mut u8,
    out_buf_len: usize,
) -> c_int {
    let client = match client.as_ref() {
        Some(c) => c,
        None => return -1,
    };
    let fingerprint = match client.session.local_dtls_fingerprint() {
        Ok(f) => f,
        Err(_) => return -1,
    };
    if fingerprint.len() > out_buf_len {
        return -1;
    }
    std::ptr::copy_nonoverlapping(fingerprint.as_ptr(), out_buf, fingerprint.len());
    fingerprint.len() as c_int
}

/// Opaque placeholder for a future frame-delivery callback registration —
/// intentionally not implemented yet (see module docs). Present so the
/// FFI surface's eventual shape is visible in the header/bindings without
/// pretending frame delivery already works.
pub type FrameCallback =
    extern "C" fn(user_data: *mut c_void, window_id: u64, data: *const u8, len: usize);

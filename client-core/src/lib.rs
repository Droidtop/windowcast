//! Reusable client SDK, exposed as a small C ABI so any embedder (droidtop
//! via JNI, a future desktop client, a VR runtime) can link against it
//! without depending on Rust directly. Deliberately minimal today: it
//! stands up a session and nothing more. The transport
//! underneath already signals, pairs and receives per-window frames
//! (`windowcast_transport::signaling`, `RemoteWindow`); exposing those
//! through this C ABI, and decoding the frames, is the next piece of work,
//! not something silently missing.

use std::ffi::c_void;
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
    // Held for the client's lifetime; nothing reads it until the C ABI
    // grows connect and frame delivery.
    _session: Arc<Session>,
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
    Box::into_raw(Box::new(WindowcastClient {
        runtime,
        _session: session,
    }))
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

/// Opaque placeholder for a future frame-delivery callback registration —
/// intentionally not implemented yet (see module docs). Present so the
/// FFI surface's eventual shape is visible in the header/bindings without
/// pretending frame delivery already works.
pub type FrameCallback =
    extern "C" fn(user_data: *mut c_void, window_id: u64, data: *const u8, len: usize);

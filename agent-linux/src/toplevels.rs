//! Enumerates open top-level windows via
//! `zwlr_foreign_toplevel_manager_v1` (already vendored as a protocol
//! concept droidtop's own `host-bridge` module uses for local toplevel
//! listing/control — this is the same protocol, used remotely here).
//!
//! Capture of a *specific* toplevel's pixel content is a separate,
//! not-yet-implemented piece (see `capture.rs`) — this module only
//! answers "what windows exist and what are they called," which is
//! everything `ControlMessage::ListWindowsResponse` needs.

use std::collections::HashMap;

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry;
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::{
    self, ZwlrForeignToplevelHandleV1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::{
    self, ZwlrForeignToplevelManagerV1,
};

use windowcast_protocol::{WindowId, WindowInfo};

#[derive(Debug, thiserror::Error)]
pub enum ToplevelError {
    #[error("wayland connection error: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    #[error("wayland dispatch error: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    #[error("compositor does not support zwlr_foreign_toplevel_manager_v1")]
    ManagerUnavailable,
    #[error("failed to enumerate compositor globals: {0}")]
    Globals(#[from] wayland_client::globals::GlobalError),
}

#[derive(Default, Clone)]
struct ToplevelState {
    title: String,
    app_id: String,
    focused: bool,
}

struct AppState {
    toplevels: HashMap<u32, ToplevelState>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
            state
                .toplevels
                .insert(toplevel.id().protocol_id(), ToplevelState::default());
        }
    }

    event_created_child!(AppState, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let id = proxy.id().protocol_id();
        let entry = state.toplevels.entry(id).or_default();
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => entry.title = title,
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => entry.app_id = app_id,
            zwlr_foreign_toplevel_handle_v1::Event::State { state: states } => {
                entry.focused = states.chunks(4).any(|chunk| {
                    u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
                        == zwlr_foreign_toplevel_handle_v1::State::Activated as u32
                });
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                state.toplevels.remove(&id);
            }
            _ => {}
        }
    }
}

/// Connects to the compositor named by `WAYLAND_DISPLAY` (or the default
/// socket), enumerates its current top-level windows, and disconnects.
/// One-shot, not a live subscription — a real agent would keep the
/// connection open and re-list on `Toplevel`/`Closed` events instead of
/// reconnecting per request, which is real follow-up work once this is
/// wired into the actual session-serving loop.
pub fn list_windows() -> Result<Vec<WindowInfo>, ToplevelError> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<AppState>(&conn)?;
    let qh = queue.handle();

    let manager: ZwlrForeignToplevelManagerV1 = globals
        .bind(&qh, 1..=3, ())
        .map_err(|_| ToplevelError::ManagerUnavailable)?;
    let _ = manager;

    let mut state = AppState {
        toplevels: HashMap::new(),
    };
    // Two round-trips: one to receive `toplevel` creation events, one more
    // for the title/app_id/state events each freshly-created handle sends.
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;

    Ok(state
        .toplevels
        .into_iter()
        .map(|(id, info)| WindowInfo {
            id: WindowId(id as u64),
            title: info.title,
            app_id: info.app_id,
            // Real per-window size isn't exposed by foreign-toplevel-management
            // itself (it only reports outputs a toplevel spans, not pixel
            // size) — left 0 until capture.rs reports actual captured
            // dimensions instead of hardcoding a guess.
            width: 0,
            height: 0,
            focused: info.focused,
        })
        .collect())
}

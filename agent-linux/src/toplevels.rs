//! The compositor's top-level windows, from `ext_foreign_toplevel_list_v1`:
//! title, app id, and a stable identifier the capture side
//! (ext-image-copy-capture's toplevel source) and sway's IPC both name a
//! window by. A window's id is a hash of that identifier, so it is the
//! same on every connection. Compositors without the list fall back to
//! `zwlr_foreign_toplevel_manager_v1`, which lists windows but gives
//! nothing to capture them by.

use std::collections::HashMap;

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry;
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::{
    self, ExtForeignToplevelHandleV1,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::{
    self, ExtForeignToplevelListV1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::{
    self, ZwlrForeignToplevelHandleV1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::{
    self, ZwlrForeignToplevelManagerV1,
};
use windowcast_protocol::{selection, WindowId, WindowInfo};

#[derive(Debug, thiserror::Error)]
pub enum ToplevelError {
    #[error("wayland connection error: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    #[error("wayland dispatch error: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    #[error("the compositor lists no windows (neither ext_foreign_toplevel_list_v1 nor zwlr_foreign_toplevel_manager_v1)")]
    ManagerUnavailable,
    #[error("failed to enumerate compositor globals: {0}")]
    Globals(#[from] wayland_client::globals::GlobalError),
}

/// A window id from a toplevel identifier (FNV-1a), the same on every
/// connection to the compositor.
pub fn window_id(identifier: &str) -> WindowId {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in identifier.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    // Small enough to survive a round trip through a JSON number.
    WindowId(hash >> 12)
}

/// One toplevel as the list reports it.
#[derive(Debug, Default, Clone)]
pub struct Toplevel {
    pub identifier: String,
    pub title: String,
    pub app_id: String,
    pub focused: bool,
    pub closed: bool,
}

/// Toplevels by protocol object id, filled in by either protocol.
#[derive(Default)]
pub struct Toplevels {
    pub by_object: HashMap<u32, Toplevel>,
    /// The list's handles, kept for creating capture sources.
    pub handles: HashMap<u32, ExtForeignToplevelHandleV1>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ListState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// The state of a listing connection.
#[derive(Default)]
pub struct ListState {
    pub toplevels: Toplevels,
}

impl AsMut<Toplevels> for ListState {
    fn as_mut(&mut self) -> &mut Toplevels {
        &mut self.toplevels
    }
}

wayland_client::delegate_dispatch!(ListState: [ExtForeignToplevelListV1: ()] => Toplevels);
wayland_client::delegate_dispatch!(ListState: [ExtForeignToplevelHandleV1: ()] => Toplevels);

// Generic over the state that holds a `Toplevels`, so a capture's own
// connection state can delegate the list to it.
impl<D> Dispatch<ExtForeignToplevelListV1, (), D> for Toplevels
where
    D: Dispatch<ExtForeignToplevelListV1, ()>
        + Dispatch<ExtForeignToplevelHandleV1, ()>
        + AsMut<Toplevels>
        + 'static,
{
    fn event(
        state: &mut D,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<D>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            let state = state.as_mut();
            let id = toplevel.id().protocol_id();
            state.by_object.insert(id, Toplevel::default());
            state.handles.insert(id, toplevel);
        }
    }

    event_created_child!(D, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl<D> Dispatch<ExtForeignToplevelHandleV1, (), D> for Toplevels
where
    D: Dispatch<ExtForeignToplevelHandleV1, ()> + AsMut<Toplevels>,
{
    fn event(
        state: &mut D,
        proxy: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<D>,
    ) {
        let entry = state
            .as_mut()
            .by_object
            .entry(proxy.id().protocol_id())
            .or_default();
        match event {
            ext_foreign_toplevel_handle_v1::Event::Title { title } => entry.title = title,
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => entry.app_id = app_id,
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                entry.identifier = identifier
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => entry.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for ListState {
    fn event(
        state: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
            state
                .toplevels
                .by_object
                .insert(toplevel.id().protocol_id(), Toplevel::default());
        }
    }

    event_created_child!(ListState, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for ListState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = proxy.id().protocol_id();
        let entry = state.toplevels.by_object.entry(id).or_default();
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => entry.title = title,
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => entry.app_id = app_id,
            zwlr_foreign_toplevel_handle_v1::Event::State { state: states } => {
                entry.focused = states.chunks(4).any(|chunk| {
                    u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
                        == zwlr_foreign_toplevel_handle_v1::State::Activated as u32
                });
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => entry.closed = true,
            _ => {}
        }
    }
}

/// Binds the toplevel list on `queue`'s connection and reads every current
/// toplevel; with `ext` false (or no ext list) the wlr manager is used.
pub fn read(
    conn: &Connection,
    globals: &wayland_client::globals::GlobalList,
    queue: &mut wayland_client::EventQueue<ListState>,
) -> Result<(Toplevels, bool), ToplevelError> {
    let qh = queue.handle();
    let ext: Option<ExtForeignToplevelListV1> = globals.bind(&qh, 1..=1, ()).ok();
    if ext.is_none() {
        let _: ZwlrForeignToplevelManagerV1 = globals
            .bind(&qh, 1..=3, ())
            .map_err(|_| ToplevelError::ManagerUnavailable)?;
    }
    let mut state = ListState::default();
    // One round trip for the toplevels, one for their title, app id and
    // identifier.
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    let _ = conn;
    Ok((state.toplevels, ext.is_some()))
}

/// Connects to the compositor named by `WAYLAND_DISPLAY`, lists its
/// windows, and disconnects. With sway, each window's size is filled in
/// from sway's IPC.
pub fn list_windows() -> Result<Vec<WindowInfo>, ToplevelError> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<ListState>(&conn)?;
    let (toplevels, ext) = read(&conn, &globals, &mut queue)?;
    let sizes: HashMap<String, (u32, u32)> = crate::sway::Sway::connect()
        .and_then(|mut sway| sway.windows().ok())
        .into_iter()
        .flatten()
        .map(|w| {
            (
                w.identifier,
                (w.rect.width.max(0) as u32, w.rect.height.max(0) as u32),
            )
        })
        .collect();
    Ok(toplevels
        .by_object
        .into_iter()
        .filter(|(_, t)| !t.closed)
        .map(|(object, t)| {
            let (width, height) = sizes.get(&t.identifier).copied().unwrap_or((0, 0));
            WindowInfo {
                // Without the ext list there is no identifier: the object
                // id (stable only per connection) still names the window
                // in the list, though it cannot be captured.
                id: if ext {
                    window_id(&t.identifier)
                } else {
                    WindowId(u64::from(object))
                },
                content: selection::classify(&t.app_id, &t.title),
                title: t.title,
                app_id: t.app_id,
                width,
                height,
                focused: t.focused,
            }
        })
        .collect())
}

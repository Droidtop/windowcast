//! Capture with `ext-image-copy-capture-v1`: one window by its toplevel
//! (the `ext_foreign_toplevel_image_capture_source_manager_v1` source), or,
//! for the desktop backend, the whole output a window is on with the
//! window's rectangle (from sway) cut out of each picture. Pictures land in
//! a shared-memory buffer as BGRA; each capture is its own Wayland
//! connection on the stream's thread.

use std::os::fd::{AsFd, OwnedFd};
use std::time::{Duration, Instant};

use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::fs::{ftruncate, memfd_create, MemfdFlags};
use rustix::mm::{mmap, munmap, MapFlags, ProtFlags};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{delegate_dispatch, Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};
use windowcast_host::video::Bgra;
use windowcast_protocol::WindowId;

use crate::sway::{self, Sway};
use crate::toplevels::{self, Toplevels};

type Result<T> = std::result::Result<T, String>;

/// What the session and the frame in flight have said.
#[derive(Default)]
struct State {
    toplevels: Toplevels,
    /// wl_output name events, by object.
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
    size: Option<(u32, u32)>,
    formats: Vec<wl_shm::Format>,
    constraints: bool,
    stopped: bool,
    ready: bool,
    failed: Option<WEnum<ext_image_copy_capture_frame_v1::FailureReason>>,
}

impl AsMut<Toplevels> for State {
    fn as_mut(&mut self) -> &mut Toplevels {
        &mut self.toplevels
    }
}

delegate_dispatch!(State: [ExtForeignToplevelListV1: ()] => Toplevels);
delegate_dispatch!(State: [ExtForeignToplevelHandleV1: ()] => Toplevels);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
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

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(entry) = state.outputs.iter_mut().find(|(o, _)| o == proxy) {
                entry.1 = Some(name);
            }
        }
    }
}

macro_rules! ignore_events {
    ($($proxy:ty),*) => {$(
        impl Dispatch<$proxy, ()> for State {
            fn event(
                _: &mut Self,
                _: &$proxy,
                _: <$proxy as wayland_client::Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}

ignore_events!(
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    ExtImageCopyCaptureManagerV1,
    ExtForeignToplevelImageCaptureSourceManagerV1,
    ExtOutputImageCaptureSourceManagerV1,
    ExtImageCaptureSourceV1
);

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => {
                state.size = Some((width, height));
                state.formats.clear();
                state.constraints = false;
            }
            Event::ShmFormat {
                format: WEnum::Value(format),
            } => state.formats.push(format),
            Event::Done => state.constraints = true,
            Event::Stopped => state.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => state.ready = true,
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                state.failed = Some(reason)
            }
            _ => {}
        }
    }
}

/// A shared-memory buffer the compositor copies pictures into.
struct Buffer {
    _fd: OwnedFd,
    map: *mut core::ffi::c_void,
    len: usize,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    width: usize,
    height: usize,
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        unsafe {
            let _ = munmap(self.map, self.len);
        }
    }
}

/// What a capture cuts out of its pictures: nothing (a window's own
/// capture), or one window's rectangle on its output (the desktop backend).
enum Cut {
    Whole,
    Window {
        sway: Sway,
        identifier: String,
        output: String,
    },
}

pub struct Capture {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    shm: wl_shm::WlShm,
    session: ExtImageCopyCaptureSessionV1,
    _source: ExtImageCaptureSourceV1,
    buffer: Option<Buffer>,
    frame: Option<ExtImageCopyCaptureFrameV1>,
    /// A picture is in the buffer.
    have_picture: bool,
    cut: Cut,
}

// The connection and its objects are only used from the stream's thread
// after the move; the raw map pointer is owned by `Buffer`.
unsafe impl Send for Capture {}

fn wayland(e: impl std::fmt::Display) -> String {
    format!("wayland: {e}")
}

impl Capture {
    /// Captures the window `window` names.
    pub fn window(window: WindowId) -> Result<Self> {
        Self::open(window, false)
    }

    /// Captures the output `window` is on and cuts the window out (needs
    /// sway, for where the window is).
    pub fn desktop(window: WindowId) -> Result<Self> {
        Self::open(window, true)
    }

    fn open(window: WindowId, desktop: bool) -> Result<Self> {
        let conn = Connection::connect_to_env().map_err(wayland)?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(wayland)?;
        let qh = queue.handle();
        let _list: ExtForeignToplevelListV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| "the compositor has no ext_foreign_toplevel_list_v1".to_owned())?;
        let manager: ExtImageCopyCaptureManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| "the compositor has no ext_image_copy_capture_manager_v1".to_owned())?;
        let shm: wl_shm::WlShm = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| "the compositor has no wl_shm".to_owned())?;
        let mut state = State::default();
        if desktop {
            for global in globals.contents().clone_list() {
                if global.interface == "wl_output" && global.version >= 4 {
                    let output: wl_output::WlOutput =
                        globals.registry().bind(global.name, 4, &qh, ());
                    state.outputs.push((output, None));
                }
            }
        }
        // The toplevels (and their identifiers), and the outputs' names.
        for _ in 0..2 {
            queue.roundtrip(&mut state).map_err(wayland)?;
        }
        let (object, identifier) = state
            .toplevels
            .by_object
            .iter()
            .find(|(_, t)| !t.closed && toplevels::window_id(&t.identifier) == window)
            .map(|(object, t)| (*object, t.identifier.clone()))
            .ok_or_else(|| "no such window".to_owned())?;

        let (source, cut) = if desktop {
            let mut sway =
                Sway::connect().ok_or("the desktop backend needs sway (no $SWAYSOCK)")?;
            let placed = sway
                .windows()
                .map_err(|e| format!("sway: {e}"))?
                .into_iter()
                .find(|w| w.identifier == identifier)
                .ok_or("sway does not know this window")?;
            let output = state
                .outputs
                .iter()
                .find(|(_, name)| name.as_deref() == Some(placed.output.as_str()))
                .map(|(output, _)| output.clone())
                .ok_or("the window's output is not on this connection")?;
            let sources: ExtOutputImageCaptureSourceManagerV1 =
                globals.bind(&qh, 1..=1, ()).map_err(|_| {
                    "the compositor has no ext_output_image_capture_source_manager_v1".to_owned()
                })?;
            (
                sources.create_source(&output, &qh, ()),
                Cut::Window {
                    sway,
                    identifier,
                    output: placed.output,
                },
            )
        } else {
            let sources: ExtForeignToplevelImageCaptureSourceManagerV1 =
                globals.bind(&qh, 1..=1, ()).map_err(|_| {
                    "the compositor cannot capture single windows (no ext_foreign_toplevel_image_capture_source_manager_v1)".to_owned()
                })?;
            let handle = state.toplevels.handles[&object].clone();
            (sources.create_source(&handle, &qh, ()), Cut::Whole)
        };
        let session = manager.create_session(
            &source,
            ext_image_copy_capture_manager_v1::Options::empty(),
            &qh,
            (),
        );
        let mut capture = Capture {
            conn,
            queue,
            state,
            shm,
            session,
            _source: source,
            buffer: None,
            frame: None,
            have_picture: false,
            cut,
        };
        capture.wait_for_constraints()?;
        Ok(capture)
    }

    fn wait_for_constraints(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.state.constraints {
            if self.state.stopped {
                return Err("the window went away".into());
            }
            if Instant::now() >= deadline {
                return Err("the compositor sent no buffer constraints".into());
            }
            self.dispatch(Duration::from_millis(100))?;
        }
        self.allocate()
    }

    /// A buffer at the session's size in a format we read (XRGB or ARGB,
    /// both BGRA bytes in memory).
    fn allocate(&mut self) -> Result<()> {
        self.buffer = None;
        let (width, height) = self.state.size.ok_or("no buffer size")?;
        let format = [wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888]
            .into_iter()
            .find(|f| self.state.formats.contains(f))
            .ok_or("the compositor offers no 32-bit RGB shared-memory format")?;
        let (width, height) = (width as usize, height as usize);
        let len = width * height * 4;
        let fd = memfd_create("windowcast-capture", MemfdFlags::CLOEXEC)
            .map_err(|e| format!("memfd: {e}"))?;
        ftruncate(&fd, len as u64).map_err(|e| format!("memfd size: {e}"))?;
        let map = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                &fd,
                0,
            )
        }
        .map_err(|e| format!("mmap: {e}"))?;
        let qh = self.queue.handle();
        let pool = self.shm.create_pool(fd.as_fd(), len as i32, &qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            (width * 4) as i32,
            format,
            &qh,
            (),
        );
        self.buffer = Some(Buffer {
            _fd: fd,
            map,
            len,
            pool,
            buffer,
            width,
            height,
        });
        Ok(())
    }

    /// Reads and dispatches events, waiting up to `timeout` for some.
    fn dispatch(&mut self, timeout: Duration) -> Result<()> {
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(wayland)?;
        self.conn.flush().map_err(wayland)?;
        if let Some(guard) = self.queue.prepare_read() {
            let fd = guard.connection_fd();
            let mut fds = [PollFd::new(&fd, PollFlags::IN)];
            let timespec = Timespec {
                tv_sec: timeout.as_secs() as _,
                tv_nsec: timeout.subsec_nanos() as _,
            };
            let ready = poll(&mut fds, Some(&timespec)).map_err(|e| format!("poll: {e}"))?;
            if ready > 0 {
                guard.read().map_err(wayland)?;
            }
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(wayland)?;
        Ok(())
    }

    /// Waits up to `timeout` for a new picture: true when one arrived (the
    /// compositor completes a capture only once the window changed).
    /// Fails when the window went away.
    pub fn poll(&mut self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.state.stopped {
                return Err("the window went away".into());
            }
            if self.frame.is_none() {
                let buffer = self.buffer.as_ref().ok_or("no buffer")?;
                let qh = self.queue.handle();
                let frame = self.session.create_frame(&qh, ());
                frame.attach_buffer(&buffer.buffer);
                frame.damage_buffer(0, 0, buffer.width as i32, buffer.height as i32);
                frame.capture();
                self.state.ready = false;
                self.state.failed = None;
                self.frame = Some(frame);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            self.dispatch(left)?;
            if self.state.ready {
                if let Some(frame) = self.frame.take() {
                    frame.destroy();
                }
                self.have_picture = true;
                return Ok(true);
            }
            if let Some(reason) = self.state.failed.take() {
                if let Some(frame) = self.frame.take() {
                    frame.destroy();
                }
                match reason {
                    WEnum::Value(ext_image_copy_capture_frame_v1::FailureReason::Stopped) => {
                        return Err("the window went away".into())
                    }
                    // The window was resized: new constraints follow.
                    WEnum::Value(
                        ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints,
                    ) => {
                        self.state.constraints = false;
                        self.have_picture = false;
                        self.wait_for_constraints()?;
                    }
                    _ => {}
                }
                continue;
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
        }
    }

    /// The latest picture, if one arrived (and for the desktop backend,
    /// the window's part of it).
    pub fn picture(&mut self) -> Option<Bgra<'_>> {
        if !self.have_picture {
            return None;
        }
        let buffer = self.buffer.as_ref()?;
        let data = unsafe { std::slice::from_raw_parts(buffer.map as *const u8, buffer.len) };
        let stride = buffer.width * 4;
        let (left, top, width, height) = match &mut self.cut {
            Cut::Whole => (0, 0, buffer.width, buffer.height),
            Cut::Window {
                sway,
                identifier,
                output,
            } => cut_rect(sway, identifier, output, buffer.width, buffer.height)?,
        };
        Some(Bgra {
            data: &data[top * stride + left * 4..],
            width,
            height,
            stride,
        })
    }
}

/// The window's rectangle inside its output's picture (`width` by
/// `height`, which may be scaled from the output's layout size).
fn cut_rect(
    sway: &mut Sway,
    identifier: &str,
    output: &str,
    width: usize,
    height: usize,
) -> Option<(usize, usize, usize, usize)> {
    let window = sway
        .windows()
        .ok()?
        .into_iter()
        .find(|w| w.identifier == identifier)?;
    let screen: sway::Rect = sway
        .outputs()
        .ok()?
        .into_iter()
        .find(|(name, _)| name == output)?
        .1;
    if screen.width <= 0 || screen.height <= 0 {
        return None;
    }
    let scale_x = width as f64 / f64::from(screen.width);
    let scale_y = height as f64 / f64::from(screen.height);
    let clamp = |v: f64, limit: usize| (v.max(0.0) as usize).min(limit);
    let left = clamp(f64::from(window.rect.x - screen.x) * scale_x, width);
    let top = clamp(f64::from(window.rect.y - screen.y) * scale_y, height);
    let right = clamp(
        f64::from(window.rect.x + window.rect.width - screen.x) * scale_x,
        width,
    );
    let bottom = clamp(
        f64::from(window.rect.y + window.rect.height - screen.y) * scale_y,
        height,
    );
    (right >= left + 2 && bottom >= top + 2).then(|| (left, top, right - left, bottom - top))
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        self.session.destroy();
        let _ = self.conn.flush();
    }
}

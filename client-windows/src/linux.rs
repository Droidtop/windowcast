//! The Linux stream window (Droidtop/tracker#615): each streamed window as a
//! top-level Wayland window of its own, so in droidtop's Linux container (or
//! any Wayland desktop) it moves, resizes and closes like any window. This is
//! the reference app's client role on Linux, the counterpart of the Windows
//! window beside it in this crate, with the same `open`, [`StreamSource`],
//! [`Placement`] and [`Shared`].
//!
//! H.264 is decoded with OpenH264 (the decoder windowcast's host already
//! builds) into RGBA, drawn into `wl_shm` buffers. Pointer, wheel and keys go
//! to the host while [`Shared::send_input`] is set; Wayland's key codes are
//! evdev's, which is windowcast's key space, so they pass as they are.

use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use windowcast_client::{ClientSession, FramePoll};
use windowcast_protocol::{InputEvent, PointerButton, VideoCodec, WindowId};

use crate::{Placement, Shared, SharedStats, StreamSource};

/// A stream window running on a thread of its own. Dropping it closes it.
pub struct StreamWindow {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub shared: SharedStats,
}

impl StreamWindow {
    /// Closes the window (the stream itself is the caller's to stop).
    pub fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for StreamWindow {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Opens `window`'s stream in a Wayland window titled `title`.
pub fn open(
    session: Arc<ClientSession>,
    window: WindowId,
    source: StreamSource,
    title: String,
    _placement: Placement,
    shared: SharedStats,
) -> StreamWindow {
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let (stop, shared) = (Arc::clone(&stop), Arc::clone(&shared));
        std::thread::spawn(move || {
            let result = run(&session, window, source, &title, &shared, &stop);
            let mut stats = shared.stats.lock().expect("stats");
            stats.closed = true;
            if let Err(e) = result {
                stats.error = Some(e);
            }
        })
    };
    StreamWindow {
        stop,
        thread: Some(thread),
        shared,
    }
}

/// The codecs this end decodes: OpenH264's H.264.
pub fn decodable() -> Vec<VideoCodec> {
    vec![VideoCodec::H264]
}

fn run(
    session: &ClientSession,
    window: WindowId,
    source: StreamSource,
    title: &str,
    shared: &Shared,
    stop: &AtomicBool,
) -> Result<(), String> {
    match source {
        StreamSource::Video(VideoCodec::H264) => {}
        StreamSource::Video(codec) => {
            return Err(format!(
                "this Linux window decodes H.264 only, not {codec:?}"
            ))
        }
        StreamSource::Pictures => {
            return Err("this Linux window shows video only, not RDP pictures".into())
        }
    }
    let connection =
        Connection::connect_to_env().map_err(|e| format!("no Wayland display: {e}"))?;
    let mut queue = connection.new_event_queue();
    let handle = queue.handle();
    connection.display().get_registry(&handle, ());
    let mut state = State::new(window);
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    let (Some(compositor), Some(shm), Some(wm)) = (
        state.compositor.clone(),
        state.shm.clone(),
        state.wm.clone(),
    ) else {
        return Err("the compositor offers no xdg-shell, wl_compositor or wl_shm".into());
    };
    let surface = compositor.create_surface(&handle, ());
    let xdg = wm.get_xdg_surface(&surface, &handle, ());
    let toplevel = xdg.get_toplevel(&handle, ());
    toplevel.set_title(title.to_string());
    toplevel.set_app_id("dev.windowcast.stream".into());
    surface.commit();
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;

    let mut decoder = openh264::decoder::Decoder::new().map_err(|e| format!("OpenH264: {e}"))?;
    shared.stats.lock().expect("stats").decoder = "OpenH264 (software)".into();
    let mut buffers: Vec<Shown> = Vec::new();
    session.request_keyframe(window);
    while !stop.load(Ordering::SeqCst) && !state.closed {
        queue
            .dispatch_pending(&mut state)
            .map_err(|e| e.to_string())?;
        connection.flush().map_err(|e| e.to_string())?;
        if let Some(guard) = connection.prepare_read() {
            // Events that are already there; the frame wait below paces the loop.
            let _ = guard.read();
        }
        state.send_input = shared.send_input.load(Ordering::SeqCst);
        for event in state.input.drain(..) {
            let _ = session.send_input(event);
        }
        let frame = match session.next_frame(window, Duration::from_millis(16)) {
            FramePoll::Frame(frame) => frame,
            FramePoll::Timeout => continue,
            FramePoll::Ended => break,
        };
        {
            let mut stats = shared.stats.lock().expect("stats");
            stats.frames_received += 1;
            stats.bytes += frame.data.len() as u64;
            if frame.keyframe {
                stats.keyframes += 1;
            }
        }
        let decoded = match decoder.decode(&frame.data) {
            Ok(Some(picture)) => picture,
            Ok(None) => continue,
            Err(_) => {
                shared.stats.lock().expect("stats").resets += 1;
                session.request_keyframe(window);
                continue;
            }
        };
        use openh264::formats::YUVSource;
        let (width, height) = decoded.dimensions();
        let (width, height) = (width as u32, height as u32);
        state.picture = (width, height);
        // A free buffer of this size, or a new one; old sizes go once they are free.
        buffers.retain(|b| (b.width, b.height) == (width, height) || b.busy.load(Ordering::SeqCst));
        let index = match buffers
            .iter()
            .position(|b| (b.width, b.height) == (width, height) && !b.busy.load(Ordering::SeqCst))
        {
            Some(index) => index,
            None if buffers.len() < 4 => {
                buffers.push(Shown::new(&shm, &handle, width, height)?);
                buffers.len() - 1
            }
            // Every buffer is still the compositor's: this picture is skipped.
            None => continue,
        };
        let shown = &mut buffers[index];
        decoded.write_rgba8(&mut shown.rgba);
        shown.copy_out();
        shown.busy.store(true, Ordering::SeqCst);
        surface.attach(Some(&shown.buffer), 0, 0);
        surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.commit();
        let mut stats = shared.stats.lock().expect("stats");
        stats.size = Some((width, height));
        stats.frames_shown += 1;
    }
    toplevel.destroy();
    xdg.destroy();
    surface.destroy();
    let _ = connection.flush();
    Ok(())
}

/// One `wl_shm` buffer the size of a picture, XRGB in memory the
/// compositor maps, and whether the compositor still holds it.
struct Shown {
    width: u32,
    height: u32,
    map: memmap::Map,
    rgba: Vec<u8>,
    buffer: wl_buffer::WlBuffer,
    busy: Arc<AtomicBool>,
}

impl Shown {
    fn new(
        shm: &wl_shm::WlShm,
        handle: &QueueHandle<State>,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let stride = width * 4;
        let size = (stride * height) as usize;
        let map =
            memmap::Map::new(size).map_err(|e| format!("shared memory for a picture: {e}"))?;
        let pool = shm.create_pool(map.fd.as_fd(), size as i32, handle, ());
        let busy = Arc::new(AtomicBool::new(false));
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            wl_shm::Format::Xrgb8888,
            handle,
            Arc::clone(&busy),
        );
        pool.destroy();
        Ok(Shown {
            width,
            height,
            map,
            rgba: vec![0; size],
            buffer,
            busy,
        })
    }

    fn copy_out(&mut self) {
        rgba_to_xrgb(&self.rgba, self.map.bytes());
    }
}

impl Drop for Shown {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

/// RGBA (OpenH264's) to the little-endian XRGB8888 Wayland wants: B, G, R, X in memory.
fn rgba_to_xrgb(from: &[u8], to: &mut [u8]) {
    for (to, from) in to
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(from.as_chunks::<4>().0)
    {
        to[0] = from[2];
        to[1] = from[1];
        to[2] = from[0];
        to[3] = 0xFF;
    }
}

/// Anonymous shared memory for `wl_shm`.
mod memmap {
    use std::os::fd::OwnedFd;

    pub struct Map {
        pub fd: OwnedFd,
        ptr: *mut u8,
        len: usize,
    }

    // The mapping is written only by the stream window's own thread.
    unsafe impl Send for Map {}

    impl Map {
        pub fn new(len: usize) -> std::io::Result<Self> {
            let fd =
                rustix::fs::memfd_create("windowcast-stream", rustix::fs::MemfdFlags::CLOEXEC)?;
            rustix::fs::ftruncate(&fd, len as u64)?;
            // SAFETY: a fresh shared mapping of the whole file just sized to `len`.
            let ptr = unsafe {
                rustix::mm::mmap(
                    std::ptr::null_mut(),
                    len,
                    rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
                    rustix::mm::MapFlags::SHARED,
                    &fd,
                    0,
                )?
            };
            Ok(Map {
                fd,
                ptr: ptr.cast(),
                len,
            })
        }

        pub fn bytes(&mut self) -> &mut [u8] {
            // SAFETY: `ptr` maps `len` bytes for as long as `self` lives.
            unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
        }
    }

    impl Drop for Map {
        fn drop(&mut self) {
            // SAFETY: the mapping made in `new`, unmapped once.
            unsafe {
                let _ = rustix::mm::munmap(self.ptr.cast(), self.len);
            }
        }
    }
}

/// The Wayland side's state: the globals, the window's size, and the input
/// it turned into windowcast events.
struct State {
    window: WindowId,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    picture: (u32, u32),
    pointer: (f64, f64),
    send_input: bool,
    input: Vec<InputEvent>,
    closed: bool,
}

impl State {
    fn new(window: WindowId) -> Self {
        State {
            window,
            compositor: None,
            shm: None,
            wm: None,
            picture: (0, 0),
            pointer: (0.0, 0.0),
            send_input: false,
            input: Vec::new(),
            closed: false,
        }
    }

    fn push(&mut self, event: InputEvent) {
        if self.send_input {
            self.input.push(event);
        }
    }

    /// The pointer as a fraction of the picture, which the surface shows at 1:1.
    fn fraction(&self) -> (f32, f32) {
        let (w, h) = (self.picture.0.max(1) as f64, self.picture.1.max(1) as f64);
        (
            (self.pointer.0 / w).clamp(0.0, 1.0) as f32,
            (self.pointer.1 / h).clamp(0.0, 1.0) as f32,
        )
    }
}

/// The evdev button codes a Wayland pointer sends.
fn button(code: u32) -> Option<PointerButton> {
    match code {
        0x110 => Some(PointerButton::Left),
        0x111 => Some(PointerButton::Right),
        0x112 => Some(PointerButton::Middle),
        0x113 => Some(PointerButton::Back),
        0x114 => Some(PointerButton::Forward),
        _ => None,
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" => {
                state.compositor = Some(registry.bind(name, version.min(4), handle, ()))
            }
            "wl_shm" => state.shm = Some(registry.bind(name, 1, handle, ())),
            "xdg_wm_base" => state.wm = Some(registry.bind(name, 1, handle, ())),
            "wl_seat" => {
                registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(5), handle, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Pointer) {
                seat.get_pointer(handle, ());
            }
            if caps.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(handle, ());
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let window = state.window;
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer = (surface_x, surface_y);
                let (x, y) = state.fraction();
                state.push(InputEvent::PointerMove { window, x, y });
            }
            wl_pointer::Event::Button {
                button: code,
                state: pressed,
                ..
            } => {
                if let Some(button) = button(code) {
                    let pressed = matches!(pressed, WEnum::Value(wl_pointer::ButtonState::Pressed));
                    state.push(InputEvent::PointerButton {
                        window,
                        button,
                        pressed,
                    });
                }
            }
            wl_pointer::Event::Axis {
                axis: WEnum::Value(axis),
                value,
                ..
            } => {
                // Wayland's axis is in surface pixels, about ten a notch, positive down or right;
                // windowcast's positive dy scrolls content up.
                let notches = (value / 10.0) as f32;
                let (dx, dy) = match axis {
                    wl_pointer::Axis::HorizontalScroll => (notches, 0.0),
                    _ => (0.0, -notches),
                };
                state.push(InputEvent::PointerScroll { window, dx, dy });
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key {
            key,
            state: pressed,
            ..
        } = event
        {
            let pressed = matches!(pressed, WEnum::Value(wl_keyboard::KeyState::Pressed));
            state.push(InputEvent::Key {
                keycode: key,
                pressed,
            });
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        _: &mut Self,
        xdg: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            state.closed = true;
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, Arc<AtomicBool>> for State {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        busy: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            busy.store(false, Ordering::SeqCst);
        }
    }
}

macro_rules! no_events {
    ($($proxy:ty),*) => {$(
        impl Dispatch<$proxy, ()> for State {
            fn event(_: &mut Self, _: &$proxy, _: <$proxy as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
no_events!(
    wl_compositor::WlCompositor,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_buttons_are_evdevs() {
        assert_eq!(button(0x110), Some(PointerButton::Left));
        assert_eq!(button(0x111), Some(PointerButton::Right));
        assert_eq!(button(0x114), Some(PointerButton::Forward));
        assert_eq!(button(0x120), None);
    }

    #[test]
    fn the_pointer_is_a_fraction_of_the_picture_and_input_waits_for_the_switch() {
        let mut state = State::new(WindowId(3));
        state.picture = (200, 100);
        state.pointer = (50.0, 75.0);
        assert_eq!(state.fraction(), (0.25, 0.75));
        state.pointer = (500.0, -5.0);
        assert_eq!(state.fraction(), (1.0, 0.0));
        state.push(InputEvent::Key {
            keycode: 30,
            pressed: true,
        });
        assert!(state.input.is_empty());
        state.send_input = true;
        state.push(InputEvent::Key {
            keycode: 30,
            pressed: true,
        });
        assert_eq!(state.input.len(), 1);
    }

    #[test]
    fn rgba_becomes_xrgb_in_wayland_byte_order() {
        let mut map = memmap::Map::new(8).unwrap();
        rgba_to_xrgb(&[10, 20, 30, 40, 1, 2, 3, 4], map.bytes());
        assert_eq!(map.bytes(), &[30, 20, 10, 0xFF, 3, 2, 1, 0xFF]);
    }
}

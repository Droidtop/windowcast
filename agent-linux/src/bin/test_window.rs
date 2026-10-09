//! A Wayland window for the agent's tests: it paints one colour and prints
//! the input it receives, one line each, so a test can check what reached
//! it.
//!
//! Usage: `windowcast-test-window TITLE RRGGBB`. Lines: `ready WxH`,
//! `motion X Y` (surface coordinates), `button CODE pressed|released`,
//! `key CODE pressed|released`, `axis VALUE`.

use std::io::Write;
use std::os::fd::AsFd;

use rustix::fs::{ftruncate, memfd_create, MemfdFlags};
use rustix::mm::{mmap, MapFlags, ProtFlags};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

struct Window {
    color: u32,
    shm: wl_shm::WlShm,
    surface: wl_surface::WlSurface,
    size: (i32, i32),
    configured: bool,
    closed: bool,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
}

fn say(line: String) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

impl Window {
    fn draw(&mut self, qh: &QueueHandle<Self>) {
        let (w, h) = (self.size.0.max(1), self.size.1.max(1));
        let len = (w * h * 4) as usize;
        let fd = memfd_create("test-window", MemfdFlags::CLOEXEC).expect("memfd");
        ftruncate(&fd, len as u64).expect("size");
        unsafe {
            let map = mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                &fd,
                0,
            )
            .expect("mmap");
            let pixels = std::slice::from_raw_parts_mut(map as *mut u32, len / 4);
            pixels.fill(0xff00_0000 | self.color);
        }
        let pool = self.shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Xrgb8888, qh, ());
        pool.destroy();
        self.surface.attach(Some(&buffer), 0, 0);
        self.surface.damage_buffer(0, 0, w, h);
        self.surface.commit();
        say(format!("ready {w}x{h}"));
    }
}

macro_rules! ignore_events {
    ($($proxy:ty),*) => {$(
        impl Dispatch<$proxy, ()> for Window {
            fn event(_: &mut Self, _: &$proxy, _: <$proxy as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}

ignore_events!(
    wl_compositor::WlCompositor,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface
);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Window {
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

impl Dispatch<wl_buffer::WlBuffer, ()> for Window {
    fn event(
        _: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            buffer.destroy();
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Window {
    fn event(
        _: &mut Self,
        base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Window {
    fn event(
        window: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            window.configured = true;
            window.draw(qh);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Window {
    fn event(
        window: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                if width > 0 && height > 0 {
                    window.size = (width, height);
                }
            }
            xdg_toplevel::Event::Close => window.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Window {
    fn event(
        window: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Pointer) && window.pointer.is_none() {
                window.pointer = Some(seat.get_pointer(qh, ()));
            }
            if capabilities.contains(wl_seat::Capability::Keyboard) && window.keyboard.is_none() {
                window.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

fn state(state: WEnum<wl_pointer::ButtonState>) -> &'static str {
    match state {
        WEnum::Value(wl_pointer::ButtonState::Pressed) => "pressed",
        _ => "released",
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Window {
    fn event(
        _: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
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
            } => say(format!("motion {surface_x:.0} {surface_y:.0}")),
            wl_pointer::Event::Button {
                button,
                state: pressed,
                ..
            } => say(format!("button {button} {}", state(pressed))),
            wl_pointer::Event::Axis { value, .. } => say(format!("axis {value:.0}")),
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Window {
    fn event(
        _: &mut Self,
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
            say(format!(
                "key {key} {}",
                if pressed { "pressed" } else { "released" }
            ));
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let title = args.next().unwrap_or_else(|| "windowcast test".into());
    let color = args
        .next()
        .and_then(|hex| u32::from_str_radix(&hex, 16).ok())
        .unwrap_or(0x3366cc);

    let conn = Connection::connect_to_env().expect("wayland");
    let (globals, mut queue) = registry_queue_init::<Window>(&conn).expect("globals");
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=5, ()).expect("compositor");
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).expect("shm");
    let base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=5, ()).expect("xdg_wm_base");
    let _seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).expect("seat");
    let surface = compositor.create_surface(&qh, ());
    let xdg = base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title(title);
    toplevel.set_app_id("windowcast-test".into());
    surface.commit();

    let mut window = Window {
        color,
        shm,
        surface,
        size: (640, 480),
        configured: false,
        closed: false,
        pointer: None,
        keyboard: None,
    };
    while !window.closed {
        if queue.blocking_dispatch(&mut window).is_err() {
            break;
        }
    }
    let _ = window.configured;
}

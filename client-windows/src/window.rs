//! The stream window: a plain Win32 window on a thread of its own that
//! pulls the window's frames from client-core, decodes them on the GPU and
//! presents them. F11 switches between a normal window and borderless
//! fullscreen on the display the window is on. With input switched on,
//! the pointer (mapped onto the picture) and keys go to the host.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windowcast_client::{ClientSession, FramePoll};
use windowcast_protocol::{InputEvent, PointerButton, WindowId};
use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromWindow, HDC, HMONITOR, MONITORINFO,
    MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_F11};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::decoder::Decoder;
use crate::present::{self, Presenter};
use crate::{Display, Placement, Shared, SharedStats, StreamSource};

/// An open stream window. Dropping the handle leaves the window open;
/// [`StreamWindow::close`] closes it.
pub struct StreamWindow {
    hwnd: Arc<AtomicIsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub shared: SharedStats,
}

impl StreamWindow {
    /// Closes the window (the stream itself is the caller's to stop).
    pub fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let hwnd = self.hwnd.load(Ordering::SeqCst);
        if hwnd != 0 {
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    pub fn is_closed(&self) -> bool {
        self.shared.stats.lock().expect("stats").closed
    }

    /// Shows a switch's new carrier beside the current one and swaps to it
    /// on its first picture, telling client-core
    /// (`ClientSession::carrier_shown`).
    pub fn switch_to(&self, source: StreamSource, generation: u32) {
        *self.shared.switch.lock().expect("switch") = Some((source, generation));
    }

    /// A switch that did not happen: its carrier is no longer waited for.
    pub fn cancel_switch(&self, generation: u32) {
        *self.shared.cancel_switch.lock().expect("switch") = Some(generation);
    }
}

/// Opens a window showing `window`'s stream from `session`.
pub fn open(
    session: Arc<ClientSession>,
    window: WindowId,
    source: StreamSource,
    title: String,
    placement: Placement,
    shared: SharedStats,
) -> StreamWindow {
    let hwnd = Arc::new(AtomicIsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    crate::audio::play(
        Arc::clone(&session),
        window,
        Arc::clone(&stop),
        Arc::clone(&shared),
    );
    let thread = {
        let (hwnd, stop, shared) = (Arc::clone(&hwnd), Arc::clone(&stop), Arc::clone(&shared));
        std::thread::spawn(move || {
            let result = run(
                &session, window, source, &title, &placement, &shared, &hwnd, &stop,
            );
            let mut stats = shared.stats.lock().expect("stats");
            stats.closed = true;
            if let Err(e) = result {
                stats.error = Some(e);
            }
        })
    };
    StreamWindow {
        hwnd,
        stop,
        thread: Some(thread),
        shared,
    }
}

/// State the window procedure reaches through GWLP_USERDATA.
struct WindowState {
    window: WindowId,
    picture: (u32, u32),
    client: (u32, u32),
    input: Vec<InputEvent>,
    send_input: bool,
    fullscreen: bool,
    /// The normal window's place, to go back to from fullscreen.
    restore: RECT,
    toggle_fullscreen: bool,
    closed: bool,
}

const CLASS: PCWSTR = w!("windowcast-stream");

fn register_class() {
    static REGISTERED: Once = Once::new();
    REGISTERED.call_once(|| unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&class);
    });
}

/// The displays in physical pixels, by Windows' number (n of the device
/// name DISPLAYn).
pub fn displays() -> Vec<Display> {
    unsafe extern "system" fn each(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let found = unsafe { &mut *(data.0 as *mut Vec<Display>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }.as_bool() {
            let name = String::from_utf16_lossy(&info.szDevice);
            let digits: String = name.chars().filter(char::is_ascii_digit).collect();
            let r = info.monitorInfo.rcMonitor;
            found.push(Display {
                number: digits.parse().unwrap_or(0),
                left: r.left,
                top: r.top,
                width: r.right - r.left,
                height: r.bottom - r.top,
                primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            });
        }
        BOOL(1)
    }
    let mut found: Vec<Display> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(each),
            LPARAM(&mut found as *mut Vec<Display> as isize),
        );
    }
    found.sort_by_key(|display| display.number);
    found
}

#[allow(clippy::too_many_arguments)]
fn run(
    session: &ClientSession,
    window: WindowId,
    source: StreamSource,
    title: &str,
    placement: &Placement,
    shared: &Shared,
    hwnd_out: &AtomicIsize,
    stop: &AtomicBool,
) -> Result<(), String> {
    register_class();
    let displays = displays();
    let display = placement
        .display
        .and_then(|number| displays.iter().find(|d| d.number == number))
        .or_else(|| displays.iter().find(|d| d.primary))
        .or_else(|| displays.first())
        .map_or((0, 0, 1280, 720), |d| (d.left, d.top, d.width, d.height));
    let (style, rect) = if let Some(at) = placement.at {
        (WS_POPUP | WS_VISIBLE, at)
    } else if placement.fullscreen {
        (WS_POPUP | WS_VISIBLE, display)
    } else {
        (
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            (
                display.0 + display.2 / 8,
                display.1 + display.3 / 8,
                display.2 * 3 / 4,
                display.3 * 3 / 4,
            ),
        )
    };
    let mut state = Box::new(WindowState {
        window,
        picture: (0, 0),
        client: (rect.2.max(1) as u32, rect.3.max(1) as u32),
        input: Vec::new(),
        send_input: false,
        fullscreen: placement.fullscreen,
        restore: RECT {
            left: display.0 + display.2 / 8,
            top: display.1 + display.3 / 8,
            right: display.0 + display.2 * 7 / 8,
            bottom: display.1 + display.3 * 7 / 8,
        },
        toggle_fullscreen: false,
        closed: false,
    });
    let name: Vec<u16> = format!("{title} - windowcast\0").encode_utf16().collect();
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            PCWSTR(name.as_ptr()),
            style,
            rect.0,
            rect.1,
            rect.2,
            rect.3,
            placement.owner.map(|owner| HWND(owner as *mut _)),
            None,
            GetModuleHandleW(None).ok().map(Into::into),
            None,
        )
    }
    .map_err(|e| format!("stream window: {e}"))?;
    hwnd_out.store(hwnd.0 as isize, Ordering::SeqCst);
    unsafe {
        // The first ShowWindow of a process follows the show state its
        // launcher asked for (a launcher hiding the console hides this
        // too); the second shows the window regardless.
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
    unsafe {
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            &mut *state as *mut WindowState as isize,
        );
    }
    let result = show(session, source, title, shared, hwnd, &mut state, stop);
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        let _ = DestroyWindow(hwnd);
    }
    hwnd_out.store(0, Ordering::SeqCst);
    result
}

fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut rect = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rect);
    }
    (
        (rect.right - rect.left).max(1) as u32,
        (rect.bottom - rect.top).max(1) as u32,
    )
}

/// One way a window's pictures arrive: decoded video from the session, or
/// RDP's RGBA pictures.
enum Path {
    Video {
        decoder: Decoder,
        waiting_for_keyframe: bool,
    },
    Pictures {
        last: Option<windowcast_client::RgbaPicture>,
    },
}

/// What one poll of a path did.
enum Polled {
    Shown,
    Nothing,
    Ended,
}

impl Path {
    /// The path for `source`, with its decoder's name for the statistics.
    fn open(
        source: StreamSource,
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        session: &ClientSession,
        window: WindowId,
    ) -> Result<(Path, String), String> {
        Ok(match source {
            StreamSource::Video(codec) => {
                let decoder = Decoder::new(codec, device)?;
                let name = decoder.name.clone();
                session.request_keyframe(window);
                (
                    Path::Video {
                        decoder,
                        waiting_for_keyframe: true,
                    },
                    name,
                )
            }
            StreamSource::Pictures => (Path::Pictures { last: None }, "RDP pictures".into()),
        })
    }

    /// Takes what the path has, waiting at most `wait`, and presents it.
    fn poll(
        &mut self,
        session: &ClientSession,
        presenter: &mut Presenter,
        state: &mut WindowState,
        shared: &Shared,
        (wait, resized): (Duration, bool),
    ) -> Result<Polled, String> {
        let window = state.window;
        match self {
            Path::Video {
                decoder,
                waiting_for_keyframe,
            } => {
                let frame = match session.next_frame(window, wait) {
                    FramePoll::Frame(frame) => frame,
                    FramePoll::Timeout => return Ok(Polled::Nothing),
                    FramePoll::Ended => return Ok(Polled::Ended),
                };
                let arrived = Instant::now();
                {
                    let mut stats = shared.stats.lock().expect("stats");
                    stats.frames_received += 1;
                    stats.bytes += frame.data.len() as u64;
                    stats.keyframes += u64::from(frame.keyframe);
                }
                if *waiting_for_keyframe && !frame.keyframe {
                    return Ok(Polled::Nothing);
                }
                *waiting_for_keyframe = false;
                match decoder.decode(&frame.data) {
                    Ok(pictures) => {
                        let mut shown = false;
                        for picture in pictures {
                            state.picture = (picture.width, picture.height);
                            presenter.present(&picture)?;
                            shown = true;
                            let mut stats = shared.stats.lock().expect("stats");
                            stats.frames_shown += 1;
                            stats.size = Some(state.picture);
                            let ms = arrived.elapsed().as_secs_f64() * 1000.0;
                            stats.latency_ms = if stats.latency_ms == 0.0 {
                                ms
                            } else {
                                stats.latency_ms * 0.9 + ms * 0.1
                            };
                        }
                        Ok(if shown {
                            Polled::Shown
                        } else {
                            Polled::Nothing
                        })
                    }
                    Err(e) => {
                        eprintln!("window {}: {e}; asking for a keyframe", window.0);
                        shared.stats.lock().expect("stats").resets += 1;
                        *waiting_for_keyframe = true;
                        session.request_keyframe(window);
                        Ok(Polled::Nothing)
                    }
                }
            }
            Path::Pictures { last } => {
                // A window that does not change sends nothing: begin with
                // the newest picture there is.
                let first = if last.is_none() {
                    session.latest_picture(window)
                } else {
                    None
                };
                let picture = match first {
                    Some(picture) => picture,
                    None => match session.next_picture(window, wait) {
                        windowcast_client::PicturePoll::Picture(picture) => picture,
                        windowcast_client::PicturePoll::Timeout => {
                            if let (true, Some(picture)) = (resized, &*last) {
                                presenter.present_rgba(
                                    picture.width,
                                    picture.height,
                                    &picture.data,
                                )?;
                            }
                            return Ok(Polled::Nothing);
                        }
                        windowcast_client::PicturePoll::Ended => return Ok(Polled::Ended),
                    },
                };
                let arrived = Instant::now();
                state.picture = (picture.width, picture.height);
                presenter.present_rgba(picture.width, picture.height, &picture.data)?;
                let mut stats = shared.stats.lock().expect("stats");
                stats.frames_received += 1;
                stats.frames_shown += 1;
                stats.bytes += picture.data.len() as u64;
                stats.size = Some(state.picture);
                let ms = arrived.elapsed().as_secs_f64() * 1000.0;
                stats.latency_ms = if stats.latency_ms == 0.0 {
                    ms
                } else {
                    stats.latency_ms * 0.9 + ms * 0.1
                };
                drop(stats);
                *last = Some(picture);
                Ok(Polled::Shown)
            }
        }
    }
}

/// Shows the window's pictures as they come, on whichever carrier: one
/// presenter, fed by the active path; a switch's new carrier (`switch` in
/// [`Shared`]) runs beside it and takes over on its first picture, in one
/// present, so the window never goes blank (docs/BACKENDS.md, "One window,
/// any carrier").
fn show(
    session: &ClientSession,
    source: StreamSource,
    title: &str,
    shared: &Shared,
    hwnd: HWND,
    state: &mut WindowState,
    stop: &AtomicBool,
) -> Result<(), String> {
    let device = present::create_device()?;
    let mut presenter = Presenter::new(&device, hwnd, client_size(hwnd))?;
    let window = state.window;
    let (mut active, name) = Path::open(source, &device, session, window)?;
    shared.stats.lock().expect("stats").decoder = name;
    let mut pending: Option<(Path, u32, String)> = None;
    let mut second = Instant::now();
    let (mut shown_then, mut bytes_then) = (0u64, 0u64);
    loop {
        // Messages first, so the window stays responsive.
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if state.closed || stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        if state.toggle_fullscreen {
            state.toggle_fullscreen = false;
            toggle_fullscreen(hwnd, state);
        }
        state.send_input = shared.send_input.load(Ordering::SeqCst);
        for event in state.input.drain(..) {
            let _ = session.send_input(event);
        }
        let size = client_size(hwnd);
        let resized = size != state.client;
        state.client = size;
        presenter.resize(size)?;
        publish_view(hwnd, state, shared);

        // A switch asked for, or given up.
        if let Some((source, generation)) = shared.switch.lock().expect("switch").take() {
            match Path::open(source, &device, session, window) {
                Ok((path, name)) => pending = Some((path, generation, name)),
                Err(e) => eprintln!("window {}: cannot show the new carrier: {e}", window.0),
            }
        }
        if let Some(generation) = shared.cancel_switch.lock().expect("switch").take() {
            if pending.as_ref().is_some_and(|(_, g, _)| *g == generation) {
                pending = None;
            }
        }

        let wait = Duration::from_millis(if pending.is_some() { 2 } else { 6 });
        let polled = active.poll(session, &mut presenter, state, shared, (wait, resized))?;
        if matches!(polled, Polled::Ended) && pending.is_none() {
            return Ok(());
        }
        let swapped = match pending.as_mut() {
            Some((path, _, _)) => {
                match path.poll(
                    session,
                    &mut presenter,
                    state,
                    shared,
                    (Duration::from_millis(4), false),
                )? {
                    Polled::Shown => true,
                    Polled::Ended => {
                        pending = None;
                        false
                    }
                    Polled::Nothing => false,
                }
            }
            None => false,
        };
        if swapped {
            let (path, generation, name) = pending.take().expect("pending");
            active = path;
            shared.stats.lock().expect("stats").decoder = name;
            let _ = session.carrier_shown(window, generation);
        }
        tick(
            shared,
            hwnd,
            title,
            state,
            &mut second,
            &mut shown_then,
            &mut bytes_then,
        );
    }
}

/// Where the window is on screen and how it scales the host's window.
fn publish_view(hwnd: HWND, state: &WindowState, shared: &Shared) {
    if state.picture.0 == 0 || state.picture.1 == 0 {
        return;
    }
    let mut origin = windows::Win32::Foundation::POINT::default();
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut origin);
    }
    *shared.view.lock().expect("view") = Some(crate::View {
        hwnd: hwnd.0 as isize,
        origin: (origin.x, origin.y),
        scale: (
            state.client.0 as f32 / state.picture.0 as f32,
            state.client.1 as f32 / state.picture.1 as f32,
        ),
    });
}

/// Once a second: the rates, and the title of a normal window.
fn tick(
    shared: &Shared,
    hwnd: HWND,
    title: &str,
    state: &WindowState,
    second: &mut Instant,
    shown_then: &mut u64,
    bytes_then: &mut u64,
) {
    let elapsed = second.elapsed().as_secs_f64();
    if elapsed < 1.0 {
        return;
    }
    let mut stats = shared.stats.lock().expect("stats");
    stats.fps = (stats.frames_shown - *shown_then) as f64 / elapsed;
    stats.mbps = (stats.bytes - *bytes_then) as f64 * 8.0 / 1_000_000.0 / elapsed;
    *shown_then = stats.frames_shown;
    *bytes_then = stats.bytes;
    *second = Instant::now();
    if !state.fullscreen {
        let text: Vec<u16> = format!(
            "{title} - windowcast ({:.0} fps, {:.1} Mbit/s; F11 fullscreen)\0",
            stats.fps, stats.mbps
        )
        .encode_utf16()
        .collect();
        unsafe {
            let _ = SetWindowTextW(hwnd, PCWSTR(text.as_ptr()));
        }
    }
}

fn toggle_fullscreen(hwnd: HWND, state: &mut WindowState) {
    unsafe {
        if state.fullscreen {
            SetWindowLongPtrW(
                hwnd,
                GWL_STYLE,
                (WS_OVERLAPPEDWINDOW | WS_VISIBLE).0 as isize,
            );
            let r = state.restore;
            let _ = SetWindowPos(
                hwnd,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_FRAMECHANGED | SWP_NOZORDER,
            );
            state.fullscreen = false;
        } else {
            let _ = GetWindowRect(hwnd, &mut state.restore);
            let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(monitor, &mut info).as_bool() {
                let m = info.rcMonitor;
                SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    m.left,
                    m.top,
                    m.right - m.left,
                    m.bottom - m.top,
                    SWP_FRAMECHANGED | SWP_NOZORDER,
                );
                state.fullscreen = true;
            }
        }
    }
}

fn point(lparam: LPARAM) -> (i32, i32) {
    let value = lparam.0 as u32;
    ((value & 0xffff) as i16 as i32, (value >> 16) as i16 as i32)
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowState;
    let Some(state) = (unsafe { state.as_mut() }) else {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    };
    let window = state.window;
    let pointer = |state: &mut WindowState, lparam: LPARAM| {
        let (x, y) = point(lparam);
        if let Some((x, y)) = present::to_picture(state.picture, state.client, x, y) {
            state.input.push(InputEvent::PointerMove { window, x, y });
        }
    };
    let button = |state: &mut WindowState, button: PointerButton, pressed: bool| {
        pointer(state, lparam);
        state.input.push(InputEvent::PointerButton {
            window,
            button,
            pressed,
        });
        unsafe {
            if pressed {
                SetCapture(hwnd);
            } else {
                let _ = ReleaseCapture();
            }
        }
    };
    match message {
        WM_CLOSE => {
            state.closed = true;
            return LRESULT(0);
        }
        WM_KEYDOWN | WM_SYSKEYDOWN if wparam.0 as u16 == VK_F11.0 => {
            state.toggle_fullscreen = true;
            return LRESULT(0);
        }
        WM_LBUTTONDBLCLK if !state.send_input => {
            state.toggle_fullscreen = true;
            return LRESULT(0);
        }
        _ => {}
    }
    if state.send_input {
        match message {
            WM_MOUSEMOVE => pointer(state, lparam),
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => button(state, PointerButton::Left, true),
            WM_LBUTTONUP => button(state, PointerButton::Left, false),
            WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => button(state, PointerButton::Right, true),
            WM_RBUTTONUP => button(state, PointerButton::Right, false),
            WM_MBUTTONDOWN | WM_MBUTTONDBLCLK => button(state, PointerButton::Middle, true),
            WM_MBUTTONUP => button(state, PointerButton::Middle, false),
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let notches = ((wparam.0 >> 16) as u16 as i16) as f32 / WHEEL_DELTA as f32;
                let (dx, dy) = if message == WM_MOUSEWHEEL {
                    (0.0, notches)
                } else {
                    (notches, 0.0)
                };
                state
                    .input
                    .push(InputEvent::PointerScroll { window, dx, dy });
            }
            WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
                let flags = lparam.0 as u32;
                let scan = (flags >> 16) & 0xff;
                let extended = flags & (1 << 24) != 0;
                if let Some(keycode) =
                    windowcast_protocol::keys::evdev_from_scan(scan as u16, extended)
                {
                    let pressed = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
                    state.input.push(InputEvent::Key { keycode, pressed });
                }
                // Alt and F10 would open the (absent) window menu.
                return LRESULT(0);
            }
            _ => {}
        }
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

//! A locked PC keeps streaming its windows (docs/BACKENDS.md, "A locked
//! host"; Droidtop/tracker#467): a window that repaints all the time is
//! captured, the workstation is locked, and the test checks that its own
//! capture keeps delivering changing pictures (one started while locked
//! too), that the agent opens streams of it on the native and desktop
//! backends instead of refusing, that the agent reports the PC locked, and
//! that input reaches the window as posted messages (SendInput is refused
//! while the lock screen has the input). Locking cannot be undone without
//! the user's password, so it runs only where `WINDOWCAST_TEST_LOCK` is
//! set (the last step of a CI job) and is skipped elsewhere.
#![cfg(windows)]

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windowcast_agent_windows::capture::{self, Capture};
use windowcast_agent_windows::convert::Picture;
use windowcast_agent_windows::lock;
use windowcast_agent_windows::{Options, WindowsSource};
use windowcast_host::WindowSource;
use windowcast_protocol::{InputEvent, PointerButton, VideoCodec, WindowId};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Paints so far: the window's colour steps with each.
static PAINTS: AtomicU32 = AtomicU32::new(0);
/// Characters the window received (WM_CHAR).
static CHARS: AtomicU32 = AtomicU32::new(0);
/// The last WM_KEYDOWN: its key code in the high half, lParam in the low.
static KEY: AtomicU64 = AtomicU64::new(0);
/// Left button presses and releases, and the last press's lParam.
static PRESSES: AtomicU32 = AtomicU32::new(0);
static RELEASES: AtomicU32 = AtomicU32::new(0);
static PRESSED_AT: AtomicU64 = AtomicU64::new(0);
/// Wheel messages.
static WHEELS: AtomicU32 = AtomicU32::new(0);

extern "system" fn paint(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match message {
            WM_TIMER => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_PAINT => {
                let n = PAINTS.fetch_add(1, Ordering::SeqCst);
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                let level = n * 16 % 256;
                let brush = CreateSolidBrush(COLORREF(level | 0x40 << 8 | (255 - level) << 16));
                FillRect(dc, &ps.rcPaint, brush);
                let _ = DeleteObject(brush.into());
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_CHAR => {
                CHARS.fetch_add(1, Ordering::SeqCst);
                LRESULT(0)
            }
            WM_KEYDOWN => {
                KEY.store(
                    (wparam.0 as u64) << 32 | (lparam.0 as u64 & 0xffff_ffff),
                    Ordering::SeqCst,
                );
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                PRESSES.fetch_add(1, Ordering::SeqCst);
                PRESSED_AT.store(lparam.0 as u64, Ordering::SeqCst);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                RELEASES.fetch_add(1, Ordering::SeqCst);
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                WHEELS.fetch_add(1, Ordering::SeqCst);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }
}

/// A 320x240 window at (100, 100) that repaints every 50 ms, on a thread
/// of its own with a message loop that translates keys into characters.
fn open_window() -> (HWND, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || unsafe {
        let instance = GetModuleHandleW(None).unwrap();
        let class = WNDCLASSW {
            lpfnWndProc: Some(paint),
            hInstance: instance.into(),
            lpszClassName: w!("windowcast-lock-window"),
            ..Default::default()
        };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("windowcast-lock-window"),
            w!("windowcast lock test window"),
            WS_POPUP,
            100,
            100,
            320,
            240,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .unwrap();
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(Some(hwnd), 1, 50, None);
        tx.send(hwnd.0 as usize).unwrap();
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    });
    (HWND(rx.recv().unwrap() as *mut _), thread)
}

/// The BGRA at the picture's centre.
fn centre(capture: &Capture) -> Option<[u8; 4]> {
    match capture.picture()? {
        Picture::Bgra(p) => {
            let at = (p.height / 2) * p.stride + (p.width / 2) * 4;
            Some([p.data[at], p.data[at + 1], p.data[at + 2], p.data[at + 3]])
        }
        _ => None,
    }
}

/// Pictures over `time`: how many arrived and how many times the centre
/// changed.
fn watch(name: &str, capture: &mut Capture, time: Duration) -> (usize, usize) {
    let end = Instant::now() + time;
    let mut pictures = 0;
    let mut centres = Vec::new();
    while Instant::now() < end {
        match capture.poll(Duration::from_millis(200)) {
            Ok(true) => {
                pictures += 1;
                if let Some(c) = centre(capture) {
                    if centres.last() != Some(&c) {
                        centres.push(c);
                    }
                }
            }
            Ok(false) => {}
            Err(e) => panic!("{name}: capture failed: {e}"),
        }
    }
    println!(
        "{name}: {pictures} pictures in {time:?}, {} centre changes",
        centres.len()
    );
    (pictures, centres.len())
}

/// Waits up to 2 s for `done`.
fn settle(done: impl Fn() -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(2);
    while !done() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}

#[test]
fn a_locked_session_keeps_streaming_windows_and_takes_input_as_messages() {
    if std::env::var_os("WINDOWCAST_TEST_LOCK").is_none() {
        println!("skipped: locks the session; set WINDOWCAST_TEST_LOCK to run");
        return;
    }
    capture::init_thread();
    let (hwnd, window_thread) = open_window();
    let id = WindowId(hwnd.0 as usize as u64);
    std::thread::sleep(Duration::from_millis(500));
    let source = WindowsSource::new(Options::default()).expect("the agent's source");
    assert!(
        lock::input_desktop_reachable(),
        "the input desktop at the start"
    );
    assert!(!source.host_state().locked, "locked before locking");

    let mut window = Capture::window(hwnd).expect("window capture");
    window.set_bgra_output();
    let (before, _) = watch("unlocked window", &mut window, Duration::from_secs(2));
    assert!(before > 0, "no pictures before locking");

    unsafe { windows::Win32::System::Shutdown::LockWorkStation() }.expect("LockWorkStation");
    let since = Instant::now();
    while lock::input_desktop_reachable() && since.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !lock::input_desktop_reachable(),
        "the input desktop is still in reach 10 s after locking"
    );
    println!("locked after {:?}", since.elapsed());
    let since = Instant::now();
    while !source.host_state().locked && since.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        source.host_state().locked,
        "the agent does not say it is locked"
    );
    std::thread::sleep(Duration::from_secs(1));

    // The window's own capture goes on, and starts while locked.
    let (pictures, changes) = watch("locked window", &mut window, Duration::from_secs(4));
    assert!(
        pictures >= 10 && changes >= 10,
        "the window's capture stalled while locked"
    );
    let mut fresh = Capture::window(hwnd).expect("window capture started while locked");
    fresh.set_bgra_output();
    let (pictures, changes) = watch(
        "window capture started while locked",
        &mut fresh,
        Duration::from_secs(4),
    );
    assert!(
        pictures >= 10 && changes >= 10,
        "a capture started while locked stalled"
    );
    drop(fresh);
    drop(window);

    // The agent streams it while locked, on the desktop backend too (which
    // serves the window's own capture then).
    for desktop in [false, true] {
        let name = if desktop { "desktop" } else { "native" };
        let mut stream = if desktop {
            source.open_desktop(id, VideoCodec::H264)
        } else {
            source.open(id, VideoCodec::H264)
        }
        .unwrap_or_else(|e| panic!("{name} stream refused while locked: {e}"));
        let mut frames = 0;
        let until = Instant::now() + Duration::from_secs(10);
        while frames < 10 && Instant::now() < until {
            let frame = stream
                .next_frame(frames == 0)
                .unwrap_or_else(|| panic!("the {name} stream ended while locked"));
            assert!(!frame.data.is_empty());
            frames += 1;
        }
        println!("{name} stream while locked: {frames} frames");
        assert_eq!(frames, 10, "the {name} stream stalled while locked");
    }

    // Input reaches the window as messages.
    let focus = Some(id);
    source.input(
        &InputEvent::PointerMove {
            window: id,
            x: 0.5,
            y: 0.5,
        },
        focus,
    );
    for pressed in [true, false] {
        source.input(
            &InputEvent::PointerButton {
                window: id,
                button: PointerButton::Left,
                pressed,
            },
            focus,
        );
    }
    assert!(
        settle(|| PRESSES.load(Ordering::SeqCst) == 1 && RELEASES.load(Ordering::SeqCst) == 1),
        "a click while locked: {} presses, {} releases",
        PRESSES.load(Ordering::SeqCst),
        RELEASES.load(Ordering::SeqCst)
    );
    let at = PRESSED_AT.load(Ordering::SeqCst);
    let (x, y) = (at & 0xffff, at >> 16 & 0xffff);
    println!("click while locked at ({x}, {y})");
    assert!(
        x.abs_diff(160) <= 2 && y.abs_diff(120) <= 2,
        "a click at the window's centre landed at ({x}, {y})"
    );

    let chars = CHARS.load(Ordering::SeqCst);
    for pressed in [true, false] {
        // evdev 30 is A.
        source.input(
            &InputEvent::Key {
                keycode: 30,
                pressed,
            },
            focus,
        );
    }
    assert!(
        settle(|| KEY.load(Ordering::SeqCst) != 0),
        "no key reached the window while locked"
    );
    let key = KEY.load(Ordering::SeqCst);
    println!(
        "key while locked: code {:#x}, lParam {:#x}",
        key >> 32,
        key & 0xffff_ffff
    );
    assert_eq!(key >> 32, 0x41, "the key's code");
    assert_eq!(key >> 16 & 0xff, 0x1e, "the key's scan code");
    assert!(
        settle(|| CHARS.load(Ordering::SeqCst) == chars + 1),
        "the key did not make a character while locked"
    );

    source.input(&InputEvent::Text { text: "hi".into() }, focus);
    assert!(
        settle(|| CHARS.load(Ordering::SeqCst) == chars + 3),
        "typed text while locked: {} characters",
        CHARS.load(Ordering::SeqCst) - chars - 1
    );

    source.input(
        &InputEvent::PointerScroll {
            window: id,
            dx: 0.0,
            dy: 1.0,
        },
        focus,
    );
    assert!(
        settle(|| WHEELS.load(Ordering::SeqCst) == 1),
        "the wheel did not reach the window while locked"
    );

    // Close everything before the process ends: the window on its own
    // thread, which then leaves its message loop.
    drop(source);
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    window_thread.join().expect("the window's thread");
}

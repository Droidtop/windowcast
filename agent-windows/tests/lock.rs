//! What window capture does while the session is locked (Droidtop/tracker#467):
//! a window that repaints all the time is captured with
//! Windows.Graphics.Capture, the workstation is locked, and the test reports
//! whether pictures keep coming, whether they change, whether a capture
//! started while locked gets a picture, and what a capture of the whole
//! screen shows. Locking cannot be undone without the user's password, so
//! it runs only where `WINDOWCAST_TEST_LOCK` is set (the last step of a CI
//! job) and is skipped elsewhere.
#![cfg(windows)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use windowcast_agent_windows::capture::{self, Capture};
use windowcast_agent_windows::convert::Picture;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenInputDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_SWITCHDESKTOP,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Paints so far: the window's colour steps with each.
static PAINTS: AtomicU32 = AtomicU32::new(0);
/// Characters the window received.
static CHARS: AtomicU32 = AtomicU32::new(0);

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
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }
}

fn open_window() -> HWND {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || unsafe {
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
    HWND(rx.recv().unwrap() as *mut _)
}

fn input_desktop_reachable() -> bool {
    match unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_SWITCHDESKTOP) } {
        Ok(desktop) => {
            let _ = unsafe { CloseDesktop(desktop) };
            true
        }
        Err(_) => false,
    }
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

/// Pictures over `time`: how many arrived and how many distinct centres.
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
            Err(e) => {
                println!("{name}: capture failed: {e}");
                break;
            }
        }
    }
    println!(
        "{name}: {pictures} pictures in {time:?}, {} centre changes, last centre {:?}",
        centres.len(),
        centres.last()
    );
    (pictures, centres.len())
}

#[test]
fn window_capture_while_the_session_is_locked() {
    if std::env::var_os("WINDOWCAST_TEST_LOCK").is_none() {
        println!("skipped: locks the session; set WINDOWCAST_TEST_LOCK to run");
        return;
    }
    capture::init_thread();
    let hwnd = open_window();
    std::thread::sleep(Duration::from_millis(500));
    let mut window = Capture::window(hwnd).expect("window capture");
    window.set_bgra_output();
    let mut screen = Capture::desktop(hwnd).expect("screen capture");
    screen.set_bgra_output();
    println!(
        "unlocked: input desktop reachable {}",
        input_desktop_reachable()
    );
    let (before, _) = watch("unlocked window", &mut window, Duration::from_secs(2));
    watch("unlocked screen", &mut screen, Duration::from_secs(2));
    assert!(before > 0, "no pictures before locking");

    let paints = PAINTS.load(Ordering::SeqCst);
    unsafe { windows::Win32::System::Shutdown::LockWorkStation() }.expect("LockWorkStation");
    let since = Instant::now();
    while input_desktop_reachable() && since.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(100));
    }
    println!(
        "locked after {:?}: input desktop reachable {}",
        since.elapsed(),
        input_desktop_reachable()
    );
    std::thread::sleep(Duration::from_secs(1));
    watch("locked window", &mut window, Duration::from_secs(4));
    watch("locked screen", &mut screen, Duration::from_secs(4));
    let mut fresh = Capture::window(hwnd).expect("window capture while locked");
    fresh.set_bgra_output();
    watch(
        "window capture started while locked",
        &mut fresh,
        Duration::from_secs(4),
    );
    match Capture::desktop(hwnd) {
        Ok(mut s) => {
            s.set_bgra_output();
            watch(
                "screen capture started while locked",
                &mut s,
                Duration::from_secs(4),
            );
        }
        Err(e) => println!("screen capture started while locked: {e}"),
    }
    println!(
        "the window painted {} times while locked",
        PAINTS.load(Ordering::SeqCst) - paints
    );
    // PrintWindow with PW_RENDERFULLCONTENT, twice, half a second apart.
    let first = print_window_centre(hwnd);
    std::thread::sleep(Duration::from_millis(500));
    let second = print_window_centre(hwnd);
    println!("PrintWindow while locked: {first:?} then {second:?}");

    // Input while locked: SendInput (to the input desktop) and messages
    // posted to the window itself.
    let chars = CHARS.load(Ordering::SeqCst);
    let sent = send_key();
    std::thread::sleep(Duration::from_millis(300));
    println!(
        "SendInput while locked: {sent} events taken, window got {} characters",
        CHARS.load(Ordering::SeqCst) - chars
    );
    let chars = CHARS.load(Ordering::SeqCst);
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_CHAR, WPARAM('a' as usize), LPARAM(1));
    }
    std::thread::sleep(Duration::from_millis(300));
    println!(
        "PostMessage WM_CHAR while locked: window got {} characters",
        CHARS.load(Ordering::SeqCst) - chars
    );
}

/// The centre of the window as PrintWindow draws it, and whether it drew.
fn print_window_centre(hwnd: HWND) -> (bool, [u8; 4]) {
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, 320, 240);
        let old = SelectObject(dc, bitmap.into());
        let drew = PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(2)).as_bool();
        let pixel = GetPixel(dc, 160, 120);
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        let c = pixel.0;
        (drew, [(c >> 16) as u8, (c >> 8) as u8, c as u8, 0])
    }
}

/// One 'a' key press and release through SendInput: how many it took.
fn send_key() -> u32 {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0x41),
                dwFlags: flags,
                ..Default::default()
            },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) }
}

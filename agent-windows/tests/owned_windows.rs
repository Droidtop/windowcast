//! Window by window (docs/BACKENDS.md): a program's dialog and its
//! drop-down are window-list entries of their own, each naming its owner,
//! and each is captured on its own: the drop-down, a tool window that
//! Windows.Graphics.Capture will not capture by itself, from its part of
//! the screen. Needs the windows on screen (`WINDOWCAST_TEST_ON_SCREEN`,
//! set by CI); skipped otherwise.
#![cfg(windows)]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use windowcast_agent_windows::{Options, WindowsSource};
use windowcast_host::WindowSource;
use windowcast_protocol::{WindowId, WindowInfo, WindowKind};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, EndPaint, FillRect, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Each window's colour, by its user data (set at creation).
const COLOURS: [(u8, u8, u8); 3] = [(30, 160, 220), (220, 60, 40), (40, 200, 90)];

extern "system" fn paint(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match message {
            WM_PAINT => {
                let which = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as usize;
                let (r, g, b) = COLOURS[which.min(2)];
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                let brush =
                    CreateSolidBrush(COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16));
                FillRect(dc, &ps.rcPaint, brush);
                let _ = EndPaint(hwnd, &ps);
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

/// A program's window, a dialog it owns and a drop-down (an owned tool
/// window without a title), on their own thread.
fn open_windows() -> ([u64; 3], std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || unsafe {
        let instance = GetModuleHandleW(None).unwrap();
        let class = WNDCLASSW {
            lpfnWndProc: Some(paint),
            hInstance: instance.into(),
            lpszClassName: w!("windowcast-owned-test"),
            ..Default::default()
        };
        RegisterClassW(&class);
        let make = |ex: WINDOW_EX_STYLE,
                    title: PCWSTR,
                    style: WINDOW_STYLE,
                    (x, y, w, h): (i32, i32, i32, i32),
                    owner: Option<HWND>,
                    which: isize| {
            let hwnd = CreateWindowExW(
                ex,
                w!("windowcast-owned-test"),
                title,
                style,
                x,
                y,
                w,
                h,
                owner,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, which);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = windows::Win32::Graphics::Gdi::UpdateWindow(hwnd);
            hwnd
        };
        let main = make(
            WINDOW_EX_STYLE(0),
            w!("windowcast owner test"),
            WS_POPUP,
            (100, 100, 320, 240),
            None,
            0,
        );
        let dialog = make(
            WINDOW_EX_STYLE(0),
            w!("windowcast owned dialog"),
            WS_POPUP | WS_CAPTION,
            (480, 100, 240, 180),
            Some(main),
            1,
        );
        let dropdown = make(
            WS_EX_TOOLWINDOW,
            PCWSTR::null(),
            WS_POPUP,
            (100, 380, 200, 120),
            Some(main),
            2,
        );
        tx.send([main, dialog, dropdown].map(|h| h.0 as usize as u64))
            .unwrap();
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    });
    (rx.recv().unwrap(), thread)
}

fn find(list: &[WindowInfo], id: u64) -> Option<&WindowInfo> {
    list.iter().find(|w| w.id == WindowId(id))
}

#[test]
fn owned_windows_are_listed_and_captured_on_their_own() {
    if std::env::var_os("WINDOWCAST_TEST_ON_SCREEN").is_none() {
        println!("skipped: set WINDOWCAST_TEST_ON_SCREEN (the windows must be on screen)");
        return;
    }
    let ([main, dialog, dropdown], thread) = open_windows();
    let source = WindowsSource::new(Options::default()).unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let list = loop {
        let list = source.list_windows();
        if [main, dialog, dropdown]
            .iter()
            .all(|id| find(&list, *id).is_some())
        {
            break list;
        }
        assert!(
            Instant::now() < deadline,
            "not all three listed: {:?}",
            list.iter()
                .map(|w| (w.id.0, &w.title, w.kind, w.owner))
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    let (m, d, p) = (
        find(&list, main).unwrap(),
        find(&list, dialog).unwrap(),
        find(&list, dropdown).unwrap(),
    );
    println!("{m:?}\n{d:?}\n{p:?}");
    assert_eq!((m.kind, m.owner), (WindowKind::Normal, None));
    assert_eq!(
        (d.kind, d.owner),
        (WindowKind::Dialog, Some(WindowId(main)))
    );
    assert_eq!((p.kind, p.owner), (WindowKind::Popup, Some(WindowId(main))));
    assert_eq!(p.title, "windowcast owner test popup");

    // Each one's own picture, in its own colour: the drop-down from its part
    // of the screen.
    for (id, which) in [(main, 0), (dialog, 1), (dropdown, 2)] {
        let mut pictures = source
            .open_pictures(WindowId(id))
            .expect("pictures")
            .expect("a capture");
        let picture = pictures.next_picture().expect("a picture");
        let centre =
            (picture.height as usize / 2) * picture.stride + (picture.width as usize / 2) * 4;
        let (b, g, r) = (
            picture.data[centre],
            picture.data[centre + 1],
            picture.data[centre + 2],
        );
        let want = COLOURS[which];
        println!(
            "window {id}: {}x{}, centre {:?}",
            picture.width,
            picture.height,
            (r, g, b)
        );
        let near = |a: u8, b: u8| (a as i32 - b as i32).abs() <= 12;
        assert!(
            near(r, want.0) && near(g, want.1) && near(b, want.2),
            "window {id}: centre {:?}, wanted {want:?}",
            (r, g, b)
        );
    }

    unsafe {
        let _ = PostMessageW(
            Some(HWND(main as usize as *mut core::ffi::c_void)),
            WM_CLOSE,
            WPARAM(0),
            LPARAM(0),
        );
    }
    let _ = thread.join();
}

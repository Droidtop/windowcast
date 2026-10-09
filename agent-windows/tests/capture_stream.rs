//! A real window, captured with Windows.Graphics.Capture, encoded by each
//! encoder this machine has, streamed over loopback through the host and
//! client libraries, and decoded: the centre pixel must be the window's
//! colour.
//!
//! The test window is placed on screen only when `WINDOWCAST_TEST_ON_SCREEN`
//! is set (CI); otherwise it sits off screen. It is a plain popup:
//! Windows.Graphics.Capture refused it as a tool window and as a
//! WS_EX_NOACTIVATE window ("Could not capture the given window",
//! 0x80070057, both seen in CI).
#![cfg(windows)]

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use windowcast_agent_windows::encoder::{self, EncoderChoice};
use windowcast_agent_windows::{Options, WindowsSource};
use windowcast_cli_tools::H264Check;
use windowcast_client::{Client, Event, FramePoll};
use windowcast_host::HostConfig;
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{VideoCodec, WindowId};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, EndPaint, FillRect, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

const WAIT: Duration = Duration::from_secs(20);
const WIDTH: i32 = 320;
const HEIGHT: i32 = 240;
/// The window's colour, and its BT.601 limited-range Y, U, V.
const RGB: (u8, u8, u8) = (30, 160, 220);
const YUV: (i32, i32, i32) = (126, 173, 67);

extern "system" fn paint(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match message {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                let brush = CreateSolidBrush(COLORREF(
                    RGB.0 as u32 | (RGB.1 as u32) << 8 | (RGB.2 as u32) << 16,
                ));
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

/// Opens the test window on its own thread (with its message loop) and
/// returns its handle as a window id.
fn open_test_window() -> (u64, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || unsafe {
        let instance = GetModuleHandleW(None).unwrap();
        let class = WNDCLASSW {
            lpfnWndProc: Some(paint),
            hInstance: instance.into(),
            lpszClassName: w!("windowcast-test-window"),
            ..Default::default()
        };
        RegisterClassW(&class);
        let (x, y) = if std::env::var_os("WINDOWCAST_TEST_ON_SCREEN").is_some() {
            (100, 100)
        } else {
            (-4000, -4000)
        };
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("windowcast-test-window"),
            w!("windowcast test window"),
            WS_POPUP,
            x,
            y,
            WIDTH,
            HEIGHT,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .unwrap();
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = windows::Win32::Graphics::Gdi::UpdateWindow(hwnd);
        tx.send(hwnd.0 as usize as u64).unwrap();
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    });
    (rx.recv().unwrap(), thread)
}

fn close_window(id: u64) {
    unsafe {
        let _ = PostMessageW(
            Some(HWND(id as usize as *mut core::ffi::c_void)),
            WM_CLOSE,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("windowcast-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Streams the test window with one encoder choice; returns what decoded.
fn stream_with(choice: EncoderChoice, window: u64) -> H264Check {
    let host_dir = temp_dir(&format!("host-{choice:?}"));
    let client_dir = temp_dir(&format!("client-{choice:?}"));
    let host_id = Identity::load_or_generate(&host_dir.join("agent-identity.key"))
        .unwrap()
        .peer_id();
    let client_id = Identity::load_or_generate(&client_dir.join("client-identity.key"))
        .unwrap()
        .peer_id();
    let mut trust = TrustStore::default();
    trust.pin(client_id);
    trust.save(&host_dir.join("agent-trusted-clients")).unwrap();
    let mut trust = TrustStore::default();
    trust.pin(host_id);
    trust
        .save(&client_dir.join("client-trusted-hosts"))
        .unwrap();

    let source = WindowsSource::new(Options {
        encoder: choice,
        ..Options::default()
    })
    .unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let address = listener.local_addr().unwrap().to_string();
    runtime.spawn(windowcast_host::serve(
        listener,
        HostConfig {
            listen: address.clone(),
            pairing: false,
            data_dir: host_dir.clone(),
        },
        Arc::new(source),
    ));

    let client = Client::new(&client_dir).unwrap();
    let session = client.connect(&address, None).unwrap();
    session
        .start_window(WindowId(window), &[VideoCodec::H264])
        .unwrap();
    match session.next_event(WAIT) {
        Some(Event::StreamStarted { .. }) => {}
        other => panic!("{choice:?}: expected the stream to start, got {other:?}"),
    }

    let mut check = H264Check::new().unwrap();
    for n in 0..3 {
        match session.next_frame(WindowId(window), WAIT) {
            FramePoll::Frame(frame) => {
                if n == 0 {
                    assert!(
                        frame.keyframe,
                        "{choice:?}: the first frame must be a keyframe"
                    );
                }
                check.decode(&frame.data).unwrap();
            }
            FramePoll::Timeout => panic!("{choice:?}: frame {n} did not arrive"),
            FramePoll::Ended => panic!("{choice:?}: the stream ended at frame {n}"),
        }
    }
    session.stop_window(WindowId(window)).unwrap();
    drop(session);
    drop(client);
    runtime.shutdown_timeout(Duration::from_secs(1));
    let _ = std::fs::remove_dir_all(host_dir);
    let _ = std::fs::remove_dir_all(client_dir);
    check
}

#[test]
fn a_window_is_captured_encoded_streamed_and_decoded() {
    let (window, thread) = open_test_window();
    // Give DWM a moment to compose the new window.
    std::thread::sleep(Duration::from_millis(500));

    let mut choices = vec![EncoderChoice::OpenH264, EncoderChoice::MfSoftware];
    if encoder::available_codecs(EncoderChoice::MfHardware).contains(&VideoCodec::H264) {
        choices.push(EncoderChoice::MfHardware);
    }
    for line in encoder::list() {
        println!("{line}");
    }
    for choice in choices {
        let check = stream_with(choice, window);
        println!(
            "{choice:?}: {} pictures at {:?}, centre YUV {:?}",
            check.pictures, check.dimensions, check.center
        );
        assert!(check.pictures >= 1, "{choice:?}: nothing decoded");
        assert_eq!(check.dimensions, Some((WIDTH as usize, HEIGHT as usize)));
        let (y, u, v) = check.center.unwrap();
        for (got, want, name) in [(y, YUV.0, "Y"), (u, YUV.1, "U"), (v, YUV.2, "V")] {
            assert!(
                (i32::from(got) - want).abs() <= 12,
                "{choice:?}: centre {name} is {got}, expected about {want}"
            );
        }
    }

    close_window(window);
    thread.join().unwrap();
}

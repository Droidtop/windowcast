//! Input from a client reaching a real window through SendInput, and the
//! clipboard both ways, end to end: a client clicks into the streamed
//! window, presses A and types "é"; the window's own message handler must
//! see the click at the right place, the key and both characters. Needs an
//! interactive desktop, so it runs where `WINDOWCAST_TEST_ON_SCREEN` is set
//! (CI) and is skipped elsewhere: it moves the real pointer and types.
#![cfg(windows)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use windowcast_agent_windows::{Options, WindowsSource};
use windowcast_client::{Client, Event, FramePoll};
use windowcast_host::HostConfig;
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{InputEvent, PointerButton, VideoCodec, WindowId};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

const WAIT: Duration = Duration::from_secs(20);
const WIDTH: i32 = 320;
const HEIGHT: i32 = 240;

static CLICK_X: AtomicI32 = AtomicI32::new(-1);
static CLICK_Y: AtomicI32 = AtomicI32::new(-1);
static KEY_DOWN: AtomicU32 = AtomicU32::new(0);
static CHARS: Mutex<Vec<u16>> = Mutex::new(Vec::new());

extern "system" fn record(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match message {
            WM_LBUTTONDOWN => {
                CLICK_X.store((lparam.0 & 0xffff) as i16 as i32, Ordering::SeqCst);
                CLICK_Y.store(((lparam.0 >> 16) & 0xffff) as i16 as i32, Ordering::SeqCst);
                LRESULT(0)
            }
            WM_KEYDOWN => {
                // The first key only: typed text arrives as VK_PACKET keys.
                let _ = KEY_DOWN.compare_exchange(
                    0,
                    wparam.0 as u32,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                DefWindowProcW(hwnd, message, wparam, lparam)
            }
            WM_CHAR => {
                CHARS.lock().unwrap().push(wparam.0 as u16);
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

fn open_window() -> (u64, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || unsafe {
        let instance = GetModuleHandleW(None).unwrap();
        let class = WNDCLASSW {
            lpfnWndProc: Some(record),
            hInstance: instance.into(),
            lpszClassName: w!("windowcast-input-test"),
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(
                windows::Win32::Graphics::Gdi::GetStockObject(
                    windows::Win32::Graphics::Gdi::WHITE_BRUSH,
                )
                .0,
            ),
            ..Default::default()
        };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("windowcast-input-test"),
            w!("windowcast input test"),
            WS_POPUP,
            200,
            150,
            WIDTH,
            HEIGHT,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .unwrap();
        let _ = ShowWindow(hwnd, SW_SHOW);
        tx.send(hwnd.0 as usize as u64).unwrap();
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    });
    (rx.recv().unwrap(), thread)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("windowcast-winput-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_for(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_client_clicks_and_types_into_a_real_window() {
    if std::env::var_os("WINDOWCAST_TEST_ON_SCREEN").is_none() {
        println!("skipped: moves the real pointer; set WINDOWCAST_TEST_ON_SCREEN to run");
        return;
    }
    let (window, thread) = open_window();
    std::thread::sleep(Duration::from_millis(500));

    let host_dir = temp_dir("host");
    let client_dir = temp_dir("client");
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

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let source = WindowsSource::new(Options::default()).unwrap();
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
    let id = WindowId(window);
    session.start_window(id, &[VideoCodec::H264]).unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(Instant::now() < deadline, "the stream did not start");
        if let Some(Event::StreamStarted { .. }) = session.next_event(Duration::from_millis(200)) {
            break;
        }
    }
    assert!(matches!(session.next_frame(id, WAIT), FramePoll::Frame(_)));

    // A click a quarter of the way across, half way down.
    for event in [
        InputEvent::PointerMove {
            window: id,
            x: 0.25,
            y: 0.5,
        },
        InputEvent::PointerButton {
            window: id,
            button: PointerButton::Left,
            pressed: true,
        },
        InputEvent::PointerButton {
            window: id,
            button: PointerButton::Left,
            pressed: false,
        },
    ] {
        session.send_input(event).unwrap();
    }
    wait_for("the click", || CLICK_X.load(Ordering::SeqCst) >= 0);
    let (x, y) = (
        CLICK_X.load(Ordering::SeqCst),
        CLICK_Y.load(Ordering::SeqCst),
    );
    println!("click arrived at ({x}, {y})");
    assert!((x - (WIDTH - 1) / 4).abs() <= 2 && (y - (HEIGHT - 1) / 2).abs() <= 2);

    // A (evdev 30), then "é" as text.
    for event in [
        InputEvent::Key {
            keycode: 30,
            pressed: true,
        },
        InputEvent::Key {
            keycode: 30,
            pressed: false,
        },
        InputEvent::Text { text: "é".into() },
    ] {
        session.send_input(event).unwrap();
    }
    wait_for("the typing", || CHARS.lock().unwrap().len() >= 2);
    let chars = String::from_utf16_lossy(&CHARS.lock().unwrap());
    println!(
        "key down {:#x}, characters {chars:?}",
        KEY_DOWN.load(Ordering::SeqCst)
    );
    assert_eq!(KEY_DOWN.load(Ordering::SeqCst), 0x41, "A's virtual key");
    assert_eq!(chars.to_lowercase(), "aé");

    // Clipboard: client to host, then host to client.
    session.set_clipboard("from the client").unwrap();
    wait_for("the host clipboard", || {
        windowcast_agent_windows::clipboard::text().as_deref() == Some("from the client")
    });
    windowcast_agent_windows::clipboard::set_text("from the host");
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(
            Instant::now() < deadline,
            "the host clipboard did not arrive"
        );
        if let Some(Event::Clipboard { text }) = session.next_event(Duration::from_millis(200)) {
            assert_eq!(text, "from the host");
            break;
        }
    }

    session.stop_window(id).unwrap();
    drop(session);
    drop(client);
    runtime.shutdown_timeout(Duration::from_secs(1));
    unsafe {
        let _ = PostMessageW(
            Some(HWND(window as usize as *mut core::ffi::c_void)),
            WM_CLOSE,
            WPARAM(0),
            LPARAM(0),
        );
    }
    thread.join().unwrap();
    let _ = std::fs::remove_dir_all(host_dir);
    let _ = std::fs::remove_dir_all(client_dir);
}

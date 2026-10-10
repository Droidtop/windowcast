//! Our RDP client against our RDP host on loopback: the host serves the
//! test pattern window with a login, the client logs in (TLS and NLA,
//! the host's certificate pinned), sees the pattern move, and its keys and
//! pointer reach the window. A wrong password is refused.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::{frame_number, TestPatternSource, HEIGHT, WIDTH};
use windowcast_host::{FrameSource, PictureSource, WindowSource};
use windowcast_protocol::{InputEvent, PointerButton, VideoCodec, WindowId, WindowInfo};
use windowcast_rdp::client::{connect, ClientConfig};
use windowcast_rdp::host::serve_window;
use windowcast_rdp::tls::HostIdentity;
use windowcast_rdp::Credentials;

/// The test pattern, keeping what input reaches it.
#[derive(Default)]
struct Recording {
    events: Arc<Mutex<Vec<InputEvent>>>,
}

impl WindowSource for Recording {
    fn list_windows(&self) -> Vec<WindowInfo> {
        TestPatternSource.list_windows()
    }
    fn encoders(&self) -> Vec<VideoCodec> {
        TestPatternSource.encoders()
    }
    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        TestPatternSource.open(window, codec)
    }
    fn open_pictures(&self, window: WindowId) -> Option<Result<Box<dyn PictureSource>, String>> {
        TestPatternSource.open_pictures(window)
    }
    fn input(&self, event: &InputEvent, _focus: Option<WindowId>) {
        self.events.lock().unwrap().push(event.clone());
    }
}

#[test]
fn our_client_sees_and_drives_a_window_over_our_rdp_host() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let identity = HostIdentity::generate("windowcast test host").unwrap();
    let fingerprint = identity.fingerprint();
    let recording = Recording::default();
    let events = Arc::clone(&recording.events);
    let source: Arc<dyn WindowSource> = Arc::new(recording);
    let window = source.list_windows()[0].id;
    std::thread::spawn(move || {
        let credentials = Credentials {
            username: "windowcast".into(),
            password: "a one-time password".into(),
            domain: None,
        };
        let stats = Arc::new(windowcast_rdp::host::HostStats::default());
        if let Err(e) = serve_window(listener, source, window, credentials, &identity, stats) {
            eprintln!("host: {e}");
        }
    });

    let config = |password: &str| ClientConfig {
        address,
        server_name: "localhost".into(),
        username: "windowcast".into(),
        password: password.into(),
        domain: None,
        size: (1024, 768),
        pinned: Some(fingerprint),
    };
    assert!(
        connect(&config("not it")).is_err(),
        "a wrong password must be refused"
    );

    let stream = connect(&config("a one-time password")).unwrap();
    assert_eq!(
        stream.size,
        (WIDTH as u16, HEIGHT as u16),
        "the desktop is the window"
    );
    assert_eq!(stream.fingerprint, fingerprint);

    // The pattern moves: frame numbers go up across pictures.
    let mut numbers = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = None;
    while numbers.len() < 10 && Instant::now() < deadline {
        if let Ok(picture) = stream.pictures.recv_timeout(Duration::from_millis(500)) {
            assert_eq!(
                (picture.width, picture.height),
                (WIDTH as u32, HEIGHT as u32)
            );
            numbers.push(frame_number(&picture.data, WIDTH));
            last = Some(picture);
        }
    }
    println!("frame numbers seen: {numbers:?}");
    assert!(numbers.len() >= 10, "only {} pictures", numbers.len());
    assert!(numbers.last() > numbers.first(), "the picture did not move");
    // A ramp pixel survived the codec: grey, bright on the right.
    let picture = last.unwrap();
    let at = ((HEIGHT - 10) * WIDTH + WIDTH - 40) * 4;
    let (r, g, b) = (picture.data[at], picture.data[at + 1], picture.data[at + 2]);
    assert!(r > 150 && g > 150 && b > 150, "{r} {g} {b}");
    assert!(r.abs_diff(g) < 30 && g.abs_diff(b) < 30, "{r} {g} {b}");

    // Input reaches the window.
    stream.input(InputEvent::PointerMove {
        window,
        x: 0.5,
        y: 0.25,
    });
    stream.input(InputEvent::PointerButton {
        window,
        button: PointerButton::Left,
        pressed: true,
    });
    stream.input(InputEvent::Key {
        keycode: 30,
        pressed: true,
    });
    stream.input(InputEvent::Key {
        keycode: 103,
        pressed: true,
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while events.lock().unwrap().len() < 4 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let got = events.lock().unwrap().clone();
    println!("input: {got:?}");
    assert!(matches!(
        got.iter().find(|e| matches!(e, InputEvent::PointerMove { .. })),
        Some(InputEvent::PointerMove { x, y, .. }) if (x - 0.5).abs() < 0.01 && (y - 0.25).abs() < 0.01
    ));
    assert!(got.contains(&InputEvent::PointerButton {
        window,
        button: PointerButton::Left,
        pressed: true
    }));
    assert!(got.contains(&InputEvent::Key {
        keycode: 30,
        pressed: true
    }));
    assert!(got.contains(&InputEvent::Key {
        keycode: 103,
        pressed: true
    }));
    drop(stream);
}

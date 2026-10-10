//! The RDP backend through both ends of the library: a client whose rules
//! send the test pattern window to RDP asks for it, the host starts an RDP
//! server for that one stream with a one-time login and hands it over the
//! session, the client logs in (the certificate pinned) and gets the
//! window's pictures, its pointer and keys reach the host's window over
//! RDP, and stopping the stream stops the server.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::{frame_number, TestPatternSource, HEIGHT, WIDTH, WINDOW};
use windowcast_client::{Client, ClientSession, Event, PicturePoll};
use windowcast_host::{FrameSource, HostConfig, PictureSource, WindowSource};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::selection::{BackendRule, WindowMatch};
use windowcast_protocol::{BackendKind, InputEvent, VideoCodec, WindowId, WindowInfo};
use windowcast_rdp::host::WithRdp;

const WAIT: Duration = Duration::from_secs(20);

/// The test pattern, keeping the input it gets.
#[derive(Default)]
struct Source(Mutex<Vec<InputEvent>>);

impl WindowSource for Source {
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
        self.0.lock().unwrap().push(event.clone());
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("windowcast-rdp-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn next_event(session: &ClientSession, matches: impl Fn(&Event) -> bool) -> Event {
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(Instant::now() < deadline, "event did not arrive");
        if let Some(event) = session.next_event(Duration::from_millis(200)) {
            if matches(&event) {
                return event;
            }
        }
    }
}

#[test]
fn a_window_streams_over_rdp_through_a_session() {
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

    let source = Arc::new(Source::default());
    let with_rdp = WithRdp::new(
        Arc::clone(&source) as Arc<dyn WindowSource>,
        "127.0.0.1".parse().unwrap(),
    )
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
        Arc::new(with_rdp),
    ));

    let client = Client::new(&client_dir).unwrap();
    let session = client.connect(&address, None).unwrap();
    session.accept_pictures(true);
    session.set_rules(vec![BackendRule {
        when: WindowMatch {
            app_id: Some("windowcast.testpattern".into()),
            title_contains: None,
            content: None,
        },
        backend: BackendKind::Rdp,
    }]);
    session.request_windows().unwrap();
    next_event(&session, |e| matches!(e, Event::Windows { .. }));

    session.start_window(WINDOW, &[VideoCodec::H264]).unwrap();
    let started = next_event(&session, |e| {
        matches!(e, Event::StreamStarted { .. } | Event::StreamRefused { .. })
    });
    assert_eq!(
        started,
        Event::StreamStarted {
            window: WINDOW.0,
            backend: BackendKind::Rdp,
            codec: None
        }
    );

    // Pictures, moving.
    let mut numbers = Vec::new();
    let deadline = Instant::now() + WAIT;
    while numbers.len() < 5 && Instant::now() < deadline {
        if let PicturePoll::Picture(picture) =
            session.next_picture(WINDOW, Duration::from_millis(500))
        {
            assert_eq!(
                (picture.width, picture.height),
                (WIDTH as u32, HEIGHT as u32)
            );
            numbers.push(frame_number(&picture.data, WIDTH));
        }
    }
    println!("frame numbers: {numbers:?}");
    assert!(numbers.len() >= 5);
    assert!(numbers.last() > numbers.first());

    // Input over RDP reaches the host's window.
    session
        .send_input(InputEvent::PointerMove {
            window: WINDOW,
            x: 0.75,
            y: 0.5,
        })
        .unwrap();
    session
        .send_input(InputEvent::Key {
            keycode: 30,
            pressed: true,
        })
        .unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        let got = source.0.lock().unwrap().clone();
        let moved = got.iter().any(|e| {
            matches!(e, InputEvent::PointerMove { x, y, .. } if (x - 0.75).abs() < 0.01 && (y - 0.5).abs() < 0.01)
        });
        let typed = got.contains(&InputEvent::Key {
            keycode: 30,
            pressed: true,
        });
        if moved && typed {
            break;
        }
        assert!(Instant::now() < deadline, "input did not arrive: {got:?}");
        std::thread::sleep(Duration::from_millis(20));
    }

    session.stop_window(WINDOW).unwrap();
    next_event(&session, |e| matches!(e, Event::StreamStopped { .. }));
    assert!(matches!(
        session.next_picture(WINDOW, Duration::from_millis(100)),
        PicturePoll::Ended
    ));
}

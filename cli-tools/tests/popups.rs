//! Window by window (docs/BACKENDS.md): a popup a streamed window opens
//! reaches the client as its own window-list entry, naming its owner,
//! without the client asking for the list again; the client shows it on
//! its own as it opens (following popups, on by default), stops it with its
//! owner, and with following off only lists it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::{TestPatternSource, WINDOW};
use windowcast_client::{Client, ClientSession, Event};
use windowcast_host::{FrameSource, HostConfig, HostControl, WindowSource};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{VideoCodec, WindowId, WindowInfo, WindowKind};

const POPUP: WindowId = WindowId(2);

/// The test pattern, and a popup it owns while `open` is set.
struct WithPopup {
    open: Arc<AtomicBool>,
}

impl WindowSource for WithPopup {
    fn list_windows(&self) -> Vec<WindowInfo> {
        let mut windows = TestPatternSource.list_windows();
        windows[0].position = Some((100, 100));
        if self.open.load(Ordering::SeqCst) {
            let owner = windows[0].clone();
            windows.push(WindowInfo {
                id: POPUP,
                title: format!("{} popup", owner.title),
                width: 120,
                height: 80,
                focused: false,
                owner: Some(owner.id),
                kind: WindowKind::Popup,
                position: Some((140, 180)),
                ..owner
            });
        }
        windows
    }
    fn encoders(&self) -> Vec<VideoCodec> {
        TestPatternSource.encoders()
    }
    fn open(&self, _window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        // The popup draws the test pattern too.
        TestPatternSource.open(WINDOW, codec)
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("windowcast-popups-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Rig {
    _runtime: tokio::runtime::Runtime,
    open: Arc<AtomicBool>,
    session: ClientSession,
}

fn rig(name: &str) -> Rig {
    let host_dir = temp_dir(&format!("{name}-host"));
    let client_dir = temp_dir(&format!("{name}-client"));
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
    let control = HostControl::open(&HostConfig {
        listen: address.clone(),
        pairing: false,
        data_dir: host_dir,
    })
    .unwrap();
    let open = Arc::new(AtomicBool::new(false));
    runtime.spawn(windowcast_host::serve_with(
        listener,
        control,
        Arc::new(WithPopup {
            open: Arc::clone(&open),
        }),
    ));
    let client = Client::new(&client_dir).unwrap();
    let session = client.connect(&address, None).unwrap();
    Rig {
        _runtime: runtime,
        open,
        session,
    }
}

/// Events until `wanted` matches one, within 20 s.
fn wait_for(session: &ClientSession, what: &str, mut wanted: impl FnMut(&Event) -> bool) {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        if let Some(event) = session.next_event(Duration::from_millis(200)) {
            if wanted(&event) {
                return;
            }
        }
    }
}

fn lists_popup(event: &Event) -> bool {
    matches!(event, Event::Windows { windows } if windows.iter().any(|w| {
        w.id == POPUP
            && w.owner == Some(WINDOW)
            && w.kind == WindowKind::Popup
            && w.position == Some((140, 180))
    }))
}

#[test]
fn a_popup_of_a_shown_window_opens_on_its_own_and_closes_with_it() {
    let rig = rig("follow");
    let session = &rig.session;
    session.request_windows().unwrap();
    wait_for(session, "the window list", |e| {
        matches!(e, Event::Windows { .. })
    });
    session.start_window(WINDOW, &[VideoCodec::H264]).unwrap();
    wait_for(
        session,
        "the window's stream",
        |e| matches!(e, Event::StreamStarted { window, .. } if *window == WINDOW.0),
    );

    // The program opens a popup: the host sends the list unasked, and the
    // client starts the popup by itself.
    rig.open.store(true, Ordering::SeqCst);
    wait_for(session, "the popup in the list", lists_popup);
    wait_for(
        session,
        "the popup's stream",
        |e| matches!(e, Event::StreamStarted { window, .. } if *window == POPUP.0),
    );

    // Stopping the window stops its popup.
    session.stop_window(WINDOW).unwrap();
    wait_for(
        session,
        "the popup to stop",
        |e| matches!(e, Event::StreamStopped { window } if *window == POPUP.0),
    );
}

#[test]
fn with_following_off_a_popup_is_only_listed() {
    let rig = rig("list-only");
    let session = &rig.session;
    session.set_follow_popups(false);
    session.request_windows().unwrap();
    wait_for(session, "the window list", |e| {
        matches!(e, Event::Windows { .. })
    });
    session.start_window(WINDOW, &[VideoCodec::H264]).unwrap();
    wait_for(
        session,
        "the window's stream",
        |e| matches!(e, Event::StreamStarted { window, .. } if *window == WINDOW.0),
    );
    rig.open.store(true, Ordering::SeqCst);
    wait_for(session, "the popup in the list", lists_popup);
    // Nothing starts it within a few list changes' time.
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        if let Some(Event::StreamStarted { window, .. }) =
            session.next_event(Duration::from_millis(200))
        {
            assert_ne!(window, POPUP.0, "the popup was started");
        }
    }
}

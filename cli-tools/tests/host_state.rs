//! A locked host (docs/BACKENDS.md, "A locked host"): the host tells its
//! clients when it is locked or unlocked, says nothing while it is
//! unlocked, and keeps streaming its windows while it is locked.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::{TestPatternSource, WINDOW};
use windowcast_client::{Client, ClientSession, Event, FramePoll};
use windowcast_host::{FrameSource, HostConfig, HostControl, WindowSource};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{HostState, VideoCodec, WindowId, WindowInfo};

/// The test pattern on a host that is locked while `locked` is set.
struct Lockable {
    locked: Arc<AtomicBool>,
}

impl WindowSource for Lockable {
    fn list_windows(&self) -> Vec<WindowInfo> {
        TestPatternSource.list_windows()
    }
    fn encoders(&self) -> Vec<VideoCodec> {
        TestPatternSource.encoders()
    }
    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        TestPatternSource.open(window, codec)
    }
    fn host_state(&self) -> HostState {
        HostState {
            locked: self.locked.load(Ordering::SeqCst),
        }
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "windowcast-host-state-{name}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Rig {
    _runtime: tokio::runtime::Runtime,
    locked: Arc<AtomicBool>,
    session: ClientSession,
}

fn rig(name: &str, locked: bool) -> Rig {
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
    let locked = Arc::new(AtomicBool::new(locked));
    runtime.spawn(windowcast_host::serve_with(
        listener,
        control,
        Arc::new(Lockable {
            locked: Arc::clone(&locked),
        }),
    ));
    let client = Client::new(&client_dir).unwrap();
    let session = client.connect(&address, None).unwrap();
    Rig {
        _runtime: runtime,
        locked,
        session,
    }
}

/// Events until `wanted` matches one, within 20 s; the ones before it.
fn wait_for(
    session: &ClientSession,
    what: &str,
    mut wanted: impl FnMut(&Event) -> bool,
) -> Vec<Event> {
    let until = Instant::now() + Duration::from_secs(20);
    let mut before = Vec::new();
    loop {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        if let Some(event) = session.next_event(Duration::from_millis(200)) {
            if wanted(&event) {
                return before;
            }
            before.push(event);
        }
    }
}

/// Every event within `time`.
fn events_for(session: &ClientSession, time: Duration) -> Vec<Event> {
    let until = Instant::now() + time;
    let mut events = Vec::new();
    while Instant::now() < until {
        if let Some(event) = session.next_event(Duration::from_millis(100)) {
            events.push(event);
        }
    }
    events
}

fn is_host_state(event: &Event) -> bool {
    matches!(event, Event::HostState { .. })
}

#[test]
fn the_client_hears_when_the_host_locks_and_its_windows_keep_streaming() {
    let rig = rig("change", false);
    let session = &rig.session;
    session.request_windows().unwrap();
    wait_for(session, "the window list", |e| {
        matches!(e, Event::Windows { .. })
    });
    // An unlocked host says nothing about its state.
    let quiet = events_for(session, Duration::from_secs(1));
    assert!(
        !quiet.iter().any(is_host_state),
        "an unlocked host sent its state: {quiet:?}"
    );

    rig.locked.store(true, Ordering::SeqCst);
    wait_for(session, "the host locking", |e| {
        matches!(e, Event::HostState { locked: true })
    });
    // Locked, a window still streams.
    session.start_window(WINDOW, &[VideoCodec::H264]).unwrap();
    let before = wait_for(
        session,
        "the window's stream",
        |e| matches!(e, Event::StreamStarted { window, .. } if *window == WINDOW.0),
    );
    assert!(
        !before
            .iter()
            .any(|e| matches!(e, Event::StreamRefused { .. })),
        "refused while locked: {before:?}"
    );
    assert!(
        matches!(
            session.next_frame(WINDOW, Duration::from_secs(10)),
            FramePoll::Frame(_)
        ),
        "no frame while the host is locked"
    );

    rig.locked.store(false, Ordering::SeqCst);
    wait_for(session, "the host unlocking", |e| {
        matches!(e, Event::HostState { locked: false })
    });
}

#[test]
fn a_host_locked_when_the_session_starts_says_so_at_once() {
    let rig = rig("start", true);
    wait_for(&rig.session, "the host's state", |e| {
        matches!(e, Event::HostState { locked: true })
    });
}

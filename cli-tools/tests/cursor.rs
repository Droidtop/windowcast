//! The session's pointer (docs/BACKENDS.md, "The session owns the
//! window"): while a window streams, the host sends its pointer over it in
//! fractions of the window's picture, each shape once before it is named,
//! and stops once nothing streams.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::{TestPatternSource, WithPointer, POINTER_SHAPE, WINDOW};
use windowcast_client::{Client, ClientSession, Event};
use windowcast_host::{HostConfig, HostControl};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::VideoCodec;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("windowcast-cursor-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Rig {
    _runtime: tokio::runtime::Runtime,
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
    runtime.spawn(windowcast_host::serve_with(
        listener,
        control,
        Arc::new(WithPointer::new(Arc::new(TestPatternSource))),
    ));
    let client = Client::new(&client_dir).unwrap();
    let session = client.connect(&address, None).unwrap();
    Rig {
        _runtime: runtime,
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

#[test]
fn the_host_pointer_follows_a_streamed_window_and_stops_with_it() {
    let rig = rig("follow");
    let session = &rig.session;
    session.request_windows().unwrap();
    wait_for(session, "the window list", |e| {
        matches!(e, Event::Windows { .. })
    });
    // Nothing streams: no pointer.
    let before = events_for(session, Duration::from_millis(500));
    assert!(
        !before.iter().any(|e| matches!(e, Event::Cursor { .. })),
        "a pointer before anything streams"
    );

    session.start_window(WINDOW, &[VideoCodec::H264]).unwrap();
    wait_for(session, "the pointer", |e| {
        matches!(e, Event::Cursor { window, visible: true, shape, .. }
            if *window == WINDOW.0 && *shape == POINTER_SHAPE)
    });
    // Its shape came first, whole.
    assert_eq!(
        session.cursor_image(POINTER_SHAPE),
        Some(WithPointer::arrow())
    );
    // It moves, inside the window's picture.
    let moves: Vec<(f32, f32)> = events_for(session, Duration::from_secs(1))
        .into_iter()
        .filter_map(|e| match e {
            Event::Cursor { x, y, .. } => Some((x, y)),
            _ => None,
        })
        .collect();
    assert!(moves.len() >= 5, "the pointer hardly moved: {moves:?}");
    assert!(moves
        .iter()
        .all(|(x, y)| (0.0..1.0).contains(x) && (0.0..1.0).contains(y)));
    let now = session.cursor(WINDOW).expect("the pointer over the window");
    assert!(now.visible);

    session.stop_window(WINDOW).unwrap();
    wait_for(
        session,
        "the stream stopping",
        |e| matches!(e, Event::StreamStopped { window } if *window == WINDOW.0),
    );
    // Settled: nothing streams, so the pointer is not sent.
    events_for(session, Duration::from_millis(300));
    let after = events_for(session, Duration::from_secs(1));
    assert!(
        !after.iter().any(|e| matches!(e, Event::Cursor { .. })),
        "the pointer is still sent after the stream stopped: {after:?}"
    );
    assert!(session.cursor(WINDOW).is_none());
}

//! Switching a window's carrier without a break (docs/BACKENDS.md, "One
//! window, any carrier"; Droidtop/tracker#457): the test pattern streams as
//! video, switches to RDP pictures and back to video, each new carrier
//! coming up beside the old one and replacing it once shown, the window's
//! stream never ending; a switch the host cannot make is refused and the
//! window stays where it was.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::{TestPatternSource, HEIGHT, WIDTH, WINDOW};
use windowcast_client::{Client, ClientSession, Event, FramePoll, PicturePoll};
use windowcast_host::{FrameSource, HostConfig, PictureSource, WindowSource};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{BackendKind, VideoCodec, WindowId, WindowInfo};
use windowcast_rdp::host::WithRdp;

const WAIT: Duration = Duration::from_secs(20);

struct Source;

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
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("windowcast-switch-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Events until one matches; a stream that stops fails the test.
fn next_event(session: &ClientSession, what: &str, matches: impl Fn(&Event) -> bool) -> Event {
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(Instant::now() < deadline, "{what} did not arrive");
        if let Some(event) = session.next_event(Duration::from_millis(200)) {
            assert!(
                !matches!(event, Event::StreamStopped { .. }),
                "the window's stream stopped while waiting for {what}"
            );
            if matches(&event) {
                return event;
            }
        }
    }
}

fn a_frame(session: &ClientSession) {
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(Instant::now() < deadline, "no video frame");
        match session.next_frame(WINDOW, Duration::from_millis(200)) {
            FramePoll::Frame(_) => return,
            FramePoll::Timeout => {}
            FramePoll::Ended => panic!("the video ended"),
        }
    }
}

fn a_picture(session: &ClientSession) {
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(Instant::now() < deadline, "no RDP picture");
        match session.next_picture(WINDOW, Duration::from_millis(200)) {
            PicturePoll::Picture(picture) => {
                assert_eq!(
                    (picture.width, picture.height),
                    (WIDTH as u32, HEIGHT as u32)
                );
                return;
            }
            PicturePoll::Timeout => {}
            PicturePoll::Ended => panic!("the RDP pictures ended"),
        }
    }
}

#[test]
fn a_window_switches_carriers_and_back_without_its_stream_ending() {
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
    let with_rdp = WithRdp::new(Arc::new(Source), "127.0.0.1".parse().unwrap()).unwrap();
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
            data_dir: host_dir,
        },
        Arc::new(with_rdp),
    ));
    let client = Client::new(&client_dir).unwrap();
    let session = client.connect(&address, None).unwrap();
    session.accept_pictures(true);
    session.request_windows().unwrap();
    next_event(&session, "the window list", |e| {
        matches!(e, Event::Windows { .. })
    });

    // Video first (the test pattern's rules give it Native).
    session.start_window(WINDOW, &[VideoCodec::H264]).unwrap();
    next_event(&session, "the stream", |e| {
        matches!(
            e,
            Event::StreamStarted {
                backend: BackendKind::Native,
                ..
            }
        )
    });
    a_frame(&session);

    // To RDP: the new carrier comes up beside the video.
    let to_rdp = session
        .switch_window(WINDOW, BackendKind::Rdp, &[VideoCodec::H264])
        .unwrap();
    next_event(
        &session,
        "the RDP carrier",
        |e| matches!(e, Event::CarrierStarted { generation, backend: BackendKind::Rdp, .. } if *generation == to_rdp),
    );
    a_picture(&session);
    session.carrier_shown(WINDOW, to_rdp).unwrap();
    // The video stops; the RDP pictures go on.
    let deadline = Instant::now() + WAIT;
    while !matches!(
        session.next_frame(WINDOW, Duration::from_millis(200)),
        FramePoll::Ended
    ) {
        assert!(Instant::now() < deadline, "the video did not stop");
    }
    a_picture(&session);

    // And back to video.
    let to_video = session
        .switch_window(WINDOW, BackendKind::Native, &[VideoCodec::H264])
        .unwrap();
    next_event(
        &session,
        "the video carrier",
        |e| matches!(e, Event::CarrierStarted { generation, backend: BackendKind::Native, .. } if *generation == to_video),
    );
    a_frame(&session);
    session.carrier_shown(WINDOW, to_video).unwrap();
    let deadline = Instant::now() + WAIT;
    while !matches!(
        session.next_picture(WINDOW, Duration::from_millis(200)),
        PicturePoll::Ended
    ) {
        assert!(Instant::now() < deadline, "the RDP pictures did not stop");
    }
    a_frame(&session);

    // A carrier this host does not offer falls back to the one the window
    // is on, which the host refuses as a second one: no switch.
    let to_vnc = session
        .switch_window(WINDOW, BackendKind::Vnc, &[VideoCodec::H264])
        .unwrap();
    next_event(
        &session,
        "the refusal",
        |e| matches!(e, Event::CarrierRefused { generation, .. } if *generation == to_vnc),
    );
    a_frame(&session);
}

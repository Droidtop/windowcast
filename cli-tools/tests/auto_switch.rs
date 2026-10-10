//! The automatic carrier selector (docs/BACKENDS.md, "When a carrier
//! switches"; Droidtop/tracker#457 step 3) through both ends: a text
//! window starts on RDP pictures (the default rules), switches by itself to
//! video while it moves, and back to RDP once it is still again, each switch
//! made without the stream stopping.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::{TestPatternSource, WINDOW};
use windowcast_client::{Client, ClientSession, Event, FramePoll, PicturePoll};
use windowcast_host::{
    EncodedFrame, FrameSource, HostConfig, Picture, PictureSource, WindowSource,
};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{BackendKind, ContentHint, VideoCodec, WindowId, WindowInfo};
use windowcast_rdp::host::WithRdp;

/// The test pattern as a text window that is still (one change a second)
/// until `moving` is set.
struct Text {
    moving: Arc<AtomicBool>,
}

struct Frames {
    inner: Box<dyn FrameSource>,
    moving: Arc<AtomicBool>,
    n: u64,
}

impl FrameSource for Frames {
    fn next_frame(&mut self, keyframe: bool) -> Option<EncodedFrame> {
        loop {
            let frame = self.inner.next_frame(keyframe)?;
            self.n += 1;
            if keyframe || self.moving.load(Ordering::SeqCst) || self.n.is_multiple_of(30) {
                return Some(frame);
            }
        }
    }
    fn describe(&self) -> String {
        self.inner.describe()
    }
}

struct Pictures {
    inner: Box<dyn PictureSource>,
    moving: Arc<AtomicBool>,
    n: u64,
}

impl PictureSource for Pictures {
    fn next_picture(&mut self) -> Option<Picture> {
        loop {
            let picture = self.inner.next_picture()?;
            self.n += 1;
            if self.moving.load(Ordering::SeqCst) || self.n % 30 == 1 {
                return Some(picture);
            }
        }
    }
}

impl WindowSource for Text {
    fn list_windows(&self) -> Vec<WindowInfo> {
        TestPatternSource
            .list_windows()
            .into_iter()
            .map(|w| WindowInfo {
                content: ContentHint::Text,
                ..w
            })
            .collect()
    }
    fn encoders(&self) -> Vec<VideoCodec> {
        TestPatternSource.encoders()
    }
    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        Ok(Box::new(Frames {
            inner: TestPatternSource.open(window, codec)?,
            moving: Arc::clone(&self.moving),
            n: 0,
        }))
    }
    fn open_pictures(&self, window: WindowId) -> Option<Result<Box<dyn PictureSource>, String>> {
        Some(TestPatternSource.open_pictures(window)?.map(|inner| {
            Box::new(Pictures {
                inner,
                moving: Arc::clone(&self.moving),
                n: 0,
            }) as Box<dyn PictureSource>
        }))
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("windowcast-auto-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Acts as an app until `until`: takes the frames and pictures, shows a
/// switch's new carrier once it has a picture, and returns the carrier the
/// window is on when `done` says so.
fn run(
    session: &ClientSession,
    carrier: &mut BackendKind,
    within: Duration,
    done: impl Fn(BackendKind) -> bool,
) {
    let until = Instant::now() + within;
    let mut pending: Option<(u32, BackendKind)> = None;
    while Instant::now() < until {
        if let Some(event) = session.next_event(Duration::from_millis(50)) {
            match event {
                Event::StreamStopped { .. } => panic!("the stream stopped"),
                Event::CarrierStarted {
                    generation,
                    backend,
                    ..
                } => pending = Some((generation, backend)),
                Event::CarrierRefused { reason, .. } => panic!("a switch was refused: {reason}"),
                _ => {}
            }
        }
        let frame = matches!(
            session.next_frame(WINDOW, Duration::from_millis(5)),
            FramePoll::Frame(_)
        );
        let picture = matches!(
            session.next_picture(WINDOW, Duration::from_millis(5)),
            PicturePoll::Picture(_)
        );
        if let Some((generation, backend)) = pending {
            let arrived = match backend {
                BackendKind::Rdp => picture || session.latest_picture(WINDOW).is_some(),
                _ => frame,
            };
            if arrived {
                session.carrier_shown(WINDOW, generation).unwrap();
                println!("switched to {backend:?}");
                *carrier = backend;
                pending = None;
            }
        }
        if done(*carrier) {
            return;
        }
    }
    panic!("still on {carrier:?} after {within:?}");
}

#[test]
fn a_text_window_switches_to_video_while_it_moves_and_back_when_still() {
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
    let moving = Arc::new(AtomicBool::new(false));
    let source = Text {
        moving: Arc::clone(&moving),
    };
    let with_rdp = WithRdp::new(Arc::new(source), "127.0.0.1".parse().unwrap()).unwrap();
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
    let until = Instant::now() + Duration::from_secs(20);
    while !matches!(
        session.next_event(Duration::from_millis(200)),
        Some(Event::Windows { .. })
    ) {
        assert!(Instant::now() < until, "no window list");
    }

    // Text: the default rules put it on RDP pictures.
    session.start_window(WINDOW, &[VideoCodec::H264]).unwrap();
    let mut carrier = loop {
        assert!(Instant::now() < until, "no stream");
        if let Some(Event::StreamStarted { backend, .. }) =
            session.next_event(Duration::from_millis(200))
        {
            break backend;
        }
    };
    assert_eq!(carrier, BackendKind::Rdp);

    // It starts to move: video, after the minimum interval and the dwell.
    moving.store(true, Ordering::SeqCst);
    run(&session, &mut carrier, Duration::from_secs(45), |c| {
        c == BackendKind::Native
    });

    // Still again: back to RDP pictures.
    moving.store(false, Ordering::SeqCst);
    run(&session, &mut carrier, Duration::from_secs(45), |c| {
        c == BackendKind::Rdp
    });
}

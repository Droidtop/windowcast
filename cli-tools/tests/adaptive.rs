//! Adaptive quality over a real bottleneck: a busy 640x360 picture that
//! sends about 1.4 Mbit/s (OpenH264 asked for 3), streamed over loopback
//! through `tc netem` limited to 600 kbit/s with a short queue, then with
//! the limit lifted. The host must
//! bring the stream under the bottleneck within seconds (its reports to the
//! client say what it holds the encoder to) and climb back once the
//! network is clear.
//!
//! Needs root in a network namespace of its own (it shapes `lo`), so it
//! runs only with WINDOWCAST_TEST_NETEM set, as CI runs it:
//! `sudo unshare -n sh -c "ip link set lo up && WINDOWCAST_TEST_NETEM=1 <test binary>"`.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windowcast_client::{Client, ClientSession, FramePoll};
use windowcast_host::quality::Quality;
use windowcast_host::video::OpenH264;
use windowcast_host::{EncodedFrame, FrameSource, HostConfig, HostControl, WindowSource};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{ContentHint, StreamQuality, VideoCodec, WindowId, WindowInfo};

const WINDOW: WindowId = WindowId(7);
const SIZE: (usize, usize) = (640, 360);
const FPS: u32 = 30;
const NOMINAL: u32 = 3_000_000;

struct Busy;

impl WindowSource for Busy {
    fn list_windows(&self) -> Vec<WindowInfo> {
        vec![WindowInfo {
            id: WINDOW,
            title: "busy".into(),
            app_id: "busy".into(),
            width: SIZE.0 as u32,
            height: SIZE.1 as u32,
            focused: true,
            content: ContentHint::General,
        }]
    }
    fn encoders(&self) -> Vec<VideoCodec> {
        vec![VideoCodec::H264]
    }
    fn open(&self, _: WindowId, _: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        Ok(Box::new(BusyStream {
            encoder: OpenH264::new(NOMINAL, FPS)?,
            frame: 0,
            seed: 1,
            next_at: Instant::now(),
            fps: FPS,
            rate: (NOMINAL, FPS),
            announced: false,
        }))
    }
}

/// A moving gradient with grain: OpenH264 needs about its full rate for it.
struct BusyStream {
    encoder: OpenH264,
    frame: usize,
    seed: u32,
    next_at: Instant,
    fps: u32,
    rate: (u32, u32),
    announced: bool,
}

impl FrameSource for BusyStream {
    fn frame_rate(&self) -> u32 {
        FPS
    }

    fn set_quality(&mut self, quality: Quality) {
        self.fps = quality.fps.unwrap_or(FPS).min(FPS);
        let rate = (quality.bitrate.unwrap_or(NOMINAL).min(NOMINAL), self.fps);
        if rate != self.rate {
            self.encoder.set_rate(rate.0, rate.1).unwrap();
            self.rate = rate;
        }
    }

    fn next_frame(&mut self, keyframe: bool) -> Option<EncodedFrame> {
        let (w, h) = SIZE;
        // The encoder's rate control may skip a picture: try the next.
        loop {
            let frame_time = Duration::from_secs(1) / self.fps;
            std::thread::sleep(self.next_at.saturating_duration_since(Instant::now()));
            self.next_at = Instant::now() + frame_time;
            self.frame += 1;
            let (t, seed) = (self.frame, &mut self.seed);
            let encoded = self
                .encoder
                .encode(w, h, keyframe, |i420| {
                    i420.resize(w * h * 3 / 2, 128);
                    for (i, byte) in i420[..w * h].iter_mut().enumerate() {
                        *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                        *byte = ((i % w + i / w + t * 5) as u8) ^ ((*seed >> 27) as u8);
                    }
                    Ok(())
                })
                .map_err(|e| eprintln!("encoding failed: {e}"))
                .ok()?;
            if let Some(data) = encoded {
                return Some(EncodedFrame {
                    data,
                    duration: frame_time,
                    size: (!self.announced).then_some((w as u32, h as u32)),
                })
                .inspect(|_| self.announced = true);
            }
        }
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("windowcast-adaptive-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn tc(args: &str) {
    let status = Command::new("tc")
        .args(args.split(' '))
        .status()
        .expect("tc");
    assert!(status.success(), "tc {args}");
}

/// Reads the client's frames, and watches the host's view of the stream
/// (what it holds the encoder to and what it sends; the client's reports of
/// the same ride the control channel, which a choked link delays), until
/// `done` says it is the one wanted or `within` runs out.
fn watch(
    session: &ClientSession,
    control: &HostControl,
    within: Duration,
    done: impl Fn(&StreamQuality) -> bool,
) -> Option<StreamQuality> {
    let start = Instant::now();
    let mut last_print = Instant::now() - Duration::from_secs(1);
    while start.elapsed() < within {
        while let FramePoll::Frame(_) = session.next_frame(WINDOW, Duration::ZERO) {}
        let _ = session.next_event(Duration::from_millis(50));
        let Some(quality) = control.streams().first().map(|s| s.quality) else {
            continue;
        };
        if last_print.elapsed() >= Duration::from_secs(1) {
            last_print = Instant::now();
            println!(
                "{:5.1} s: held to {:?} kbit/s, sent {} kbit/s, {} fps, {}x{}, loss {:.1}%, round trip {:?} ms",
                start.elapsed().as_secs_f32(),
                quality.target_kbps,
                quality.sent_kbps,
                quality.fps,
                quality.width,
                quality.height,
                quality.loss_percent,
                quality.rtt_ms
            );
        }
        if done(&quality) {
            return Some(quality);
        }
    }
    None
}

#[test]
fn the_stream_fits_a_bottleneck_and_recovers() {
    if std::env::var_os("WINDOWCAST_TEST_NETEM").is_none() {
        println!(
            "skipped: shapes lo with tc netem; set WINDOWCAST_TEST_NETEM in a network namespace"
        );
        return;
    }
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
    let control = HostControl::open(&HostConfig {
        listen: address.clone(),
        pairing: false,
        data_dir: host_dir.clone(),
    })
    .unwrap();
    runtime.spawn(windowcast_host::serve_with(
        listener,
        Arc::clone(&control),
        Arc::new(Busy),
    ));
    let client = Client::new(&client_dir).unwrap();
    let session = client.connect(&address, None).unwrap();
    session.request_windows().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    session.start_window(WINDOW, &[VideoCodec::H264]).unwrap();

    // Clear network: the stream runs at its own rate, nothing held.
    let free = watch(&session, &control, Duration::from_secs(8), |q| {
        q.sent_kbps > 0
    })
    .expect("the stream never started");
    std::thread::sleep(Duration::from_secs(3));
    let free = control.streams().first().map_or(free, |s| s.quality);
    assert!(
        free.sent_kbps > 1200,
        "the busy picture sends only {} kbit/s",
        free.sent_kbps
    );

    // A 600 kbit/s bottleneck with a short queue: the host must get under it.
    tc("qdisc add dev lo root netem rate 600kbit delay 10ms limit 40");
    let fitted = watch(&session, &control, Duration::from_secs(25), |q| {
        q.target_kbps.is_some_and(|t| t <= 600) && q.sent_kbps <= 650
    });
    let fitted = fitted.expect("the host never brought the stream under 600 kbit/s");

    // Lifted: back up past the bottleneck's rate. Growth is 8% a second
    // from wherever the cut left it, after the last of the queued pings
    // (seconds late under the bottleneck) is answered; from a deep cut
    // (150 kbit/s in run 38011225143) that is most of a minute.
    tc("qdisc del dev lo root");
    let recovered = watch(&session, &control, Duration::from_secs(90), |q| {
        q.target_kbps.is_none_or(|t| t >= 1200) && q.sent_kbps >= 1000
    });
    let recovered = recovered.expect("the stream did not climb back after the bottleneck went");
    println!(
        "clear {} kbit/s; under 600 kbit/s: held to {:?}, sent {}; after: held to {:?}, sent {}",
        free.sent_kbps,
        fitted.target_kbps,
        fitted.sent_kbps,
        recovered.target_kbps,
        recovered.sent_kbps
    );

    drop(session);
    drop(client);
    runtime.shutdown_timeout(Duration::from_secs(1));
    let _ = std::fs::remove_dir_all(host_dir);
    let _ = std::fs::remove_dir_all(client_dir);
}

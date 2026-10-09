//! Reference client: connects to a host agent through `client-core`, the
//! same surface every client uses, pairs by PIN the first time (or resumes
//! afterwards), lists the host's windows and can stream one, checking that
//! the frames decode.
//!
//! Usage: `windowcast-client HOST:PORT [--pin PIN] [--watch WINDOW] [--frames N]
//! [--codec h264|h265|av1]` (frames are checked by decoding them for H.264
//! only; other codecs are counted).

use std::time::Duration;

use windowcast_cli_tools::H264Check;
use windowcast_client::{AudioPoll, Client, Event, FramePoll};
use windowcast_protocol::{VideoCodec, WindowId};

const WAIT: Duration = Duration::from_secs(10);

fn main() {
    let mut host = None;
    let mut pin = None;
    let mut watch = None;
    let mut frames = 90usize;
    let mut codec = VideoCodec::H264;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--pin" => pin = Some(args.next().expect("--pin needs the PIN the host shows")),
            "--watch" => {
                watch = Some(WindowId(
                    args.next()
                        .and_then(|w| w.parse().ok())
                        .expect("--watch needs a window id"),
                ))
            }
            "--frames" => {
                frames = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .expect("--frames needs a number")
            }
            "--codec" => {
                codec = match args.next().as_deref() {
                    Some("h264") => VideoCodec::H264,
                    Some("h265") => VideoCodec::H265,
                    Some("av1") => VideoCodec::Av1,
                    _ => panic!("--codec is h264, h265 or av1"),
                }
            }
            other if host.is_none() => host = Some(other.to_owned()),
            other => panic!("unknown argument {other}"),
        }
    }
    let host = host.expect("usage: windowcast-client HOST:PORT [--pin PIN] [--watch WINDOW]");

    let client = Client::new(&windowcast_cli_tools::data_dir().join("client"))
        .expect("failed to open this client's identity");
    println!("client identity: {}", client.peer_id());
    let session = client
        .connect(&host, pin.as_deref())
        .unwrap_or_else(|e| fail(&format!("could not connect to {host}: {e}")));
    if session.paired() {
        println!("paired with host {}", session.host_id());
    } else {
        println!("connected to host {}", session.host_id());
    }

    session
        .request_windows()
        .expect("failed to ask for the window list");
    loop {
        match session.next_event(WAIT) {
            Some(Event::Windows { windows }) => {
                println!("{} open windows:", windows.len());
                for window in windows {
                    println!(
                        "  {:>4}  {:<30}  app_id={}  {:?}",
                        window.id.0, window.title, window.app_id, window.content
                    );
                }
                break;
            }
            Some(Event::Closed) | None => fail("no window list"),
            Some(_) => {}
        }
    }

    if let Some(window) = watch {
        watch_window(&session, window, frames, codec);
    }
    session.close();
}

fn watch_window(
    session: &windowcast_client::ClientSession,
    window: WindowId,
    frames: usize,
    codec: VideoCodec,
) {
    session
        .start_window(window, &[codec])
        .expect("failed to ask for the stream");
    loop {
        match session.next_event(WAIT) {
            Some(Event::StreamStarted { backend, codec, .. }) => {
                println!(
                    "streaming window {} over {backend:?} in {codec:?}",
                    window.0
                );
                break;
            }
            Some(Event::StreamRefused { reason, .. }) => fail(&format!("refused: {reason}")),
            Some(Event::Closed) | None => fail("no answer to the stream request"),
            Some(_) => {}
        }
    }

    let mut check = H264Check::new().expect("software H.264 decoder");
    let mut sound = Sound::new();
    let (mut received, mut keyframes, mut bytes) = (0usize, 0usize, 0usize);
    let started = std::time::Instant::now();
    while received < frames {
        while let AudioPoll::Packet(packet) = session.next_audio(window, Duration::ZERO) {
            sound.push(&packet.data);
        }
        match session.next_frame(window, WAIT) {
            FramePoll::Frame(frame) => {
                received += 1;
                keyframes += usize::from(frame.keyframe);
                bytes += frame.data.len();
                if codec != VideoCodec::H264 {
                    continue;
                }
                if let Err(e) = check.decode(&frame.data) {
                    fail(&format!("frame {received} does not decode: {e}"));
                }
            }
            FramePoll::Timeout => fail("frames stopped arriving"),
            FramePoll::Ended => fail("the stream ended early"),
        }
    }
    let seconds = started.elapsed().as_secs_f64();
    println!(
        "received {received} frames ({keyframes} keyframes, {} KiB) in {seconds:.1} s, {:.1} fps; decoded {} pictures at {:?}, centre pixel {:?}",
        bytes / 1024,
        received as f64 / seconds,
        check.pictures,
        check.dimensions.unwrap_or_default(),
        check.center
    );
    if let Some(line) = sound.report() {
        println!("{line}");
    }
    if codec == VideoCodec::H264 && check.pictures == 0 {
        fail("nothing decoded");
    }
    session
        .stop_window(window)
        .expect("failed to stop the stream");
}

/// A window's sound as it arrives: Opus decoded, for its level and pitch.
struct Sound {
    decoder: opus::Decoder,
    packets: usize,
    left: Vec<f32>,
}

impl Sound {
    fn new() -> Self {
        Sound {
            decoder: opus::Decoder::new(48_000, opus::Channels::Stereo).expect("Opus decoder"),
            packets: 0,
            left: Vec::new(),
        }
    }

    fn push(&mut self, packet: &[u8]) {
        let mut pcm = vec![0f32; 5760 * 2];
        if let Ok(frames) = self.decoder.decode_float(packet, &mut pcm, false) {
            self.left.extend(pcm[..frames * 2].iter().step_by(2));
            self.packets += 1;
        }
    }

    /// "sound: N Opus packets, level L, pitch P Hz", if any came.
    fn report(&self) -> Option<String> {
        if self.packets == 0 {
            return None;
        }
        let n = self.left.len().max(1) as f32;
        let rms = (self.left.iter().map(|s| s * s).sum::<f32>() / n).sqrt();
        let crossings = self
            .left
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        Some(format!(
            "sound: {} Opus packets, level {rms:.3}, pitch {:.0} Hz",
            self.packets,
            crossings as f32 * 48_000.0 / n
        ))
    }
}

fn fail(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1);
}

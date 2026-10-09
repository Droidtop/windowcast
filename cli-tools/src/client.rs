//! Reference client: connects to a host agent through `client-core`, the
//! same surface every client uses, pairs by PIN the first time (or resumes
//! afterwards), lists the host's windows and can stream one, checking that
//! the frames decode.
//!
//! Usage: `windowcast-client HOST:PORT [--pin PIN] [--watch WINDOW] [--frames N]`

use std::time::Duration;

use windowcast_cli_tools::H264Check;
use windowcast_client::{Client, Event, FramePoll};
use windowcast_protocol::{VideoCodec, WindowId};

const WAIT: Duration = Duration::from_secs(10);

fn main() {
    let mut host = None;
    let mut pin = None;
    let mut watch = None;
    let mut frames = 90usize;
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
        watch_window(&session, window, frames);
    }
    session.close();
}

fn watch_window(session: &windowcast_client::ClientSession, window: WindowId, frames: usize) {
    session
        .start_window(window, &[VideoCodec::H264])
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
    let (mut received, mut keyframes, mut bytes) = (0usize, 0usize, 0usize);
    let started = std::time::Instant::now();
    while received < frames {
        match session.next_frame(window, WAIT) {
            FramePoll::Frame(frame) => {
                received += 1;
                keyframes += usize::from(frame.keyframe);
                bytes += frame.data.len();
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
        "received {received} frames ({keyframes} keyframes, {} KiB) in {seconds:.1} s, {:.1} fps; decoded {} pictures at {:?}",
        bytes / 1024,
        received as f64 / seconds,
        check.pictures,
        check.dimensions.unwrap_or_default()
    );
    if check.pictures == 0 {
        fail("nothing decoded");
    }
    session
        .stop_window(window)
        .expect("failed to stop the stream");
}

fn fail(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1);
}

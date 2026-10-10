//! windowcast's reference application: the minimal, complete demonstration
//! of the library. One program is a host, a client or both, by its
//! configuration, with a native window per role and a native window per
//! streamed window. Product features belong in the library; this app only
//! exercises them.
//!
//! - Host window: pairing PIN, the host's windows with their detected
//!   content and the backend the rules pick, encoder, codec, frame rate and
//!   bitrate (live), input and clipboard switches, live streams, clients.
//! - Client window: pairing by address and PIN, saved hosts, the host's
//!   windows with a backend rule per app, stream window placement, input
//!   switch, each stream's statistics, clipboard, a session log.
//! - Stream windows show only the streamed window: decoded on the GPU and
//!   presented natively (client-windows). F11 toggles fullscreen.
//!
//! Usage: `windowcast-app [--role host|client|both] [--data-dir DIR]
//! [--listen ADDR:PORT] [--no-window] [--connect ADDR [--pin PIN]
//! [--stream APP_OR_TITLE] [--fullscreen] [--display N] [--codec h264|h265|av1]]`
//! or `windowcast-app --open-pairing [--data-dir DIR]`
//!
//! `--no-window` runs a host without its window (the PIN is printed).
//! `--open-pairing` asks the host already running on the data directory
//! for a new PIN (pairing closes once a client pairs) and ends; the PIN
//! shows in that host's window, or its output without one.
//! `--stream` streams one window from the command line: only its stream
//! window opens, and the program ends when it is closed.

mod client;
mod config;
mod gui;
mod host;
mod platform;
mod terminal;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use config::{Roles, Store};

fn usage() -> ! {
    eprintln!(
        "usage: windowcast-app [--role host|client|both] [--data-dir DIR] [--listen ADDR:PORT] [--no-window]\n\
         \x20                     [--connect ADDR [--pin PIN] [--stream APP_OR_TITLE] [--fullscreen] [--display N] [--codec h264|h265|av1]]\n\
         \x20      windowcast-app --open-pairing [--data-dir DIR]"
    );
    std::process::exit(2);
}

#[derive(Default)]
struct Args {
    roles: Option<Roles>,
    data_dir: Option<PathBuf>,
    listen: Option<String>,
    no_window: bool,
    open_pairing: bool,
    connect: Option<String>,
    pin: Option<String>,
    stream: Option<String>,
    overrides: client::Overrides,
}

fn parse() -> Args {
    let mut parsed = Args::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| usage());
        match arg.as_str() {
            "--role" => parsed.roles = Some(Roles::parse(&value()).unwrap_or_else(|| usage())),
            "--data-dir" => parsed.data_dir = Some(PathBuf::from(value())),
            "--listen" => parsed.listen = Some(value()),
            "--no-window" => parsed.no_window = true,
            "--open-pairing" => parsed.open_pairing = true,
            "--connect" => parsed.connect = Some(value()),
            "--pin" => parsed.pin = Some(value()),
            "--stream" => parsed.stream = Some(value()),
            "--fullscreen" => parsed.overrides.fullscreen = Some(true),
            "--display" => {
                parsed.overrides.display = Some(value().parse().unwrap_or_else(|_| usage()))
            }
            "--codec" => {
                parsed.overrides.codec = Some(
                    match value().to_ascii_lowercase().as_str() {
                        "h264" => "H264",
                        "h265" | "hevc" => "H265",
                        "av1" => "Av1",
                        _ => usage(),
                    }
                    .to_owned(),
                )
            }
            _ => usage(),
        }
    }
    parsed
}

fn main() {
    platform::init();
    let args = parse();
    let data_dir = args
        .data_dir
        .clone()
        .unwrap_or_else(windowcast_identity::app_dir);
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        eprintln!("cannot use {}: {e}", data_dir.display());
        std::process::exit(1);
    }
    if args.open_pairing {
        match host::request_open_pairing(&data_dir) {
            Ok(()) => {
                println!(
                    "asked the host running on {} to open pairing; its new PIN shows in its window or output",
                    data_dir.display()
                );
                return;
            }
            Err(e) => {
                eprintln!("cannot ask the host: {e}");
                std::process::exit(1);
            }
        }
    }
    let store = Arc::new(Store::load(&data_dir));
    let config = store.get();
    let roles = args.roles.unwrap_or(config.roles);
    let listen = args.listen.clone().unwrap_or(config.host.listen);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let host = roles.host().then(|| {
        let host = host::HostRole::start(
            runtime.handle().clone(),
            Arc::clone(&store),
            &data_dir,
            listen,
        );
        if let Err(e) = &host {
            eprintln!("host role: {e}");
        }
        host
    });
    let client = roles.client().then(|| {
        let client = client::ClientRole::start(Arc::clone(&store), &data_dir);
        if let Err(e) = &client {
            eprintln!("client role: {e}");
        }
        client
    });

    if let (Some(Ok(client)), Some(address)) = (&client, &args.connect) {
        client.set_overrides(args.overrides.clone());
        if let Err(e) = client.connect(address, args.pin.as_deref()) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        if let Some(wanted) = &args.stream {
            stream_from_command_line(client, wanted);
            return;
        }
    }

    if args.no_window && client.is_none() {
        if !matches!(host, Some(Ok(_))) {
            std::process::exit(1);
        }
        // A host without a window: serve until the process is ended.
        loop {
            std::thread::park();
        }
    }
    if let Err(e) = gui::run(roles, host, client) {
        eprintln!("window: {e}");
        std::process::exit(1);
    }
}

/// Streams the host window whose app id is `wanted`, or whose title
/// contains it, and returns when its stream window is closed.
fn stream_from_command_line(client: &Arc<client::ClientRole>, wanted: &str) {
    let windows = client.wait_for_windows(Duration::from_secs(10));
    let lower = wanted.to_lowercase();
    let Some(window) = windows
        .iter()
        .find(|w| w.app_id.eq_ignore_ascii_case(wanted))
        .or_else(|| {
            windows
                .iter()
                .find(|w| w.title.to_lowercase().contains(&lower))
        })
    else {
        eprintln!("the host has no window called {wanted}");
        std::process::exit(1);
    };
    println!("streaming {} ({})", window.title, window.app_id);
    if let Err(e) = client.start_stream(window.id.0) {
        eprintln!("{e}");
        std::process::exit(1);
    }
    let mut reported = std::time::Instant::now();
    while client.streaming() {
        std::thread::sleep(Duration::from_millis(200));
        if reported.elapsed() >= Duration::from_secs(5) {
            reported = std::time::Instant::now();
            for stream in client.snapshot().streams {
                let s = stream.stats;
                println!(
                    "{:?} {:?} via {}: {:?} {:.1} fps, {:.2} Mbit/s, {} keyframes, decode and present {:.1} ms, resets {}{}",
                    stream.backend,
                    stream.codec,
                    s.decoder,
                    s.size,
                    s.fps,
                    s.mbps,
                    s.keyframes,
                    s.latency_ms,
                    s.resets,
                    s.error.map(|e| format!(", error: {e}")).unwrap_or_default()
                );
            }
        }
    }
    for line in client.snapshot().log {
        println!("{line}");
    }
    client.disconnect();
}

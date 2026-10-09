//! A GameStream client on the command line, for trying windowcast's own
//! GameStream against Sunshine, Apollo or a windowcast host.
//!
//! Usage:
//!   windowcast-gamestream pair HOST       (shows a PIN to type on the host)
//!   windowcast-gamestream apps HOST
//!   windowcast-gamestream stream HOST APP_ID [SECONDS]
//!
//! HOST is an IP address. The client's certificate and each paired host's
//! are kept under the windowcast data directory (`gamestream/`).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use windowcast_cli_tools::H264Check;
use windowcast_gamestream::client::{GameStreamClient, Host, HTTP_PORT};
use windowcast_gamestream::crypto::Credentials;
use windowcast_gamestream::pairing::new_pin;
use windowcast_gamestream::session::StreamRequest;

fn dir() -> PathBuf {
    windowcast_cli_tools::data_dir().join("gamestream")
}

fn host_file(address: &str) -> PathBuf {
    dir().join(format!(
        "host-{}.der",
        address.replace([':', '[', ']'], "_")
    ))
}

fn host(client: &GameStreamClient, address: &str) -> Host {
    let base = Host {
        address: address.to_owned(),
        http_port: HTTP_PORT,
        https_port: 47984,
        cert: std::fs::read(host_file(address)).ok(),
    };
    match client.server_info(&base) {
        Ok(info) => Host {
            https_port: info.https_port,
            ..base
        },
        Err(_) => base,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(command), Some(address)) = (args.first(), args.get(1)) else {
        eprintln!("usage: windowcast-gamestream pair|apps|stream HOST [APP_ID [SECONDS]]");
        std::process::exit(2);
    };
    let credentials = Credentials::load_or_generate(&dir(), "NVIDIA GameStream Client")
        .unwrap_or_else(|e| fail(&e.to_string()));
    let client = GameStreamClient::new(credentials);
    let host = host(&client, address);
    match command.as_str() {
        "pair" => {
            let info = client
                .server_info(&host)
                .unwrap_or_else(|e| fail(&e.to_string()));
            let pin = new_pin();
            println!(
                "pairing with {} ({}): type the PIN {pin} on the host",
                info.hostname, info.app_version
            );
            let paired = client
                .pair(&host, &pin, "windowcast")
                .unwrap_or_else(|e| fail(&e.to_string()));
            std::fs::write(
                host_file(address),
                paired.cert.expect("a paired host has a certificate"),
            )
            .unwrap_or_else(|e| fail(&e.to_string()));
            println!("paired");
        }
        "apps" => {
            for app in client.apps(&host).unwrap_or_else(|e| fail(&e.to_string())) {
                println!("{}\t{}", app.id, app.title);
            }
        }
        "stream" => {
            let app: u32 = args
                .get(2)
                .and_then(|a| a.parse().ok())
                .unwrap_or_else(|| fail("stream needs an APP_ID (see apps)"));
            let seconds: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);
            let stream = client
                .stream(&host, app, &StreamRequest::default())
                .unwrap_or_else(|e| fail(&e.to_string()));
            let mut check = H264Check::new().unwrap_or_else(|e| fail(&e.to_string()));
            let (mut frames, mut keyframes, mut bytes, mut sound) = (0u64, 0u64, 0u64, 0u64);
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(seconds) {
                if let Ok(frame) = stream.frames.recv_timeout(Duration::from_millis(500)) {
                    frames += 1;
                    keyframes += u64::from(frame.idr);
                    bytes += frame.data.len() as u64;
                    let _ = check.decode(&frame.data);
                }
                sound += stream.audio.try_iter().count() as u64;
                if stream.ended.load(std::sync::atomic::Ordering::SeqCst) {
                    println!("the host ended the stream");
                    break;
                }
            }
            let secs = started.elapsed().as_secs_f64();
            println!(
                "{frames} frames ({keyframes} keyframes) in {secs:.1} s: {:.1} fps, {:.2} Mbit/s; OpenH264 decoded {} at {:?}; {sound} sound packets",
                frames as f64 / secs,
                bytes as f64 * 8.0 / secs / 1e6,
                check.pictures,
                check.dimensions
            );
            let _ = client.cancel(&host);
        }
        other => fail(&format!("unknown command {other}")),
    }
}

fn fail(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1)
}

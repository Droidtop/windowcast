//! Our GameStream client against our GameStream host on loopback: pair,
//! launch the test pattern window, set the stream up over RTSP, and decode
//! the frames that arrive (the first a keyframe). Uses the standard
//! GameStream ports, so it runs on Linux only (Windows machines often run
//! Sunshine on them).
#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::{TestPatternSource, HEIGHT, WIDTH};
use windowcast_cli_tools::H264Check;
use windowcast_gamestream::client::{GameStreamClient, Host};
use windowcast_gamestream::crypto::Credentials;
use windowcast_gamestream::server::{GameStreamServer, HTTPS_PORT};
use windowcast_gamestream::session::StreamRequest;
use windowcast_gamestream::windows::WindowApps;

#[test]
fn our_client_streams_from_our_host() {
    let dir = std::env::temp_dir().join(format!("windowcast-gs-stream-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = GameStreamServer::open(
        "windowcast test host",
        &dir,
        Arc::new(WindowApps(Arc::new(TestPatternSource))),
    )
    .unwrap();
    let (http, https, rtsp) = runtime.block_on(async {
        (
            tokio::net::TcpListener::bind(("127.0.0.1", 47989))
                .await
                .unwrap(),
            tokio::net::TcpListener::bind(("127.0.0.1", HTTPS_PORT))
                .await
                .unwrap(),
            tokio::net::TcpListener::bind(("127.0.0.1", 48010))
                .await
                .unwrap(),
        )
    });
    runtime.spawn(Arc::clone(&server).serve(http, https, rtsp));

    let client = GameStreamClient::new(Credentials::generate("NVIDIA GameStream Client").unwrap());
    let host = Host {
        address: "127.0.0.1".into(),
        http_port: 47989,
        https_port: HTTPS_PORT,
        cert: None,
    };
    let typist = {
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(30) {
                if !server.pairing_requests().is_empty() {
                    return server.enter_pin("2468", None);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            false
        })
    };
    let host = client.pair(&host, "2468", "stream test").unwrap();
    assert!(typist.join().unwrap());
    let app = client.apps(&host).unwrap()[0].id;

    let stream = client
        .stream(
            &host,
            app,
            &StreamRequest {
                width: WIDTH as u32,
                height: HEIGHT as u32,
                fps: 30,
                bitrate_kbps: 2000,
                packet_size: 1024,
            },
        )
        .unwrap();
    let mut check = H264Check::new().unwrap();
    let mut frames = 0;
    let mut first_idr = None;
    let deadline = Instant::now() + Duration::from_secs(15);
    while frames < 30 && Instant::now() < deadline {
        if let Ok(frame) = stream.frames.recv_timeout(Duration::from_millis(500)) {
            first_idr.get_or_insert(frame.idr);
            check.decode(&frame.data).unwrap();
            frames += 1;
        }
    }
    println!(
        "{frames} frames, {} decoded at {:?}",
        check.pictures, check.dimensions
    );
    assert_eq!(first_idr, Some(true), "the first frame must be a keyframe");
    assert!(frames >= 30, "only {frames} frames");
    assert_eq!(check.dimensions, Some((WIDTH, HEIGHT)));
    client.cancel(&host).unwrap();
    drop(stream);
    let _ = std::fs::remove_dir_all(dir);
}

//! Stock Moonlight (moonlight-qt, as Linux distributions package it)
//! against windowcast's GameStream host: `moonlight pair` with a PIN,
//! typed on our host as a person would, `moonlight list` shows our apps,
//! and `moonlight stream` plays the test pattern window. Needs moonlight-qt and a display for it (CI runs it under the
//! headless sway), and the standard GameStream ports free; runs only with
//! WINDOWCAST_TEST_MOONLIGHT set.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::TestPatternWithTone;
use windowcast_gamestream::server::{GameStreamServer, HTTPS_PORT};
use windowcast_gamestream::windows::WindowApps;

/// Runs moonlight with `args` until `done`, shown what it has printed so
/// far, says so (it is then given two seconds to finish) or `within` runs
/// out, then ends it: `moonlight pair` stays open on its result. Returns
/// what it printed.
fn moonlight(args: &[&str], within: Duration, done: impl Fn(&str) -> bool) -> String {
    let mut child = Command::new("moonlight")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("moonlight");
    let printed = Arc::new(Mutex::new(String::new()));
    let readers: Vec<_> = [
        Box::new(child.stdout.take().expect("stdout")) as Box<dyn Read + Send>,
        Box::new(child.stderr.take().expect("stderr")),
    ]
    .into_iter()
    .map(|pipe| {
        let printed = Arc::clone(&printed);
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let mut printed = printed.lock().unwrap();
                printed.push_str(&line);
                printed.push('\n');
            }
        })
    })
    .collect();
    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if done(&printed.lock().unwrap()) || start.elapsed() > within {
            std::thread::sleep(Duration::from_secs(2));
            let _ = Command::new("kill").arg(child.id().to_string()).status();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let status = child.wait().expect("moonlight status");
    for reader in readers {
        let _ = reader.join();
    }
    let printed = printed.lock().unwrap().clone();
    println!("moonlight {}: {status}\n{printed}", args.join(" "));
    printed
}

#[test]
fn stock_moonlight_pairs_and_lists_our_apps() {
    if std::env::var_os("WINDOWCAST_TEST_MOONLIGHT").is_none() {
        println!("skipped: needs moonlight-qt and a display; set WINDOWCAST_TEST_MOONLIGHT");
        return;
    }
    let dir = std::env::temp_dir().join(format!("windowcast-gs-moonlight-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    // The test pattern host's one window, as Moonlight's app.
    let server = GameStreamServer::open(
        "windowcast test host",
        &dir,
        Arc::new(WindowApps(Arc::new(TestPatternWithTone))),
    )
    .unwrap();
    // Ask for encrypted video, so Moonlight decrypts our video packets too.
    server
        .request_video_encryption
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (http, https) = runtime.block_on(async {
        (
            tokio::net::TcpListener::bind(("127.0.0.1", 47989))
                .await
                .unwrap(),
            tokio::net::TcpListener::bind(("127.0.0.1", HTTPS_PORT))
                .await
                .unwrap(),
        )
    });
    let rtsp = runtime
        .block_on(tokio::net::TcpListener::bind(("127.0.0.1", 48010)))
        .unwrap();
    runtime.spawn(Arc::clone(&server).serve(http, https, rtsp));

    // The person types the PIN Moonlight was given, here. moonlight-qt
    // builds its PIN key with QByteArray(hash.constData()), which stops at
    // the first zero byte (nvpairingmanager.cpp, generateAesKey), so about
    // one salt in sixteen gives it a key the host cannot know and it says
    // "Incorrect PIN"; a person pairs again, and so does this test.
    let mut pair = String::new();
    for _attempt in 0..4 {
        let typist = {
            let server = Arc::clone(&server);
            std::thread::spawn(move || {
                let start = Instant::now();
                while start.elapsed() < Duration::from_secs(60) {
                    if let Some(request) = server.pairing_requests().first() {
                        println!(
                            "{} asks to pair from {}",
                            request.device_name, request.address
                        );
                        return server.enter_pin("4721", None);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                false
            })
        };
        pair = moonlight(
            &["pair", "127.0.0.1", "--pin", "4721"],
            Duration::from_secs(60),
            |printed| server.paired_clients() == 1 || printed.contains("Incorrect PIN"),
        );
        assert!(typist.join().unwrap(), "Moonlight never asked to pair");
        if !pair.contains("Incorrect PIN") {
            break;
        }
        println!("Moonlight's PIN key had a zero byte; pairing again");
    }
    // Moonlight ends pairing with the HTTPS check, our certificates both
    // ways.
    assert!(
        pair.contains("https://127.0.0.1:47984/pair"),
        "Moonlight did not finish pairing"
    );
    assert_eq!(server.paired_clients(), 1);

    let listed = moonlight(&["list", "127.0.0.1"], Duration::from_secs(60), |_| false);
    assert!(
        listed.contains("windowcast test pattern"),
        "our app is not listed"
    );

    // And streams it: Moonlight sets the stream up over RTSP, pings the
    // video and audio ports, connects the control stream, and decodes what
    // comes, video and the window's tone both encrypted.
    let streamed = moonlight(
        &[
            "stream",
            "127.0.0.1",
            "windowcast test pattern",
            "--resolution",
            "640x360",
            "--fps",
            "30",
            "--video-codec",
            "H.264",
            "--video-decoder",
            "software",
            "--display-mode",
            "windowed",
        ],
        Duration::from_secs(15),
        |_| false,
    );
    assert!(
        streamed.contains("Received first video packet"),
        "no video reached Moonlight"
    );
    assert!(
        streamed.contains("Received first audio packet"),
        "no sound reached Moonlight"
    );
    assert!(
        !streamed.contains("Failed to decrypt audio packet"),
        "Moonlight could not decrypt the sound"
    );
    assert!(
        !streamed.contains("Failed to decrypt video packet"),
        "Moonlight could not decrypt the video"
    );
    assert!(
        streamed.contains("Decoding frame rate"),
        "Moonlight decoded no pictures"
    );
    assert!(
        !streamed.contains("Audio FEC has been disabled"),
        "Moonlight refused the sound's parity"
    );
    assert!(
        !streamed.contains("Terminating connection"),
        "Moonlight gave up on the stream"
    );
    let _ = std::fs::remove_dir_all(dir);
}

//! Stock Moonlight (moonlight-qt, as Linux distributions package it)
//! against windowcast's GameStream host: `moonlight pair` with a PIN,
//! typed on our host as a person would, then `moonlight list` shows our
//! apps. Needs moonlight-qt and a display for it (CI runs it under the
//! headless sway), and the standard GameStream ports free; runs only with
//! WINDOWCAST_TEST_MOONLIGHT set.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windowcast_gamestream::client::App;
use windowcast_gamestream::server::{GameStreamServer, HTTPS_PORT};

/// Runs moonlight with `args` until `done` says so (it is then given two
/// seconds to finish) or `within` runs out, then ends it: `moonlight pair`
/// stays open on its result. Returns what it printed.
fn moonlight(args: &[&str], within: Duration, done: impl Fn() -> bool) -> String {
    let mut child = Command::new("moonlight")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("moonlight");
    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if done() || start.elapsed() > within {
            std::thread::sleep(Duration::from_secs(2));
            let _ = Command::new("kill").arg(child.id().to_string()).status();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let output = child.wait_with_output().expect("moonlight output");
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!(
        "moonlight {}: {}
{printed}",
        args.join(" "),
        output.status
    );
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
    let server = GameStreamServer::open(
        "windowcast test host",
        &dir,
        Box::new(|| {
            vec![
                App {
                    id: 1,
                    title: "Desktop".into(),
                    hdr: false,
                },
                App {
                    id: 42,
                    title: "windowcast test pattern".into(),
                    hdr: false,
                },
            ]
        }),
    )
    .unwrap();
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
    runtime.spawn(Arc::clone(&server).serve(http, https));

    // The person types the PIN Moonlight was given, here.
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
    let pair = moonlight(
        &["pair", "127.0.0.1", "--pin", "4721"],
        Duration::from_secs(60),
        || server.paired_clients() == 1,
    );
    assert!(typist.join().unwrap(), "Moonlight never asked to pair");
    // Moonlight ends pairing with the HTTPS check, our certificates both
    // ways.
    assert!(
        pair.contains("https://127.0.0.1:47984/pair"),
        "Moonlight did not finish pairing"
    );
    assert_eq!(server.paired_clients(), 1);

    let listed = moonlight(&["list", "127.0.0.1"], Duration::from_secs(60), || false);
    assert!(
        listed.contains("windowcast test pattern"),
        "our app is not listed"
    );
    let _ = std::fs::remove_dir_all(dir);
}

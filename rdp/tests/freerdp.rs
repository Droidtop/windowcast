//! Stock FreeRDP (as Linux distributions package it, `wlfreerdp3`)
//! against windowcast's RDP host: it logs in with NLA and shows the test
//! pattern window, pulling pictures for as long as it stays. Needs FreeRDP
//! 3 and a Wayland display (CI runs it under the headless sway); runs only
//! with WINDOWCAST_TEST_FREERDP set.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::TestPatternSource;
use windowcast_host::WindowSource;
use windowcast_rdp::host::{serve_window, HostStats, WindowServer};
use windowcast_rdp::tls::HostIdentity;
use windowcast_rdp::Credentials;

#[test]
fn stock_freerdp_logs_in_and_shows_the_window() {
    if std::env::var_os("WINDOWCAST_TEST_FREERDP").is_none() {
        println!("skipped: set WINDOWCAST_TEST_FREERDP (needs wlfreerdp3 and a Wayland display)");
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let stats = Arc::new(HostStats::default());
    {
        let stats = Arc::clone(&stats);
        std::thread::spawn(move || {
            let source: Arc<dyn WindowSource> = Arc::new(TestPatternSource);
            let window = source.list_windows()[0].id;
            let identity = HostIdentity::generate("windowcast test host").unwrap();
            let server = WindowServer {
                source,
                window,
                credentials: Credentials {
                    username: "windowcast".into(),
                    password: "freerdp-test-password".into(),
                    domain: None,
                },
                identity: Arc::new(identity),
                stats,
                stop: Arc::new(AtomicBool::new(false)),
                input: true,
            };
            if let Err(e) = serve_window(listener, server) {
                eprintln!("host: {e}");
            }
        });
    }

    let mut freerdp = Command::new("wlfreerdp3")
        .args([
            &format!("/v:127.0.0.1:{port}"),
            "/u:windowcast",
            "/p:freerdp-test-password",
            "/cert:ignore",
            "/sec:nla",
            "-clipboard",
            "/log-level:INFO",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("wlfreerdp3");
    let start = Instant::now();
    let mut exited = None;
    while start.elapsed() < Duration::from_secs(12) {
        if let Some(status) = freerdp.try_wait().unwrap() {
            exited = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if exited.is_none() {
        let _ = Command::new("kill").arg(freerdp.id().to_string()).status();
    }
    let output = freerdp.wait_with_output().unwrap();
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("wlfreerdp3: {:?}\n{printed}", exited);
    let (sessions, pictures) = (
        stats.sessions.load(Ordering::SeqCst),
        stats.pictures.load(Ordering::SeqCst),
    );
    println!("host: {sessions} sessions, {pictures} pictures sent");
    assert!(exited.is_none(), "FreeRDP left on its own: {exited:?}");
    assert_eq!(sessions, 1, "FreeRDP did not log in");
    // Twelve seconds of a 30 fps window, after the login.
    assert!(pictures >= 100, "only {pictures} pictures reached FreeRDP");
}

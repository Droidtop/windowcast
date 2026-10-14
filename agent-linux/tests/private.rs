//! A private session on Linux (docs/BACKENDS.md, "A private session"): the
//! agent starts a headless sway of its own and serves its windows instead
//! of the seat's: a window there is listed, captured and takes keys, and
//! once the private session ends the agent serves the seat's windows again.
//! Runs where `WINDOWCAST_TEST_PRIVATE` is set, with sway installed (the
//! sway CI job, after the seat's session was locked: a private session is
//! out of the lock screen's reach) and is skipped elsewhere.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windowcast_agent_linux::capture::Capture;
use windowcast_agent_linux::{display, LinuxSource, Options};
use windowcast_host::WindowSource;
use windowcast_protocol::{InputEvent, PrivateSession};

const TITLE: &str = "windowcast private test";
/// The window's colour (cc3366) as BGRA.
const BGRA: [u8; 4] = [0x66, 0x33, 0xcc, 0xff];

fn wait_for<T>(what: &str, mut found: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = found() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_private_session_serves_a_headless_sway_of_its_own() {
    if std::env::var_os("WINDOWCAST_TEST_PRIVATE").is_none() {
        println!("skipped: starts a sway; set WINDOWCAST_TEST_PRIVATE to run");
        return;
    }
    let source = LinuxSource::new(Options {
        private_session: true,
        ..Options::default()
    });
    assert_eq!(
        source.host_state().private_session,
        PrivateSession::Available
    );
    source
        .set_private_session(true, None)
        .unwrap_or_else(|e| panic!("no private session: {}", e.reason));
    assert_eq!(source.host_state().private_session, PrivateSession::Active);
    let served = display::current().expect("the private session's sway");
    println!(
        "private session on {} ({})",
        served.name,
        served.sway.display()
    );

    // A program started there, as a client's launch would be.
    let mut child = Command::new(env!("CARGO_BIN_EXE_windowcast-test-window"))
        .args([TITLE, "cc3366"])
        .env("WAYLAND_DISPLAY", &served.name)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let lines = Arc::clone(&lines);
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                lines.lock().unwrap().push(line);
            }
        });
    }
    let window = wait_for("the window in the private session's list", || {
        source.list_windows().into_iter().find(|w| w.title == TITLE)
    });
    std::thread::sleep(Duration::from_millis(500));

    let mut capture = Capture::window(window.id).expect("a capture in the private session");
    let centre = wait_for("a picture", || {
        capture.poll(Duration::from_millis(200)).ok()?;
        let p = capture.picture()?;
        let at = (p.height / 2) * p.stride + (p.width / 2) * 4;
        Some([p.data[at], p.data[at + 1], p.data[at + 2], p.data[at + 3]])
    });
    assert_eq!(centre, BGRA, "the private window's picture");
    drop(capture);

    // Keys reach it through the private sway's own virtual keyboard.
    for pressed in [true, false] {
        source.input(
            &InputEvent::Key {
                keycode: 30,
                pressed,
            },
            Some(window.id),
        );
    }
    wait_for("A in the private window", || {
        lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l == "key 30 pressed")
            .then_some(())
    });

    // Ended: the seat's windows again, the private one not among them; its
    // program keeps running for the next private session.
    source.set_private_session(false, None).unwrap();
    assert_eq!(display::current(), None);
    assert!(
        !source.list_windows().iter().any(|w| w.title == TITLE),
        "the private window shows among the seat's"
    );
    assert!(matches!(child.try_wait(), Ok(None)), "its program stopped");
    source.set_private_session(true, None).unwrap();
    assert_eq!(display::current(), Some(served), "the same sway again");
    assert!(source.list_windows().iter().any(|w| w.title == TITLE));
    source.set_private_session(false, None).unwrap();
    let _ = child.kill();
    let _ = child.wait();
}

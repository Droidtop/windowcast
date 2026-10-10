//! What window capture does while the session is locked under sway
//! (Droidtop/tracker#467): the test window, which draws every frame, is
//! captured on its own and cut out of its output, then swaylock locks the
//! session (ext-session-lock-v1) and the test reports whether pictures keep
//! coming and what they show. A locked session stays locked, so it runs
//! only where `WINDOWCAST_TEST_SWAYLOCK` is set (the last step of the sway
//! CI job, with sway and swaylock running) and is skipped elsewhere.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windowcast_agent_linux::capture::Capture;
use windowcast_agent_linux::toplevels;

const TITLE: &str = "windowcast lock test";

fn centre(capture: &mut Capture) -> Option<[u8; 4]> {
    let p = capture.picture()?;
    let at = (p.height / 2) * p.stride + (p.width / 2) * 4;
    Some([p.data[at], p.data[at + 1], p.data[at + 2], p.data[at + 3]])
}

fn watch(name: &str, capture: &mut Capture, time: Duration) -> usize {
    let end = Instant::now() + time;
    let mut pictures = 0;
    let mut last = None;
    while Instant::now() < end {
        match capture.poll(Duration::from_millis(200)) {
            Ok(true) => {
                pictures += 1;
                last = centre(capture);
            }
            Ok(false) => {}
            Err(e) => {
                println!("{name}: capture failed: {e}");
                break;
            }
        }
    }
    println!("{name}: {pictures} pictures in {time:?}, last centre (BGRA) {last:?}");
    pictures
}

#[test]
fn window_capture_while_the_session_is_locked() {
    if std::env::var_os("WINDOWCAST_TEST_SWAYLOCK").is_none() {
        println!("skipped: locks the session; set WINDOWCAST_TEST_SWAYLOCK to run");
        return;
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_windowcast-test-window"))
        .args([TITLE, "3366cc"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || for _ in BufReader::new(stdout).lines() {});
    let deadline = Instant::now() + Duration::from_secs(20);
    let window = loop {
        if let Some(w) = toplevels::list_windows()
            .unwrap_or_default()
            .into_iter()
            .find(|w| w.title == TITLE)
        {
            break w.id;
        }
        assert!(Instant::now() < deadline, "the test window never appeared");
        std::thread::sleep(Duration::from_millis(100));
    };
    std::thread::sleep(Duration::from_millis(500));
    let mut own = Capture::window(window).expect("window capture");
    let mut cut = Capture::desktop(window).expect("output capture");
    let before = watch("unlocked window", &mut own, Duration::from_secs(2));
    watch("unlocked output", &mut cut, Duration::from_secs(2));
    assert!(before > 0, "no pictures before locking");

    // Red lock screen; -f returns once the session is locked.
    let locked = Command::new("swaylock")
        .args(["-f", "-c", "ff0000"])
        .status()
        .expect("swaylock");
    println!("swaylock -f: {locked}");
    std::thread::sleep(Duration::from_secs(1));
    watch("locked window", &mut own, Duration::from_secs(4));
    watch("locked output", &mut cut, Duration::from_secs(4));
    match Capture::window(window) {
        Ok(mut fresh) => {
            watch(
                "window capture started while locked",
                &mut fresh,
                Duration::from_secs(4),
            );
        }
        Err(e) => println!("window capture started while locked: {e}"),
    }
    let _ = child.kill();
    let _ = child.wait();
}

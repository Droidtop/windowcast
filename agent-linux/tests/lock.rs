//! A locked sway session keeps streaming its windows (docs/BACKENDS.md, "A
//! locked host"; Droidtop/tracker#467): the test window, which draws every
//! frame, is captured on its own and cut out of its output, then swaylock
//! locks the session (ext-session-lock-v1). The window's own capture keeps
//! showing the window, one started while locked too; the output shows the
//! lock screen. A locked session stays locked, so it runs only where
//! `WINDOWCAST_TEST_SWAYLOCK` is set (the last step of the sway CI job,
//! with sway and swaylock running) and is skipped elsewhere.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windowcast_agent_linux::capture::Capture;
use windowcast_agent_linux::toplevels;

const TITLE: &str = "windowcast lock test";
/// The test window's colour (3366cc) and the lock screen's (ff0000), as
/// BGRA.
const WINDOW_BGRA: [u8; 4] = [0xcc, 0x66, 0x33, 0xff];
const LOCK_BGRA: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

fn centre(capture: &mut Capture) -> Option<[u8; 4]> {
    let p = capture.picture()?;
    let at = (p.height / 2) * p.stride + (p.width / 2) * 4;
    Some([p.data[at], p.data[at + 1], p.data[at + 2], p.data[at + 3]])
}

/// Pictures over `time`, and the last one's centre.
fn watch(name: &str, capture: &mut Capture, time: Duration) -> (usize, Option<[u8; 4]>) {
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
            Err(e) => panic!("{name}: capture failed: {e}"),
        }
    }
    println!("{name}: {pictures} pictures in {time:?}, last centre (BGRA) {last:?}");
    (pictures, last)
}

#[test]
fn a_locked_session_keeps_streaming_windows() {
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
    let (before, _) = watch("unlocked window", &mut own, Duration::from_secs(2));
    watch("unlocked output", &mut cut, Duration::from_secs(2));
    assert!(before > 0, "no pictures before locking");

    // Red lock screen; -f returns once the session is locked.
    let locked = Command::new("swaylock")
        .args(["-f", "-c", "ff0000"])
        .status()
        .expect("swaylock");
    assert!(locked.success(), "swaylock -f: {locked}");
    std::thread::sleep(Duration::from_secs(1));

    let (pictures, last) = watch("locked window", &mut own, Duration::from_secs(4));
    assert!(pictures > 0, "the window's capture stopped while locked");
    assert_eq!(last, Some(WINDOW_BGRA), "the window's capture while locked");
    // The output, when it sends a picture at all, shows the lock screen.
    if let (_, Some(last)) = watch("locked output", &mut cut, Duration::from_secs(4)) {
        assert_eq!(last, LOCK_BGRA, "the output while locked");
    }
    let mut fresh = Capture::window(window).expect("window capture started while locked");
    let (pictures, last) = watch(
        "window capture started while locked",
        &mut fresh,
        Duration::from_secs(4),
    );
    assert!(pictures > 0, "a capture started while locked shows nothing");
    assert_eq!(last, Some(WINDOW_BGRA), "a capture started while locked");
    let _ = child.kill();
    let _ = child.wait();
}

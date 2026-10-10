//! Our RDP client against Windows' own Remote Desktop: it logs in to the
//! local machine (TLS and NLA with NTLM) as a user made for the test and
//! sees the session's desktop. Runs only where that is set up (CI's Windows
//! job enables Remote Desktop and makes the user):
//! WINDOWCAST_TEST_WINDOWS_RDP_USER and WINDOWCAST_TEST_WINDOWS_RDP_PASSWORD.

use std::time::{Duration, Instant};

use windowcast_rdp::client::{connect, ClientConfig};

#[test]
fn our_client_logs_in_to_windows_remote_desktop() {
    let (Ok(user), Ok(password)) = (
        std::env::var("WINDOWCAST_TEST_WINDOWS_RDP_USER"),
        std::env::var("WINDOWCAST_TEST_WINDOWS_RDP_PASSWORD"),
    ) else {
        println!("skipped: set WINDOWCAST_TEST_WINDOWS_RDP_USER and _PASSWORD");
        return;
    };
    let stream = connect(&ClientConfig {
        address: "127.0.0.1:3389".parse().unwrap(),
        server_name: "localhost".into(),
        username: user,
        password,
        domain: None,
        size: (1280, 720),
        pinned: None,
    })
    .unwrap();
    println!("logged in: desktop {:?}", stream.size);
    assert_eq!(stream.size, (1280, 720));

    // The new session draws its desktop (the logon screen, then the
    // shell): pictures arrive, and they are not all one colour.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut pictures = 0;
    let mut varied = false;
    while Instant::now() < deadline && !(pictures >= 3 && varied) {
        if let Ok(picture) = stream.pictures.recv_timeout(Duration::from_millis(500)) {
            pictures += 1;
            let first = &picture.data[..4];
            varied |= picture
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[..3] != first[..3]);
        }
        assert!(
            !stream.ended.load(std::sync::atomic::Ordering::SeqCst),
            "the session ended"
        );
    }
    println!("{pictures} picture updates; varied: {varied}");
    assert!(pictures >= 3, "only {pictures} pictures");
    assert!(varied, "the desktop is one flat colour");
}

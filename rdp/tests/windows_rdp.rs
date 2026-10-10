//! Our RDP client against Windows' own Remote Desktop: it logs in to the
//! local machine (TLS and NLA with NTLM) as a user made for the test and
//! sees the session's desktop; and as a RemoteApp client, it has Notepad
//! run and sees only Notepad's window (CI allows unlisted programs). Runs only where that is set up (CI's Windows
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
    // IronRDP's and sspi's own account of the login, for when it fails.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,ironrdp=debug,sspi=debug,windowcast_rdp=debug".into()),
        )
        .try_init();
    let stream = connect(&ClientConfig {
        address: "127.0.0.1:3389".parse().unwrap(),
        server_name: "localhost".into(),
        username: user,
        password,
        domain: None,
        size: (1280, 720),
        pinned: None,
        remote_app: None,
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
        if let Ok(picture) = stream.next_picture(Duration::from_millis(500)) {
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

#[test]
fn our_client_runs_notepad_as_a_remoteapp() {
    // A user of its own: a second login of the same user would take over
    // the desktop test's session, which is no RemoteApp session.
    let (Ok(user), Ok(password)) = (
        std::env::var("WINDOWCAST_TEST_WINDOWS_RDP_APP_USER"),
        std::env::var("WINDOWCAST_TEST_WINDOWS_RDP_PASSWORD"),
    ) else {
        println!("skipped: set WINDOWCAST_TEST_WINDOWS_RDP_USER and _PASSWORD");
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,windowcast_rdp=debug".into()),
        )
        .try_init();
    let stream = connect(&ClientConfig {
        address: "127.0.0.1:3389".parse().unwrap(),
        server_name: "localhost".into(),
        username: user,
        password,
        domain: None,
        size: (1280, 720),
        pinned: None,
        remote_app: Some(windowcast_rdp::remoteapp::RemoteApp {
            program: "C:\\Windows\\System32\\notepad.exe".into(),
            arguments: String::new(),
            working_dir: String::new(),
        }),
    })
    .unwrap();
    // Pictures come once the server has described Notepad's window, and
    // are that window: smaller than the desktop.
    let deadline = Instant::now() + Duration::from_secs(90);
    let picture = loop {
        assert!(Instant::now() < deadline, "no RemoteApp window appeared");
        assert!(
            !stream.ended.load(std::sync::atomic::Ordering::SeqCst),
            "the session ended"
        );
        if let Ok(picture) = stream.next_picture(Duration::from_millis(500)) {
            break picture;
        }
    };
    println!("RemoteApp window: {}x{}", picture.width, picture.height);
    assert!(picture.width >= 100 && picture.height >= 100);
    assert!(
        (picture.width, picture.height) != (1280, 720),
        "the whole desktop came, not a window"
    );
}

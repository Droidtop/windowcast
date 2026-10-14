//! Compatibility mode (docs/BACKENDS.md, "Third-party clients and
//! RemoteApp"): a window a session streams is also offered to third-party
//! clients while the session streams it. Here an RDP client (windowcast's
//! own, as any RDP client would) logs in to the RDP endpoint with the login
//! made for the window and sees it, a VNC viewer's handshake reaches the
//! VNC endpoint, and both close when the stream stops. A window the
//! session does not stream cannot be put in compatibility mode.

use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::{TestPatternSource, HEIGHT, WIDTH, WINDOW};
use windowcast_client::{Client, ClientSession, Event};
use windowcast_host::compat::CompatProvider;
use windowcast_host::{HostConfig, HostControl, WindowSource};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{CompatEndpoint, VideoCodec};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("windowcast-compat-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn connect() -> (tokio::runtime::Runtime, ClientSession) {
    let host_dir = temp_dir("host");
    let client_dir = temp_dir("client");
    let host_id = Identity::load_or_generate(&host_dir.join("agent-identity.key"))
        .unwrap()
        .peer_id();
    let client_id = Identity::load_or_generate(&client_dir.join("client-identity.key"))
        .unwrap()
        .peer_id();
    let mut trust = TrustStore::default();
    trust.pin(client_id);
    trust.save(&host_dir.join("agent-trusted-clients")).unwrap();
    let mut trust = TrustStore::default();
    trust.pin(host_id);
    trust
        .save(&client_dir.join("client-trusted-hosts"))
        .unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let control = HostControl::open(&HostConfig {
        listen: address.clone(),
        pairing: false,
        data_dir: host_dir,
    })
    .unwrap();
    let loopback = std::net::IpAddr::from([127, 0, 0, 1]);
    let served = Arc::new(
        windowcast_rdp::host::WithRdp::new(Arc::new(TestPatternSource), loopback).unwrap(),
    );
    control.compatibility().set_providers(vec![
        Arc::clone(&served) as Arc<dyn CompatProvider>,
        Arc::new(windowcast_vnc::VncCompat::new(
            Arc::new(TestPatternSource),
            loopback,
        )),
    ]);
    runtime.spawn(windowcast_host::serve_with(
        listener,
        control,
        served as Arc<dyn WindowSource>,
    ));
    let session = Client::new(&client_dir)
        .unwrap()
        .connect(&address, None)
        .unwrap();
    (runtime, session)
}

/// Events until `wanted` gives something, within 30 s.
fn wait_for<T>(
    session: &ClientSession,
    what: &str,
    mut wanted: impl FnMut(Event) -> Option<T>,
) -> T {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        if let Some(found) = session
            .next_event(Duration::from_millis(200))
            .and_then(&mut wanted)
        {
            return found;
        }
    }
}

fn compatibility(event: Event) -> Option<(Vec<CompatEndpoint>, Vec<String>, Option<String>)> {
    match event {
        Event::Compatibility {
            endpoints,
            described,
            refused,
            ..
        } => Some((endpoints, described, refused)),
        _ => None,
    }
}

#[test]
fn a_streamed_window_is_offered_to_rdp_and_vnc_clients_while_it_streams() {
    let (_runtime, session) = connect();
    session.request_windows().unwrap();

    // Not streamed: refused.
    session.set_compatibility(WINDOW, true).unwrap();
    let (endpoints, _, refused) = wait_for(&session, "a refusal", compatibility);
    assert!(endpoints.is_empty());
    assert!(refused.is_some_and(|r| r.contains("streams")));

    session.start_window(WINDOW, &[VideoCodec::H264]).unwrap();
    wait_for(&session, "the stream", |e| {
        matches!(e, Event::StreamStarted { window, .. } if window == WINDOW.0).then_some(())
    });
    session.set_compatibility(WINDOW, true).unwrap();
    let (endpoints, described, refused) = wait_for(&session, "the endpoints", compatibility);
    assert_eq!(refused, None);
    for line in &described {
        println!("{line}");
    }
    assert_eq!(endpoints.len(), 2, "{endpoints:?}");

    // RDP: a client logs in with the window's login and sees the window.
    let Some(CompatEndpoint::Rdp(target)) = endpoints
        .iter()
        .find(|e| matches!(e, CompatEndpoint::Rdp(_)))
        .cloned()
    else {
        panic!("no RDP endpoint: {endpoints:?}");
    };
    assert_eq!(target.address, "127.0.0.1", "the host's address filled in");
    let rdp = windowcast_rdp::client::connect(&windowcast_rdp::client::ClientConfig {
        address: std::net::SocketAddr::from(([127, 0, 0, 1], target.port)),
        server_name: "127.0.0.1".into(),
        username: target.username.clone(),
        password: target.password.clone(),
        domain: None,
        size: (1024, 768),
        pinned: target.certificate_sha256,
        remote_app: None,
    })
    .expect("an RDP login with the window's login");
    let picture = rdp
        .next_picture(Duration::from_secs(20))
        .expect("the window over RDP");
    assert_eq!(
        (picture.width as usize, picture.height as usize),
        (WIDTH, HEIGHT)
    );
    rdp.stop();

    // VNC: the server speaks RFB on the port it gave.
    let Some(CompatEndpoint::Vnc { port, password }) = endpoints
        .iter()
        .find(|e| matches!(e, CompatEndpoint::Vnc { .. }))
        .cloned()
    else {
        panic!("no VNC endpoint: {endpoints:?}");
    };
    assert_eq!(password.len(), 8);
    let mut vnc = TcpStream::connect(("127.0.0.1", port)).unwrap();
    vnc.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut version = [0u8; 12];
    std::io::Read::read_exact(&mut vnc, &mut version).unwrap();
    assert_eq!(&version, b"RFB 003.008\n");
    drop(vnc);

    // The stream stops: so does compatibility mode.
    session.stop_window(WINDOW).unwrap();
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "the VNC server outlived the stream"
    );
}

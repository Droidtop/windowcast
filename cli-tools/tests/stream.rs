//! End to end through the library's two ends: the host library serving the
//! OpenH264 test pattern, the client library receiving it, and a software
//! decoder checking every frame is real H.264 of the right size.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use windowcast_cli_tools::testpattern::{TestPatternSource, HEIGHT, WIDTH, WINDOW};
use windowcast_cli_tools::H264Check;
use windowcast_client::{Client, Event, FramePoll};
use windowcast_host::HostConfig;
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::VideoCodec;

const WAIT: Duration = Duration::from_secs(20);

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("windowcast-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_client_streams_and_decodes_the_test_pattern() {
    // Identities pinned on both sides beforehand: this test is about the
    // stream, the pairing tests are in transport.
    let host_dir = temp_dir("host");
    let client_dir = temp_dir("client");
    let host_id = Identity::load_or_generate(&host_dir.join("agent-identity.key"))
        .unwrap()
        .peer_id();
    let client_id = Identity::load_or_generate(&client_dir.join("client-identity.key"))
        .unwrap()
        .peer_id();
    let mut host_trust = TrustStore::default();
    host_trust.pin(client_id);
    host_trust
        .save(&host_dir.join("agent-trusted-clients"))
        .unwrap();
    let mut client_trust = TrustStore::default();
    client_trust.pin(host_id);
    client_trust
        .save(&client_dir.join("client-trusted-hosts"))
        .unwrap();

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let config = HostConfig {
        listen: address.clone(),
        pairing: false,
        data_dir: host_dir.clone(),
    };
    runtime.spawn(windowcast_host::serve(
        listener,
        config,
        Arc::new(TestPatternSource),
    ));

    let client = Client::new(&client_dir).unwrap();
    let session = client.connect(&address, None).unwrap();
    assert!(!session.paired());

    session.request_windows().unwrap();
    match session.next_event(WAIT) {
        Some(Event::Windows { windows }) => assert_eq!(windows[0].id, WINDOW),
        other => panic!("expected the window list, got {other:?}"),
    }

    session
        .start_window(WINDOW, &[VideoCodec::Av1, VideoCodec::H264])
        .unwrap();
    match session.next_event(WAIT) {
        Some(Event::StreamStarted { codec, .. }) => assert_eq!(codec, Some(VideoCodec::H264)),
        other => panic!("expected the stream to start, got {other:?}"),
    }

    let mut check = H264Check::new().unwrap();
    let mut keyframes = 0;
    for n in 0..45 {
        match session.next_frame(WINDOW, WAIT) {
            FramePoll::Frame(frame) => {
                assert_eq!(frame.codec, VideoCodec::H264);
                if n == 0 {
                    assert!(frame.keyframe, "the first frame must be a keyframe");
                }
                keyframes += usize::from(frame.keyframe);
                check.decode(&frame.data).unwrap();
            }
            FramePoll::Timeout => panic!("frame {n} did not arrive"),
            FramePoll::Ended => panic!("the stream ended at frame {n}"),
        }
    }
    assert!(check.pictures >= 44, "decoded {} pictures", check.pictures);
    assert_eq!(check.dimensions, Some((WIDTH, HEIGHT)));
    assert!(keyframes >= 1);

    session.stop_window(WINDOW).unwrap();
    let ended = (0..100).any(|_| matches!(session.next_frame(WINDOW, WAIT), FramePoll::Ended));
    assert!(ended, "the stream should end after stopping it");

    drop(session);
    drop(client);
    runtime.shutdown_timeout(Duration::from_secs(1));
    let _ = std::fs::remove_dir_all(host_dir);
    let _ = std::fs::remove_dir_all(client_dir);
}

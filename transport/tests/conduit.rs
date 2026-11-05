//! A session carried by a tunnel someone else owns (Droidtop/tracker#113):
//! the host and the client each bind loopback only and reach each other
//! through a pair of in-memory conduits, as droidtop-agent's WireGuard
//! tunnel will carry them. Signaling goes over an in-memory pipe, as it will
//! over the agent's own channel.

use std::time::Duration;

use bytes::Bytes;
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{ControlMessage, VideoCodec, WindowId};
use windowcast_transport::conduit;
use windowcast_transport::{accept, connect, ClientCredential, HostCredential, Session};

const TEST_TIMEOUT: Duration = Duration::from_secs(40);

/// An H.264 unit with an Annex B start code: `header`, then `len` bytes.
fn nal(header: u8, len: usize, seed: u8) -> Vec<u8> {
    let mut unit = vec![0, 0, 0, 1, header];
    unit.extend((0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed) | 1));
    unit
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_over_a_conduit_carries_control_and_a_window() {
    let (host_identity, client_identity) = (Identity::generate(), Identity::generate());
    let (mut host_trust, mut client_trust) = (TrustStore::default(), TrustStore::default());
    host_trust.pin(client_identity.peer_id());
    client_trust.pin(host_identity.peer_id());

    let (host_end, client_end) = conduit::pair();
    let (host_pipe, client_pipe) = tokio::io::duplex(64 * 1024);
    let host = async {
        accept(
            host_pipe,
            Session::over_conduit(host_end).await.unwrap(),
            &host_identity,
            HostCredential {
                pin: None,
                trusted: &host_trust,
                accounts: None,
            },
        )
        .await
    };
    let client = async {
        connect(
            client_pipe,
            Session::over_conduit(client_end).await.unwrap(),
            &client_identity,
            ClientCredential::Pinned(&client_trust),
        )
        .await
    };
    let (host, client) = tokio::time::timeout(TEST_TIMEOUT, async { tokio::join!(host, client) })
        .await
        .expect("signaling over the pipe hung");
    let host = host.expect("host side").session;
    let client = client.expect("client side").session;

    client.send_control(&ControlMessage::Ping).await.unwrap();
    assert_eq!(
        tokio::time::timeout(TEST_TIMEOUT, host.recv_control())
            .await
            .unwrap()
            .unwrap(),
        ControlMessage::Ping
    );

    // A window: the host attaches it (a renegotiation, whose offer is also
    // rewritten to the bridge) and its keyframe arrives whole.
    let track = host
        .attach_window(WindowId(3), VideoCodec::H264)
        .await
        .unwrap();
    let keyframe: Vec<u8> = [nal(0x67, 12, 1), nal(0x68, 4, 1), nal(0x65, 3000, 1)].concat();
    track
        .write_frame(Bytes::from(keyframe.clone()), Duration::from_millis(16))
        .await
        .unwrap();
    let mut remote = tokio::time::timeout(TEST_TIMEOUT, client.next_remote_window())
        .await
        .expect("no window track arrived")
        .unwrap();
    assert_eq!(remote.window(), WindowId(3));
    let frame = tokio::time::timeout(TEST_TIMEOUT, remote.next_frame())
        .await
        .expect("the frame stalled")
        .unwrap();
    assert!(frame.keyframe);

    // Every byte went through the conduits: each side put datagrams in and
    // took the other's out.
    let (host_sent, host_received) = host.conduit_counts().unwrap();
    let (client_sent, client_received) = client.conduit_counts().unwrap();
    assert!(
        host_sent > 0 && host_received > 0,
        "host {host_sent}/{host_received}"
    );
    assert!(
        client_sent > 0 && client_received > 0,
        "client {client_sent}/{client_received}"
    );
    assert!(Session::new().await.unwrap().conduit_counts().is_none());
}

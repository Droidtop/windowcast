//! Two real peers in one process: the host and client each run their own
//! WebRTC stack, signal over a TCP socket on 127.0.0.1 (or an in-memory
//! pipe when a test needs to sit in the middle), and connect over ICE on
//! this machine's interfaces.

use std::time::Duration;

use bytes::Bytes;
use tokio::io::DuplexStream;
use tokio::net::{TcpListener, TcpStream};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{ControlMessage, SdpKind, SignalMessage, WindowId};
use windowcast_transport::signaling::{read_message, write_message};
use windowcast_transport::{
    accept, connect, ClientCredential, Established, HostCredential, Session, TransportError,
};

const TEST_TIMEOUT: Duration = Duration::from_secs(40);

struct Peers {
    host_identity: Identity,
    client_identity: Identity,
    host_trust: TrustStore,
    client_trust: TrustStore,
}

impl Peers {
    fn new() -> Self {
        Peers {
            host_identity: Identity::generate(),
            client_identity: Identity::generate(),
            host_trust: TrustStore::default(),
            client_trust: TrustStore::default(),
        }
    }

    fn pin_each_other(&mut self) {
        self.host_trust.pin(self.client_identity.peer_id());
        self.client_trust.pin(self.host_identity.peer_id());
    }

    /// Runs one signaling exchange over TCP on 127.0.0.1.
    async fn run(
        &self,
        host_pin: Option<&str>,
        client: ClientCredential<'_>,
    ) -> (
        Result<Established, TransportError>,
        Result<Established, TransportError>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let host = async {
            let (stream, _) = listener.accept().await.unwrap();
            accept(
                stream,
                Session::new().await.unwrap(),
                &self.host_identity,
                HostCredential {
                    pin: host_pin,
                    trusted: &self.host_trust,
                },
            )
            .await
        };
        let client = async {
            let stream = TcpStream::connect(address).await.unwrap();
            connect(
                stream,
                Session::new().await.unwrap(),
                &self.client_identity,
                client,
            )
            .await
        };
        tokio::time::timeout(TEST_TIMEOUT, async { tokio::join!(host, client) })
            .await
            .expect("signaling exchange hung")
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_by_pin_connects_and_carries_control_messages() {
    let peers = Peers::new();
    let (host, client) = peers
        .run(Some("482913"), ClientCredential::Pin("482913"))
        .await;
    let host = host.expect("host side");
    let client = client.expect("client side");

    assert!(host.paired && client.paired);
    assert_eq!(host.peer, peers.client_identity.peer_id());
    assert_eq!(client.peer, peers.host_identity.peer_id());

    client
        .session
        .send_control(&ControlMessage::Ping)
        .await
        .unwrap();
    assert_eq!(
        host.session.recv_control().await.unwrap(),
        ControlMessage::Ping
    );
    host.session
        .send_control(&ControlMessage::Pong)
        .await
        .unwrap();
    assert_eq!(
        client.session.recv_control().await.unwrap(),
        ControlMessage::Pong
    );

    // Closing one side ends the other's receive loop.
    client.session.close().await.unwrap();
    let ended = tokio::time::timeout(TEST_TIMEOUT, host.session.recv_control()).await;
    assert!(matches!(ended, Ok(Err(TransportError::Closed))));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_pin_is_refused() {
    let peers = Peers::new();
    let (host, client) = peers
        .run(Some("482913"), ClientCredential::Pin("000000"))
        .await;
    assert!(matches!(host, Err(TransportError::AuthenticationFailed)));
    assert!(matches!(client, Err(TransportError::Rejected(_))));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_not_open_for_pairing_refuses_a_pin() {
    let peers = Peers::new();
    let (host, client) = peers.run(None, ClientCredential::Pin("482913")).await;
    assert!(matches!(host, Err(TransportError::PairingNotOpen)));
    assert!(matches!(client, Err(TransportError::Rejected(_))));
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_works_between_pinned_peers_only() {
    let mut peers = Peers::new();

    // The host does not know the client: refused before anything else.
    let (host, client) = peers
        .run(None, ClientCredential::Pinned(&peers.client_trust))
        .await;
    assert!(matches!(host, Err(TransportError::UnknownPeer)));
    assert!(matches!(client, Err(TransportError::Rejected(_))));

    // The client does not know the host: it stops after the host's Hello.
    peers.host_trust.pin(peers.client_identity.peer_id());
    let (host, client) = peers
        .run(None, ClientCredential::Pinned(&peers.client_trust))
        .await;
    assert!(matches!(client, Err(TransportError::UnknownPeer)));
    assert!(host.is_err());

    peers.pin_each_other();
    let (host, client) = peers
        .run(None, ClientCredential::Pinned(&peers.client_trust))
        .await;
    let (host, client) = (host.expect("host side"), client.expect("client side"));
    assert!(!host.paired && !client.paired);
    client
        .session
        .send_control(&ControlMessage::Ping)
        .await
        .unwrap();
    assert_eq!(
        host.session.recv_control().await.unwrap(),
        ControlMessage::Ping
    );
}

/// Forwards signaling frames from `from` to `to`, letting `tamper` rewrite
/// each one: someone in the middle of the signaling path.
async fn relay(
    mut from: DuplexStream,
    mut to: DuplexStream,
    tamper: impl Fn(SignalMessage) -> SignalMessage,
) {
    loop {
        tokio::select! {
            message = read_message(&mut from) => match message {
                Ok(message) => if write_message(&mut to, &tamper(message)).await.is_err() { return },
                Err(_) => return,
            },
            message = read_message(&mut to) => match message {
                Ok(message) => if write_message(&mut from, &message).await.is_err() { return },
                Err(_) => return,
            },
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signaling_relay_that_swaps_the_fingerprint_is_caught() {
    let peers = Peers::new();
    let (client_end, relay_client_side) = tokio::io::duplex(64 * 1024);
    let (relay_host_side, host_end) = tokio::io::duplex(64 * 1024);

    // Replace the client's DTLS fingerprint with another certificate's, as
    // an attacker who wants to terminate DTLS themselves would.
    let attacker = Session::new().await.unwrap();
    let attacker_fingerprint =
        String::from_utf8(attacker.local_dtls_fingerprint().unwrap()).unwrap();
    tokio::spawn(relay(
        relay_client_side,
        relay_host_side,
        move |message| match message {
            SignalMessage::Description {
                kind: SdpKind::Offer,
                sdp,
                signature,
                pin_tag,
            } => {
                let sdp = sdp
                    .lines()
                    .map(|line| {
                        if line.starts_with("a=fingerprint:") {
                            format!("a=fingerprint:{attacker_fingerprint}")
                        } else {
                            line.to_owned()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\r\n");
                SignalMessage::Description {
                    kind: SdpKind::Offer,
                    sdp,
                    signature,
                    pin_tag,
                }
            }
            other => other,
        },
    ));

    let host = accept(
        host_end,
        Session::new().await.unwrap(),
        &peers.host_identity,
        HostCredential {
            pin: Some("482913"),
            trusted: &peers.host_trust,
        },
    );
    let client = connect(
        client_end,
        Session::new().await.unwrap(),
        &peers.client_identity,
        ClientCredential::Pin("482913"),
    );
    let (host, client) = tokio::time::timeout(TEST_TIMEOUT, async { tokio::join!(host, client) })
        .await
        .expect("signaling exchange hung");
    assert!(matches!(host, Err(TransportError::AuthenticationFailed)));
    assert!(matches!(client, Err(TransportError::Rejected(_))));
}

/// Splits an Annex-B buffer into its NAL units (start codes removed).
fn nal_units(annex_b: &[u8]) -> Vec<Vec<u8>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= annex_b.len() {
        if annex_b[i] == 0 && annex_b[i + 1] == 0 && annex_b[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let mut end = starts.get(n + 1).map_or(annex_b.len(), |&next| next - 3);
            while end > start && annex_b[end - 1] == 0 {
                end -= 1;
            }
            annex_b[start..end].to_vec()
        })
        .collect()
}

/// A NAL unit of `len` bytes with header byte `header` and a body free of
/// zero bytes (so it can never contain a start code). Not a decodable
/// picture: this test checks the transport carries bytes intact.
fn nal(header: u8, len: usize, seed: u8) -> Vec<u8> {
    let mut unit = vec![header];
    unit.extend((0..len - 1).map(|i| ((i as u32 + seed as u32) % 251 + 1) as u8));
    unit
}

fn annex_b(units: &[Vec<u8>]) -> Bytes {
    let mut out = Vec::new();
    for unit in units {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(unit);
    }
    Bytes::from(out)
}

#[tokio::test(flavor = "multi_thread")]
async fn window_tracks_attach_carry_frames_and_detach() {
    let peers = Peers::new();
    let (host, client) = peers
        .run(Some("482913"), ClientCredential::Pin("482913"))
        .await;
    let host = host.expect("host side").session;
    let client = client.expect("client side").session;

    // Two windows at once on the one session.
    let mut sent = Vec::new();
    for (window, seed) in [(WindowId(7), 1u8), (WindowId(9), 2u8)] {
        let track = host.attach_window(window).await.unwrap();
        assert_eq!(track.track_id(), format!("window-{}", window.0));
        // A keyframe access unit (SPS, PPS, IDR slice), then a P-slice big
        // enough to be fragmented across several RTP packets.
        let keyframe = vec![
            nal(0x67, 12, seed),
            nal(0x68, 4, seed),
            nal(0x65, 900, seed),
        ];
        let delta = vec![nal(0x41, 5000, seed)];
        for frame in [&keyframe, &delta] {
            track
                .write_frame(annex_b(frame), Duration::from_millis(16))
                .await
                .unwrap();
        }
        sent.push((window, keyframe, delta));
    }

    let mut received = Vec::new();
    for _ in 0..2 {
        let mut remote = tokio::time::timeout(TEST_TIMEOUT, client.next_remote_window())
            .await
            .expect("no window track arrived")
            .unwrap();
        let first = remote.next_frame().await.unwrap();
        let second = remote.next_frame().await.unwrap();
        received.push((
            remote.window(),
            nal_units(&first.data),
            nal_units(&second.data),
            remote,
        ));
    }
    received.sort_by_key(|(window, ..)| window.0);
    for ((window, keyframe, delta), (got_window, got_keyframe, got_delta, _)) in
        sent.iter().zip(&received)
    {
        assert_eq!(window, got_window);
        assert_eq!(keyframe, got_keyframe);
        assert_eq!(delta, got_delta);
    }

    // Detaching one window ends its track and leaves the other running.
    assert!(host.detach_window(WindowId(7)).await.unwrap());
    assert!(!host.detach_window(WindowId(7)).await.unwrap());
    let (_, _, _, mut window_7) = received.remove(0);
    let ended = tokio::time::timeout(TEST_TIMEOUT, window_7.next_frame()).await;
    assert!(matches!(ended, Ok(Err(_))), "window 7 track should end");

    let track_9 = host.attach_window(WindowId(9)).await.unwrap();
    let more = vec![nal(0x41, 300, 3)];
    track_9
        .write_frame(annex_b(&more), Duration::from_millis(16))
        .await
        .unwrap();
    let (_, _, _, mut window_9) = received.remove(0);
    let frame = tokio::time::timeout(TEST_TIMEOUT, window_9.next_frame())
        .await
        .expect("window 9 stalled")
        .unwrap();
    assert_eq!(nal_units(&frame.data), more);
}

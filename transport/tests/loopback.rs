//! Two real peers in one process: the host and client each run their own
//! WebRTC stack, signal over a TCP socket on 127.0.0.1 (or an in-memory
//! pipe when a test needs to sit in the middle), and connect over ICE on
//! this machine's interfaces.

use std::time::Duration;

use bytes::Bytes;
use tokio::io::DuplexStream;
use tokio::net::{TcpListener, TcpStream};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{ControlMessage, SdpKind, SignalMessage, VideoCodec, WindowId};
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
    let attacker_fingerprint = format!(
        "sha-256 {}",
        (0..32)
            .map(|i| format!("{:02X}", i * 7))
            .collect::<Vec<_>>()
            .join(":")
    );
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

/// Test bitstream units. Not decodable pictures: these tests check the
/// transport carries each codec's units intact. Bodies are free of zero
/// bytes, so an Annex-B body can never contain a start code.
fn body(len: usize, seed: u8) -> impl Iterator<Item = u8> {
    (0..len).map(move |i| ((i as u32 + seed as u32) % 251 + 1) as u8)
}

/// An H.264 (1-byte header) or H.265 (2-byte header) NAL unit.
fn nal(header: &[u8], len: usize, seed: u8) -> Vec<u8> {
    header.iter().copied().chain(body(len, seed)).collect()
}

/// An AV1 OBU with its size field: (type, payload) on the wire.
fn obu(obu_type: u8, len: usize, seed: u8) -> Vec<u8> {
    let mut unit = vec![(obu_type << 3) | 0x02];
    let mut size = len;
    loop {
        let byte = (size & 0x7f) as u8;
        size >>= 7;
        if size == 0 {
            unit.push(byte);
            break;
        }
        unit.push(byte | 0x80);
    }
    unit.extend(body(len, seed));
    unit
}

fn encode_units(codec: VideoCodec, units: &[Vec<u8>]) -> Bytes {
    let mut out = Vec::new();
    for unit in units {
        if codec != VideoCodec::Av1 {
            out.extend_from_slice(&[0, 0, 0, 1]);
        }
        out.extend_from_slice(unit);
    }
    Bytes::from(out)
}

/// Splits a received frame back into units: NAL units without start codes,
/// or OBUs re-encoded as sent (temporal delimiters dropped, as the AV1 RTP
/// format omits them).
fn decode_units(codec: VideoCodec, data: &[u8]) -> Vec<Vec<u8>> {
    if codec == VideoCodec::Av1 {
        let mut units = Vec::new();
        let mut rest = data;
        while let Some(&header) = rest.first() {
            let obu_type = (header >> 3) & 0x0f;
            let mut offset = 1 + usize::from(header & 0x04 != 0);
            let mut size = 0usize;
            for i in 0..8 {
                let byte = rest[offset];
                offset += 1;
                size |= usize::from(byte & 0x7f) << (7 * i);
                if byte & 0x80 == 0 {
                    break;
                }
            }
            let payload = &rest[offset..offset + size];
            if obu_type != 2 {
                let mut unit = vec![(obu_type << 3) | 0x02];
                let mut s = size;
                loop {
                    let byte = (s & 0x7f) as u8;
                    s >>= 7;
                    if s == 0 {
                        unit.push(byte);
                        break;
                    }
                    unit.push(byte | 0x80);
                }
                unit.extend_from_slice(payload);
                units.push(unit);
            }
            rest = &rest[offset + size..];
        }
        return units;
    }
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
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
            let mut end = starts.get(n + 1).map_or(data.len(), |&next| next - 3);
            while end > start && data[end - 1] == 0 {
                end -= 1;
            }
            data[start..end].to_vec()
        })
        .collect()
}

/// A keyframe (parameter sets plus a key picture) and a delta frame big
/// enough to be fragmented across several RTP packets.
fn test_frames(codec: VideoCodec, seed: u8) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    match codec {
        VideoCodec::H264 => (
            vec![
                nal(&[0x67], 12, seed),
                nal(&[0x68], 4, seed),
                nal(&[0x65], 900, seed),
            ],
            vec![nal(&[0x41], 5000, seed)],
        ),
        VideoCodec::H265 => (
            vec![
                nal(&[0x40, 0x01], 20, seed),
                nal(&[0x42, 0x01], 30, seed),
                nal(&[0x44, 0x01], 6, seed),
                nal(&[0x26, 0x01], 900, seed),
            ],
            vec![nal(&[0x02, 0x01], 5000, seed)],
        ),
        VideoCodec::Av1 => (
            vec![obu(1, 12, seed), obu(6, 900, seed)],
            vec![obu(6, 5000, seed)],
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn window_tracks_in_each_codec_attach_carry_frames_and_detach() {
    let peers = Peers::new();
    let (host, client) = peers
        .run(Some("482913"), ClientCredential::Pin("482913"))
        .await;
    let host = host.expect("host side").session;
    let client = client.expect("client side").session;

    // Three windows at once on the one session, one per codec.
    let windows = [
        (WindowId(7), VideoCodec::H264, 1u8),
        (WindowId(9), VideoCodec::H265, 2u8),
        (WindowId(11), VideoCodec::Av1, 3u8),
    ];
    let mut sent = Vec::new();
    for (window, codec, seed) in windows {
        let track = host.attach_window(window, codec).await.unwrap();
        assert_eq!(track.track_id(), format!("window-{}", window.0));
        let (keyframe, delta) = test_frames(codec, seed);
        for frame in [&keyframe, &delta] {
            track
                .write_frame(encode_units(codec, frame), Duration::from_millis(16))
                .await
                .unwrap();
        }
        sent.push((window, codec, keyframe, delta));
    }

    let mut received = Vec::new();
    for _ in 0..windows.len() {
        let mut remote = tokio::time::timeout(TEST_TIMEOUT, client.next_remote_window())
            .await
            .expect("no window track arrived")
            .unwrap();
        let first = tokio::time::timeout(TEST_TIMEOUT, remote.next_frame())
            .await
            .expect("first frame stalled")
            .unwrap();
        let second = tokio::time::timeout(TEST_TIMEOUT, remote.next_frame())
            .await
            .expect("second frame stalled")
            .unwrap();
        assert!(first.keyframe && !second.keyframe);
        let codec = remote.codec();
        received.push((
            remote.window(),
            codec,
            decode_units(codec, &first.data),
            decode_units(codec, &second.data),
            remote,
        ));
    }
    received.sort_by_key(|(window, ..)| window.0);
    for ((window, codec, keyframe, delta), (got_window, got_codec, got_keyframe, got_delta, _)) in
        sent.iter().zip(&received)
    {
        assert_eq!(window, got_window);
        assert_eq!(codec, got_codec);
        assert_eq!(keyframe, got_keyframe, "{codec:?} keyframe");
        assert_eq!(delta, got_delta, "{codec:?} delta frame");
    }

    // Detaching one window ends its track and leaves the others running.
    assert!(host.detach_window(WindowId(7)).await.unwrap());
    assert!(!host.detach_window(WindowId(7)).await.unwrap());
    let (.., mut window_7) = received.remove(0);
    let ended = tokio::time::timeout(TEST_TIMEOUT, window_7.next_frame()).await;
    assert!(matches!(ended, Ok(Err(_))), "window 7 track should end");

    let track_9 = host
        .attach_window(WindowId(9), VideoCodec::H265)
        .await
        .unwrap();
    let more = vec![nal(&[0x02, 0x01], 300, 4)];
    track_9
        .write_frame(
            encode_units(VideoCodec::H265, &more),
            Duration::from_millis(16),
        )
        .await
        .unwrap();
    let (.., mut window_9) = received.remove(0);
    let frame = tokio::time::timeout(TEST_TIMEOUT, window_9.next_frame())
        .await
        .expect("window 9 stalled")
        .unwrap();
    assert_eq!(decode_units(VideoCodec::H265, &frame.data), more);
}

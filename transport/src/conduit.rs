//! A session carried by a tunnel someone else owns (Droidtop/tracker#113).
//!
//! droidtop-agent already keeps one tunnel between a handheld and a
//! computer (userspace WireGuard), and the owner's rule is that it is also
//! the streaming tunnel: "Our standard carrier tunnel is also a remote
//! streaming tunnel". A [`Conduit`] is that tunnel as this crate sees it,
//! two queues of datagrams to and from the one peer, so nothing here knows
//! what carries them.
//!
//! WebRTC itself is unchanged. The session binds loopback only, and a
//! [`Bridge`] socket on loopback stands in for the peer: every remote
//! description the session takes names the bridge as the peer's one
//! candidate ([`rewrite_candidates`]), the bridge sends what the session
//! sends it into the conduit, and gives what arrives from the conduit to the
//! session's socket. Both sides do the same, so the ICE checks, DTLS, the
//! data channel and the media all cross the tunnel and nothing else.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};

/// The largest datagram a bridge reads: WebRTC keeps its own below the path
/// MTU, so this only has to be large enough for any UDP datagram.
const MAX_DATAGRAM: usize = 65_535;

/// The tunnel, as two queues of whole datagrams to and from the one peer.
/// The tunnel's owner moves them: what is put on `outbound` goes to the
/// peer's conduit, and what the peer sent is put on the receiver's
/// `inbound`.
pub struct Conduit {
    pub outbound: mpsc::UnboundedSender<Bytes>,
    pub inbound: mpsc::UnboundedReceiver<Bytes>,
}

/// Two conduits joined to each other, as a tunnel would join them: for
/// tests, and for two peers in one process.
pub fn pair() -> (Conduit, Conduit) {
    let (a_out, b_in) = mpsc::unbounded_channel();
    let (b_out, a_in) = mpsc::unbounded_channel();
    (
        Conduit {
            outbound: a_out,
            inbound: a_in,
        },
        Conduit {
            outbound: b_out,
            inbound: b_in,
        },
    )
}

/// The loopback socket that stands in for the peer, and the two tasks that
/// move datagrams between it and the conduit. Dropping it stops them.
pub struct Bridge {
    address: SocketAddr,
    sent: Arc<AtomicU64>,
    received: Arc<AtomicU64>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Bridge {
    /// Binds the bridge on loopback and starts moving datagrams.
    pub async fn start(conduit: Conduit) -> std::io::Result<Self> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let address = socket.local_addr()?;
        // The session's own socket: whoever sends to the bridge. Datagrams
        // from the peer wait for it to be known; ICE sends again until then.
        let session: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
        let (sent, received) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let Conduit {
            outbound,
            mut inbound,
        } = conduit;

        let to_tunnel = {
            let (socket, session, sent) =
                (Arc::clone(&socket), Arc::clone(&session), Arc::clone(&sent));
            tokio::spawn(async move {
                let mut buf = vec![0u8; MAX_DATAGRAM];
                loop {
                    let (n, from) = match socket.recv_from(&mut buf).await {
                        Ok(got) => got,
                        Err(e) => {
                            tracing::debug!("conduit bridge: receive failed: {e}");
                            continue;
                        }
                    };
                    *session.lock().await = Some(from);
                    if outbound.send(Bytes::copy_from_slice(&buf[..n])).is_err() {
                        break;
                    }
                    sent.fetch_add(1, Ordering::Relaxed);
                }
            })
        };
        let from_tunnel = {
            let received = Arc::clone(&received);
            tokio::spawn(async move {
                while let Some(datagram) = inbound.recv().await {
                    let Some(to) = *session.lock().await else {
                        continue;
                    };
                    if let Err(e) = socket.send_to(&datagram, to).await {
                        tracing::debug!("conduit bridge: send failed: {e}");
                        continue;
                    }
                    received.fetch_add(1, Ordering::Relaxed);
                }
            })
        };
        Ok(Bridge {
            address,
            sent,
            received,
            tasks: vec![to_tunnel, from_tunnel],
        })
    }

    /// Where the session sends to reach the peer.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Datagrams put into the conduit, and taken out of it, so far.
    pub fn counts(&self) -> (u64, u64) {
        (
            self.sent.load(Ordering::Relaxed),
            self.received.load(Ordering::Relaxed),
        )
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// `sdp` with every candidate the peer named replaced by one host
/// candidate at `bridge`, in each media section: through a conduit the
/// peer's own addresses are never the way to it.
pub fn rewrite_candidates(sdp: &str, bridge: SocketAddr) -> String {
    let candidate = format!(
        "a=candidate:1 1 udp 2130706431 {} {} typ host",
        bridge.ip(),
        bridge.port()
    );
    let mut out = String::with_capacity(sdp.len());
    for line in sdp.lines() {
        if line.starts_with("a=candidate:") {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
        if line.starts_with("a=mid:") {
            out.push_str(&candidate);
            out.push_str("\r\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_peers_candidates_become_the_bridge() {
        let sdp = "v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:0\r\n\
                   a=candidate:1 1 udp 2130706431 192.168.1.5 50002 typ host\r\n\
                   a=candidate:3 1 udp 1694498815 203.0.113.9 50003 typ srflx raddr 0.0.0.0 rport 0\r\n\
                   a=end-of-candidates\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:1\r\n";
        let out = rewrite_candidates(sdp, "127.0.0.1:40000".parse().unwrap());
        assert_eq!(
            out,
            "v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:0\r\n\
             a=candidate:1 1 udp 2130706431 127.0.0.1 40000 typ host\r\n\
             a=end-of-candidates\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:1\r\n\
             a=candidate:1 1 udp 2130706431 127.0.0.1 40000 typ host\r\n"
        );
    }

    #[tokio::test]
    async fn a_bridge_moves_datagrams_both_ways() {
        let (near, mut far) = pair();
        let bridge = Bridge::start(near).await.unwrap();
        let session = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        session.send_to(b"check", bridge.address()).await.unwrap();
        assert_eq!(
            far.inbound.recv().await.unwrap(),
            Bytes::from_static(b"check")
        );
        far.outbound.send(Bytes::from_static(b"reply")).unwrap();
        let mut buf = [0u8; 16];
        let (n, from) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.recv_from(&mut buf),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&buf[..n], b"reply");
        assert_eq!(from, bridge.address());
        assert_eq!(bridge.counts(), (1, 1));
    }
}

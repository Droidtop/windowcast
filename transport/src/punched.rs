//! Signaling away from the LAN: a reliable byte stream over one UDP socket
//! whose NAT mapping both peers punched (`windowcast-rendezvous` finds the
//! addresses). The session's offer and answer run over it exactly as over a
//! LAN TCP socket (`signaling`), authenticated end to end, so whoever
//! carries or tampers with these datagrams learns and changes nothing; the
//! session itself then makes its own way through ICE (`Session::away`).
//!
//! One [`Rendezvous`] socket serves many peers (a host) or one (a client):
//! it answers STUN for the rendezvous keepalive, ignores punch packets, and
//! hands each peer's segments to that peer's stream. A stream is a small
//! sliding-window protocol, enough for the few kilobytes signaling sends:
//!
//! - `SYN` opens (the initiator repeats it while the NATs are punched),
//!   `SYN` back accepts; both carry a random connection id.
//! - `DATA` carries up to [`SEGMENT`] bytes at a sequence number, at most
//!   [`WINDOW`] unacknowledged, resent after [`RESEND`] doubling to
//!   [`RESEND_MAX`]; `ACK` names the next sequence number wanted.
//! - `FIN` closes; a stream silent for [`IDLE`] is dropped.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rand_core::{OsRng, RngCore};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// Every segment starts with this.
const MAGIC: &[u8; 4] = b"WCS1";
const SYN: u8 = 1;
const DATA: u8 = 2;
const ACK: u8 = 3;
const FIN: u8 = 4;
/// Magic, kind, connection id, sequence number.
const HEADER: usize = 4 + 1 + 4 + 4;
/// Payload per segment: inside any path's MTU with room for IPv6 and a
/// tunnel or two.
pub const SEGMENT: usize = 1100;
/// Segments in flight.
pub const WINDOW: usize = 16;
pub const RESEND: Duration = Duration::from_millis(300);
pub const RESEND_MAX: Duration = Duration::from_secs(2);
/// A stream that hears nothing for this long is gone.
pub const IDLE: Duration = Duration::from_secs(30);
/// How often an opening stream repeats its `SYN`.
const SYN_EVERY: Duration = Duration::from_millis(250);

/// What the socket hands to the program's own use: STUN replies (the
/// rendezvous keepalive's), and anything else not a stream segment.
pub type Datagram = (Vec<u8>, SocketAddr);

/// One UDP socket for rendezvous: streams in and out, and the program's
/// own datagrams (STUN replies) beside them.
pub struct Rendezvous {
    socket: Arc<UdpSocket>,
    peers: Arc<Mutex<HashMap<SocketAddr, mpsc::UnboundedSender<Segment>>>>,
    incoming: tokio::sync::Mutex<mpsc::UnboundedReceiver<(DuplexStream, SocketAddr)>>,
    other: tokio::sync::Mutex<mpsc::UnboundedReceiver<Datagram>>,
}

#[derive(Debug, Clone)]
struct Segment {
    kind: u8,
    id: u32,
    seq: u32,
    payload: Vec<u8>,
}

fn encode(kind: u8, id: u32, seq: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.extend_from_slice(MAGIC);
    out.push(kind);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn decode(data: &[u8]) -> Option<Segment> {
    if data.len() < HEADER || &data[..4] != MAGIC {
        return None;
    }
    Some(Segment {
        kind: data[4],
        id: u32::from_be_bytes(data[5..9].try_into().ok()?),
        seq: u32::from_be_bytes(data[9..13].try_into().ok()?),
        payload: data[HEADER..].to_vec(),
    })
}

impl Rendezvous {
    /// Wraps a bound socket; `accept` says whether peers may open streams
    /// to it (a host) or only answer the ones it opens (a client).
    pub fn new(socket: UdpSocket, accept: bool) -> Arc<Self> {
        let socket = Arc::new(socket);
        let peers: Arc<Mutex<HashMap<SocketAddr, mpsc::UnboundedSender<Segment>>>> = Arc::default();
        let (incoming_tx, incoming) = mpsc::unbounded_channel();
        let (other_tx, other) = mpsc::unbounded_channel();

        let rendezvous = Arc::new(Rendezvous {
            socket: Arc::clone(&socket),
            peers: Arc::clone(&peers),
            incoming: tokio::sync::Mutex::new(incoming),
            other: tokio::sync::Mutex::new(other),
        });
        tokio::spawn(read_loop(socket, peers, incoming_tx, other_tx, accept));
        rendezvous
    }

    pub fn socket(&self) -> &Arc<UdpSocket> {
        &self.socket
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// The next stream a peer opened.
    pub async fn accept(&self) -> Option<(DuplexStream, SocketAddr)> {
        self.incoming.lock().await.recv().await
    }

    /// The next datagram that was not a stream segment.
    pub async fn next_datagram(&self) -> Option<Datagram> {
        self.other.lock().await.recv().await
    }

    /// Sends a one-byte punch: it opens this side's NAT towards `to` and
    /// is ignored there.
    pub async fn punch(&self, to: SocketAddr) {
        let _ = self.socket.send_to(&[0], to).await;
    }

    /// Opens a stream to `to`, repeating the opening (and so punching this
    /// side's NAT) until the peer answers or `within` runs out.
    pub async fn connect(
        self: &Arc<Self>,
        to: SocketAddr,
        within: Duration,
    ) -> std::io::Result<DuplexStream> {
        let id = OsRng.next_u32();
        let (tx, mut rx) = mpsc::unbounded_channel();
        self.peers.lock().expect("peers").insert(to, tx.clone());
        let deadline = Instant::now() + within;
        let syn = encode(SYN, id, 0, &[]);
        let answered = loop {
            if Instant::now() >= deadline {
                break false;
            }
            self.socket.send_to(&syn, to).await?;
            match tokio::time::timeout(SYN_EVERY, rx.recv()).await {
                Ok(Some(segment)) if segment.kind == SYN && segment.id == id => break true,
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => break false,
            }
        };
        if !answered {
            self.peers.lock().expect("peers").remove(&to);
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("{to} did not answer"),
            ));
        }
        let (mine, theirs) = tokio::io::duplex(64 * 1024);
        tokio::spawn(run_stream(
            Arc::clone(&self.socket),
            Arc::clone(&self.peers),
            to,
            id,
            rx,
            theirs,
        ));
        Ok(mine)
    }
}

async fn read_loop(
    socket: Arc<UdpSocket>,
    peers: Arc<Mutex<HashMap<SocketAddr, mpsc::UnboundedSender<Segment>>>>,
    incoming: mpsc::UnboundedSender<(DuplexStream, SocketAddr)>,
    other: mpsc::UnboundedSender<Datagram>,
    accepting: bool,
) {
    let mut buf = vec![0u8; 2048];
    loop {
        let Ok((n, from)) = socket.recv_from(&mut buf).await else {
            // An ICMP error from a closed port shows up here on some
            // systems; it is not the socket failing.
            continue;
        };
        let data = &buf[..n];
        let Some(segment) = decode(data) else {
            if n > 1 {
                let _ = other.send((data.to_vec(), from));
            }
            continue;
        };
        let known = peers.lock().expect("peers").get(&from).cloned();
        match known {
            Some(peer) if peer.send(segment.clone()).is_ok() => {}
            _ if segment.kind == SYN && accepting => {
                let (tx, rx) = mpsc::unbounded_channel();
                peers.lock().expect("peers").insert(from, tx);
                let _ = socket.send_to(&encode(SYN, segment.id, 0, &[]), from).await;
                let (mine, theirs) = tokio::io::duplex(64 * 1024);
                tokio::spawn(run_stream(
                    Arc::clone(&socket),
                    Arc::clone(&peers),
                    from,
                    segment.id,
                    rx,
                    theirs,
                ));
                if incoming.send((mine, from)).is_err() {
                    return;
                }
            }
            _ => {}
        }
    }
}

/// Moves bytes between one side of a duplex pipe and the peer's segments.
async fn run_stream(
    socket: Arc<UdpSocket>,
    peers: Arc<Mutex<HashMap<SocketAddr, mpsc::UnboundedSender<Segment>>>>,
    to: SocketAddr,
    id: u32,
    mut segments: mpsc::UnboundedReceiver<Segment>,
    pipe: DuplexStream,
) {
    let (mut pipe_in, mut pipe_out) = tokio::io::split(pipe);
    let mut next_out: u32 = 0;
    // Sent and not yet acknowledged: sequence number, bytes, last sent,
    // current resend interval.
    let mut unacked: VecDeque<(u32, Vec<u8>, Instant, Duration)> = VecDeque::new();
    let mut next_in: u32 = 0;
    let mut early: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut heard = Instant::now();
    // The program closed its side; our FIN went (and when); the peer's FIN
    // came, at this sequence number.
    let mut read_closed = false;
    let mut fin_sent: Option<Instant> = None;
    let mut peer_fin: Option<u32> = None;
    let mut buf = vec![0u8; SEGMENT];
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    loop {
        let can_send = !read_closed && unacked.len() < WINDOW;
        tokio::select! {
            read = pipe_in.read(&mut buf), if can_send => match read {
                Ok(0) | Err(_) => read_closed = true,
                Ok(n) => {
                    let payload = buf[..n].to_vec();
                    let _ = socket.send_to(&encode(DATA, id, next_out, &payload), to).await;
                    unacked.push_back((next_out, payload, Instant::now(), RESEND));
                    next_out = next_out.wrapping_add(1);
                }
            },
            segment = segments.recv() => {
                let Some(segment) = segment else { break };
                if segment.id != id {
                    continue;
                }
                heard = Instant::now();
                match segment.kind {
                    ACK => {
                        while unacked
                            .front()
                            .is_some_and(|(seq, ..)| segment.seq.wrapping_sub(*seq) as i32 > 0)
                        {
                            unacked.pop_front();
                        }
                    }
                    DATA => {
                        if segment.seq.wrapping_sub(next_in) as i32 >= 0 {
                            early.insert(segment.seq, segment.payload);
                        }
                        while let Some(payload) = early.remove(&next_in) {
                            if pipe_out.write_all(&payload).await.is_err() {
                                break;
                            }
                            next_in = next_in.wrapping_add(1);
                        }
                        let _ = socket.send_to(&encode(ACK, id, next_in, &[]), to).await;
                    }
                    FIN => {
                        peer_fin = Some(segment.seq);
                        let _ = socket.send_to(&encode(ACK, id, next_in, &[]), to).await;
                    }
                    // A repeated opening whose answer was lost.
                    SYN => {
                        let _ = socket.send_to(&encode(SYN, id, 0, &[]), to).await;
                    }
                    _ => {}
                }
            }
            _ = tick.tick() => {
                if heard.elapsed() > IDLE {
                    break;
                }
                let now = Instant::now();
                for (seq, payload, sent, every) in unacked.iter_mut() {
                    if now.duration_since(*sent) >= *every {
                        let _ = socket.send_to(&encode(DATA, id, *seq, payload), to).await;
                        *sent = now;
                        *every = (*every * 2).min(RESEND_MAX);
                    }
                }
                // Our side is done once everything sent is acknowledged:
                // say so, again now and then until the peer is done too.
                if read_closed
                    && unacked.is_empty()
                    && fin_sent.is_none_or(|at| now.duration_since(at) >= RESEND)
                {
                    let _ = socket.send_to(&encode(FIN, id, next_out, &[]), to).await;
                    fin_sent = Some(now);
                }
            }
        }
        // The peer's side is done once everything before its FIN arrived:
        // the program reads the end of the stream.
        if peer_fin == Some(next_in) {
            let _ = pipe_out.shutdown().await;
            if fin_sent.is_some() {
                break;
            }
        }
    }
    peers.lock().expect("peers").remove(&to);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_stream_carries_bytes_both_ways_in_order() {
        let host = Rendezvous::new(UdpSocket::bind("127.0.0.1:0").await.unwrap(), true);
        let client = Rendezvous::new(UdpSocket::bind("127.0.0.1:0").await.unwrap(), false);
        let at = host.local_addr().unwrap();
        let echo = tokio::spawn(async move {
            let (mut stream, _) = host.accept().await.unwrap();
            let mut got = vec![0u8; 50_000];
            stream.read_exact(&mut got).await.unwrap();
            stream.write_all(&got).await.unwrap();
            stream.flush().await.unwrap();
            // Kept open until the client has it all.
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let mut stream = client.connect(at, Duration::from_secs(5)).await.unwrap();
        let sent: Vec<u8> = (0..50_000u32).map(|i| (i * 7 % 251) as u8).collect();
        stream.write_all(&sent).await.unwrap();
        let mut back = vec![0u8; sent.len()];
        stream.read_exact(&mut back).await.unwrap();
        assert_eq!(back, sent);
        echo.await.unwrap();
    }

    #[tokio::test]
    async fn non_stream_datagrams_reach_the_program() {
        let a = Rendezvous::new(UdpSocket::bind("127.0.0.1:0").await.unwrap(), true);
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        b.send_to(b"a stun reply, say", a.local_addr().unwrap())
            .await
            .unwrap();
        // A punch is ignored.
        b.send_to(&[0], a.local_addr().unwrap()).await.unwrap();
        let (data, from) = a.next_datagram().await.unwrap();
        assert_eq!(data, b"a stun reply, say");
        assert_eq!(from, b.local_addr().unwrap());
    }

    #[tokio::test]
    async fn a_client_socket_refuses_strangers() {
        let client = Rendezvous::new(UdpSocket::bind("127.0.0.1:0").await.unwrap(), false);
        let other = Rendezvous::new(UdpSocket::bind("127.0.0.1:0").await.unwrap(), false);
        let refused = other
            .connect(client.local_addr().unwrap(), Duration::from_millis(800))
            .await;
        assert!(refused.is_err());
    }
}

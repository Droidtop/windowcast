//! GameStream's control stream: ENet on UDP 47999, every message sealed
//! with AES-128-GCM under the launch's input key (`rikey`), as
//! moonlight-common-c (`ControlStream.c`, `encryptControlMessage`) and
//! Sunshine (`stream.cpp`) do it with the Sunshine "control v2" encryption:
//!
//! `0x0001` (LE16) | length (LE16: sequence + tag + inner) | sequence
//! (LE32) | GCM tag (16) | ciphertext of (type LE16, payload length LE16,
//! payload). The 12-byte IV is the sequence number little-endian, then
//! zeros, then the sender (`C` client, `H` host) and `C` for control.
//!
//! The client says which launch it belongs to with the ENet connect data
//! the host gave it in RTSP (`X-SS-Connect-Data`), asks for keyframes
//! (0x0302) and sends input (0x0206), loss reports and a periodic ping; the
//! host ends the stream with a termination message (0x0109).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce, Tag};
use rusty_enet::{Event, Host, HostSettings, Packet};

/// The GameStream control port.
pub const CONTROL_PORT: u16 = 47999;
/// Moonlight opens this many ENet channels.
pub const CHANNEL_COUNT: usize = 0x30;
pub const CHANNEL_GENERIC: u8 = 0x00;
pub const CHANNEL_URGENT: u8 = 0x01;

/// Message types of the encrypted generation-7 control stream.
pub const REQUEST_IDR: u16 = 0x0302;
pub const START_B: u16 = 0x0307;
pub const INVALIDATE_REFS: u16 = 0x0301;
pub const LOSS_STATS: u16 = 0x0201;
pub const PERIODIC_PING: u16 = 0x0200;
pub const INPUT: u16 = 0x0206;
pub const RUMBLE: u16 = 0x010b;
pub const TERMINATION: u16 = 0x0109;
pub const HDR_MODE: u16 = 0x010e;
pub const FEC_STATUS: u16 = 0x5502;

/// The host's "graceful termination" reason.
pub const TERMINATION_GRACEFUL: u32 = 0x8003_0023;

fn nonce(seq: u32, from_client: bool) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[..4].copy_from_slice(&seq.to_le_bytes());
    iv[10] = if from_client { b'C' } else { b'H' };
    iv[11] = b'C';
    iv
}

/// Seals one control message.
pub fn seal(key: &[u8; 16], seq: u32, from_client: bool, kind: u16, payload: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(4 + payload.len());
    inner.extend_from_slice(&kind.to_le_bytes());
    inner.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    inner.extend_from_slice(payload);
    let cipher = Aes128Gcm::new(key.into());
    let tag = cipher
        .encrypt_in_place_detached(Nonce::from_slice(&nonce(seq, from_client)), &[], &mut inner)
        .expect("AES-GCM sealing does not fail");
    let mut out = Vec::with_capacity(8 + 16 + inner.len());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&((4 + 16 + inner.len()) as u16).to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&tag);
    out.extend_from_slice(&inner);
    out
}

/// Opens one control message sent by the other side; `None` for anything
/// not genuine.
pub fn open(key: &[u8; 16], from_client: bool, data: &[u8]) -> Option<(u16, Vec<u8>)> {
    if data.len() < 8 + 16 + 4 || u16::from_le_bytes([data[0], data[1]]) != 1 {
        return None;
    }
    let length = usize::from(u16::from_le_bytes([data[2], data[3]]));
    if length + 4 != data.len() {
        return None;
    }
    let seq = u32::from_le_bytes(data[4..8].try_into().ok()?);
    let tag = Tag::clone_from_slice(&data[8..24]);
    let mut inner = data[24..].to_vec();
    Aes128Gcm::new(key.into())
        .decrypt_in_place_detached(
            Nonce::from_slice(&nonce(seq, from_client)),
            &[],
            &mut inner,
            &tag,
        )
        .ok()?;
    let kind = u16::from_le_bytes([inner[0], inner[1]]);
    let len = usize::from(u16::from_le_bytes([inner[2], inner[3]]));
    let payload = inner.get(4..4 + len)?.to_vec();
    Some((kind, payload))
}

/// What happens on a control stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEvent {
    Connected,
    /// A message from the other side, opened.
    Message(u16, Vec<u8>),
    Disconnected,
}

/// What to send on a control stream.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub kind: u16,
    pub payload: Vec<u8>,
    pub channel: u8,
    pub reliable: bool,
}

/// One end of a control stream, run on its own thread (ENet hosts are not
/// shared between threads).
pub struct Control {
    pub events: Receiver<ControlEvent>,
    pub sender: ControlSender,
}

/// How long the control thread waits on its socket when nothing arrives and
/// nothing is queued: the bound on ENet's own timers (resends, pings,
/// timeouts), which C ENet's `enet_host_service` also leaves to the caller.
/// Traffic and queued messages wake it at once.
const IDLE_WAIT: Duration = Duration::from_millis(10);

/// Sends on a control stream; clones share it.
#[derive(Clone)]
pub struct ControlSender {
    queue: Sender<Outgoing>,
    waker: Arc<Waker>,
}

impl ControlSender {
    pub fn send(&self, kind: u16, payload: &[u8], channel: u8, reliable: bool) -> bool {
        let queued = self
            .queue
            .send(Outgoing {
                kind,
                payload: payload.to_vec(),
                channel,
                reliable,
            })
            .is_ok();
        if queued {
            self.waker.wake();
        }
        queued
    }
}

/// Wakes the control thread from its wait on the ENet socket: a one-byte
/// datagram to that socket from loopback. ENet drops it unread, as it drops
/// anything shorter than its own header.
struct Waker {
    socket: Option<UdpSocket>,
    target: SocketAddr,
}

impl Waker {
    fn for_socket(control: &UdpSocket) -> Self {
        let target = match control.local_addr() {
            Ok(local) => {
                let ip = match local.ip() {
                    IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
                    IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
                    ip => ip,
                };
                SocketAddr::new(ip, local.port())
            }
            Err(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        };
        let bind = if target.is_ipv6() {
            "[::1]:0"
        } else {
            "127.0.0.1:0"
        };
        // Without a waker socket the thread still wakes every IDLE_WAIT.
        let socket = UdpSocket::bind(bind).ok();
        Waker { socket, target }
    }

    fn wake(&self) {
        if let Some(socket) = &self.socket {
            let _ = socket.send_to(&[0], self.target);
        }
    }
}

/// Blocks until a datagram waits on `socket` or `timeout` passes, without
/// taking the datagram: ENet reads it next. The socket stays in ENet's
/// non-blocking mode outside this wait.
fn wait_readable(socket: &UdpSocket, timeout: Duration) {
    let waited = socket
        .set_nonblocking(false)
        .and_then(|()| socket.set_read_timeout(Some(timeout)))
        .map(|()| {
            let mut probe = [0u8; 1];
            // A datagram longer than the probe is still "something to read":
            // Windows says so with an error (WSAEMSGSIZE), which is fine here.
            let _ = socket.peek_from(&mut probe);
        });
    if socket.set_nonblocking(true).is_err() || waited.is_err() {
        // The socket cannot block: fall back to waiting the bound out.
        std::thread::sleep(timeout);
    }
}

impl Control {
    pub fn send(&self, kind: u16, payload: &[u8], channel: u8, reliable: bool) -> bool {
        self.sender.send(kind, payload, channel, reliable)
    }

    /// Host side: waits on `bind` for the client whose ENet connect data is
    /// `connect_data`.
    pub fn listen(bind: SocketAddr, key: [u8; 16], connect_data: u32) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(bind)?;
        Ok(run(socket, key, false, Role::Host { connect_data }))
    }

    /// Client side: connects to the host's control port.
    pub fn connect(host: SocketAddr, key: [u8; 16], connect_data: u32) -> std::io::Result<Self> {
        let bind: SocketAddr = if host.is_ipv6() {
            "[::]:0".parse().expect("address")
        } else {
            "0.0.0.0:0".parse().expect("address")
        };
        let socket = UdpSocket::bind(bind)?;
        Ok(run(socket, key, true, Role::Client { host, connect_data }))
    }
}

enum Role {
    Host { connect_data: u32 },
    Client { host: SocketAddr, connect_data: u32 },
}

fn run(socket: UdpSocket, key: [u8; 16], client: bool, role: Role) -> Control {
    let (events_tx, events) = mpsc::channel();
    let (outgoing, outgoing_rx) = mpsc::channel::<Outgoing>();
    let waker = Arc::new(Waker::for_socket(&socket));
    std::thread::spawn(move || {
        let Ok(mut host) = Host::new(
            socket,
            HostSettings {
                peer_limit: if client { 1 } else { 4 },
                channel_limit: CHANNEL_COUNT,
                ..Default::default()
            },
        ) else {
            let _ = events_tx.send(ControlEvent::Disconnected);
            return;
        };
        let mut peer = None;
        if let Role::Client {
            host: address,
            connect_data,
        } = role
        {
            match host.connect(address, CHANNEL_COUNT, connect_data) {
                Ok(p) => peer = Some(p.id()),
                Err(_) => {
                    let _ = events_tx.send(ControlEvent::Disconnected);
                    return;
                }
            }
        }
        let expected = match role {
            Role::Host { connect_data } => Some(connect_data),
            Role::Client { .. } => None,
        };
        let mut seq = 0u32;
        let mut connected = false;
        let started = Instant::now();
        loop {
            // Everything queued to send.
            loop {
                match outgoing_rx.try_recv() {
                    Ok(message) => {
                        if let (true, Some(id)) = (connected, peer) {
                            let sealed = seal(&key, seq, client, message.kind, &message.payload);
                            seq = seq.wrapping_add(1);
                            let packet = if message.reliable {
                                Packet::reliable(sealed.as_slice())
                            } else {
                                Packet::unreliable_unsequenced(sealed.as_slice())
                            };
                            if let Some(p) = host.get_peer_mut(id) {
                                let _ = p.send(message.channel, &packet);
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        if let Some(p) = peer.and_then(|id| host.get_peer_mut(id)) {
                            p.disconnect(0);
                        }
                        host.flush();
                        return;
                    }
                }
            }
            let event = match host.service() {
                Ok(event) => event.map(Event::no_ref),
                Err(_) => {
                    let _ = events_tx.send(ControlEvent::Disconnected);
                    return;
                }
            };
            match event {
                Some(rusty_enet::EventNoRef::Connect { peer: id, data }) => {
                    if expected.is_some_and(|want| want != data) || (connected && peer != Some(id))
                    {
                        if let Some(p) = host.get_peer_mut(id) {
                            p.disconnect_now(0);
                        }
                        continue;
                    }
                    peer = Some(id);
                    connected = true;
                    if let Some(p) = host.get_peer_mut(id) {
                        p.set_timeout(2, 10_000, 10_000);
                    }
                    let _ = events_tx.send(ControlEvent::Connected);
                }
                Some(rusty_enet::EventNoRef::Disconnect { peer: id, .. }) => {
                    if peer == Some(id) {
                        let _ = events_tx.send(ControlEvent::Disconnected);
                        return;
                    }
                }
                Some(rusty_enet::EventNoRef::Receive {
                    peer: id, packet, ..
                }) => {
                    if peer == Some(id) {
                        if let Some((kind, payload)) = open(&key, !client, packet.data()) {
                            if events_tx
                                .send(ControlEvent::Message(kind, payload))
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
                None => {
                    if !connected && client && started.elapsed() > Duration::from_secs(10) {
                        let _ = events_tx.send(ControlEvent::Disconnected);
                        return;
                    }
                    // Nothing came and nothing is queued: wait on the socket
                    // until traffic, a queued message (the waker) or ENet's
                    // timers need another look.
                    wait_readable(host.socket(), IDLE_WAIT);
                }
            }
        }
    });
    Control {
        events,
        sender: ControlSender {
            queue: outgoing,
            waker,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_messages_open_only_the_other_way_and_with_the_key() {
        let key = [7u8; 16];
        let sealed = seal(&key, 5, true, REQUEST_IDR, &[0, 0]);
        assert_eq!(open(&key, true, &sealed), Some((REQUEST_IDR, vec![0, 0])));
        // The host's direction has another IV: a reflected message fails.
        assert_eq!(open(&key, false, &sealed), None);
        assert_eq!(open(&[8u8; 16], true, &sealed), None);
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert_eq!(open(&key, true, &tampered), None);
    }

    #[test]
    fn a_queued_message_wakes_the_wait_and_the_socket_stays_non_blocking() {
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let waker = Waker::for_socket(&socket);
        assert!(waker.target.ip().is_loopback());
        let started = Instant::now();
        let wake = std::thread::spawn(move || waker.wake());
        // A long bound: only the waker's datagram ends the wait early.
        wait_readable(&socket, Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(10));
        wake.join().unwrap();
        // The wait only peeked: the datagram is still there, then nothing,
        // without blocking.
        let mut buf = [0u8; 8];
        assert_eq!(socket.recv_from(&mut buf).unwrap().0, 1);
        assert_eq!(
            socket.recv_from(&mut buf).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn a_client_and_host_talk_over_enet() {
        let key = [3u8; 16];
        let port = {
            let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let address: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let host = Control::listen(address, key, 1234).unwrap();
        let client = Control::connect(address, key, 1234).unwrap();
        let wait = |control: &Control| control.events.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(wait(&client), ControlEvent::Connected);
        assert_eq!(wait(&host), ControlEvent::Connected);
        assert!(client.send(REQUEST_IDR, &[0, 0], CHANNEL_URGENT, true));
        assert_eq!(wait(&host), ControlEvent::Message(REQUEST_IDR, vec![0, 0]));
        assert!(host.send(
            TERMINATION,
            &TERMINATION_GRACEFUL.to_be_bytes(),
            CHANNEL_GENERIC,
            true
        ));
        assert_eq!(
            wait(&client),
            ControlEvent::Message(TERMINATION, TERMINATION_GRACEFUL.to_be_bytes().to_vec())
        );
    }
}

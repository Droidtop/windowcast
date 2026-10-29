use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// The UDP port beacons are broadcast to and heard on.
pub const PORT: u16 = 47101;

/// Starts every beacon, followed by the format version.
const MAGIC: &[u8; 4] = b"WCB\x01";
/// A device id is the host's public key: 32 bytes.
const ID_LEN: usize = 32;
/// The longest display name a beacon carries, in bytes.
pub const MAX_NAME: usize = 64;
/// magic + id + port + name length + name.
const MAX_PACKET: usize = 4 + ID_LEN + 2 + 1 + MAX_NAME;
const MIN_PACKET: usize = 4 + ID_LEN + 2 + 1;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BeaconError {
    #[error("not a windowcast beacon")]
    NotABeacon,
    #[error("beacon is cut short")]
    Truncated,
    #[error("beacon is longer than a beacon can be")]
    TooLong,
    #[error("beacon's name is not text a person can read")]
    BadName,
    #[error("beacon names port 0")]
    BadPort,
}

/// What a host says about itself: who it is and where it listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beacon {
    /// The host's device id (its public key), to expect when connecting.
    pub device_id: [u8; ID_LEN],
    /// The port the host listens on for sessions; the address is the one the
    /// beacon came from.
    pub port: u16,
    /// A name for a person to read ("Living room PC"), at most [`MAX_NAME`] bytes.
    pub name: String,
}

impl Beacon {
    /// A beacon for a host. The name is cut to [`MAX_NAME`] bytes at a
    /// character boundary and stripped of control characters.
    pub fn new(device_id: [u8; ID_LEN], port: u16, name: &str) -> Self {
        let clean: String = name.chars().filter(|c| !c.is_control()).collect();
        let mut end = clean.len().min(MAX_NAME);
        while !clean.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            device_id,
            port,
            name: clean[..end].trim().to_string(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        let name = &name[..name.len().min(MAX_NAME)];
        let mut out = Vec::with_capacity(MIN_PACKET + name.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.device_id);
        out.extend_from_slice(&self.port.to_be_bytes());
        out.push(name.len() as u8);
        out.extend_from_slice(name);
        out
    }

    /// Reads a beacon, refusing anything that is not exactly one.
    pub fn decode(bytes: &[u8]) -> Result<Self, BeaconError> {
        if bytes.len() > MAX_PACKET {
            return Err(BeaconError::TooLong);
        }
        if bytes.len() < 4 || &bytes[..4] != MAGIC {
            return Err(BeaconError::NotABeacon);
        }
        if bytes.len() < MIN_PACKET {
            return Err(BeaconError::Truncated);
        }
        let mut device_id = [0u8; ID_LEN];
        device_id.copy_from_slice(&bytes[4..4 + ID_LEN]);
        let port = u16::from_be_bytes([bytes[4 + ID_LEN], bytes[5 + ID_LEN]]);
        if port == 0 {
            return Err(BeaconError::BadPort);
        }
        let len = usize::from(bytes[6 + ID_LEN]);
        let body = &bytes[MIN_PACKET..];
        if len > MAX_NAME {
            return Err(BeaconError::TooLong);
        }
        if body.len() != len {
            return Err(if body.len() < len {
                BeaconError::Truncated
            } else {
                BeaconError::TooLong
            });
        }
        let name = std::str::from_utf8(body).map_err(|_| BeaconError::BadName)?;
        if name.chars().any(char::is_control) {
            return Err(BeaconError::BadName);
        }
        Ok(Self {
            device_id,
            port,
            name: name.to_string(),
        })
    }
}

/// Broadcasts [`beacon`] once to the local network (255.255.255.255 on
/// [`PORT`]). A host calls this every few seconds while it listens.
pub fn announce(beacon: &Beacon) -> io::Result<()> {
    announce_to(beacon, SocketAddr::from((Ipv4Addr::BROADCAST, PORT)))
}

/// As [`announce`], to a chosen address (a directed broadcast, or a test).
pub fn announce_to(beacon: &Beacon, to: SocketAddr) -> io::Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    socket.send_to(&beacon.encode(), to)?;
    Ok(())
}

/// A host heard on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub beacon: Beacon,
    /// Where to connect: the sender's address with the beacon's port.
    pub address: SocketAddr,
}

/// Listens for beacons.
pub struct Listener {
    socket: UdpSocket,
}

impl Listener {
    /// Listens on [`PORT`]. Only one listener per computer can hold the port; a second `bind` fails.
    pub fn bind() -> io::Result<Self> {
        Self::bind_to(SocketAddr::from((Ipv4Addr::UNSPECIFIED, PORT)))
    }

    /// Listens on `addr` (a test uses port 0).
    pub fn bind_to(addr: SocketAddr) -> io::Result<Self> {
        Ok(Self {
            socket: UdpSocket::bind(addr)?,
        })
    }

    /// The address this listener is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Listens for up to `window` and returns each distinct host heard (by
    /// device id; the latest beacon of a host wins), in the order first
    /// heard. Packets that are not valid beacons are ignored.
    pub fn collect(&self, window: Duration) -> io::Result<Vec<Seen>> {
        let deadline = Instant::now() + window;
        let mut order: Vec<[u8; ID_LEN]> = Vec::new();
        let mut heard: HashMap<[u8; ID_LEN], Seen> = HashMap::new();
        let mut buffer = [0u8; MAX_PACKET + 16];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            self.socket.set_read_timeout(Some(left))?;
            let (n, from) = match self.socket.recv_from(&mut buffer) {
                Ok(read) => read,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    break
                }
                Err(e) => return Err(e),
            };
            let Ok(beacon) = Beacon::decode(&buffer[..n]) else {
                continue;
            };
            let address = SocketAddr::new(from.ip(), beacon.port);
            if matches!(address.ip(), IpAddr::V4(ip) if ip.is_unspecified() || ip.is_multicast()) {
                continue;
            }
            if !heard.contains_key(&beacon.device_id) {
                order.push(beacon.device_id);
            }
            heard.insert(beacon.device_id, Seen { beacon, address });
        }
        Ok(order
            .into_iter()
            .filter_map(|id| heard.remove(&id))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> [u8; ID_LEN] {
        [byte; ID_LEN]
    }

    #[test]
    fn a_beacon_round_trips() {
        let beacon = Beacon::new(id(7), 47100, "Living room PC");
        assert_eq!(Beacon::decode(&beacon.encode()), Ok(beacon));
    }

    #[test]
    fn the_name_is_cut_at_a_character_and_cleaned() {
        let long = "é".repeat(100);
        let beacon = Beacon::new(id(1), 1, &long);
        assert!(beacon.name.len() <= MAX_NAME);
        assert!(beacon.name.chars().all(|c| c == 'é'));
        assert_eq!(Beacon::decode(&beacon.encode()).unwrap().name, beacon.name);
        assert_eq!(Beacon::new(id(1), 1, "a\nb\u{7}c").name, "abc");
    }

    #[test]
    fn what_is_not_a_beacon_is_refused() {
        assert_eq!(Beacon::decode(b""), Err(BeaconError::NotABeacon));
        assert_eq!(
            Beacon::decode(b"GET / HTTP/1.1\r\n"),
            Err(BeaconError::NotABeacon)
        );
        let good = Beacon::new(id(2), 9, "x").encode();
        assert_eq!(
            Beacon::decode(&good[..good.len() - 1]),
            Err(BeaconError::Truncated)
        );
        assert_eq!(Beacon::decode(&good[..10]), Err(BeaconError::Truncated));
        let mut longer = good.clone();
        longer.push(b'y');
        assert_eq!(Beacon::decode(&longer), Err(BeaconError::TooLong));
        assert_eq!(Beacon::decode(&vec![0u8; 5000]), Err(BeaconError::TooLong));
    }

    #[test]
    fn port_zero_and_odd_names_are_refused() {
        let mut zero = Beacon::new(id(3), 1, "x").encode();
        zero[4 + ID_LEN] = 0;
        zero[5 + ID_LEN] = 0;
        assert_eq!(Beacon::decode(&zero), Err(BeaconError::BadPort));
        let mut bad = Beacon::new(id(3), 1, "xy").encode();
        let end = bad.len();
        bad[end - 1] = 0xFF;
        assert_eq!(Beacon::decode(&bad), Err(BeaconError::BadName));
        let mut control = Beacon::new(id(3), 1, "xy").encode();
        let end = control.len();
        control[end - 1] = 0x07;
        assert_eq!(Beacon::decode(&control), Err(BeaconError::BadName));
        let mut length = Beacon::new(id(3), 1, "x").encode();
        length[6 + ID_LEN] = 200;
        assert_eq!(Beacon::decode(&length), Err(BeaconError::TooLong));
    }

    #[test]
    fn a_listener_hears_hosts_once_each_and_ignores_junk() {
        let listener = Listener::bind_to("127.0.0.1:0".parse().unwrap()).unwrap();
        let to = listener.local_addr().unwrap();
        announce_to(&Beacon::new(id(1), 47100, "one"), to).unwrap();
        // Junk, then a second host, then the first again with a new name.
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.send_to(b"hello", to).unwrap();
        announce_to(&Beacon::new(id(2), 47200, "two"), to).unwrap();
        announce_to(&Beacon::new(id(1), 47100, "one renamed"), to).unwrap();
        let seen = listener.collect(Duration::from_millis(300)).unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].beacon.name, "one renamed");
        assert_eq!(seen[0].address, "127.0.0.1:47100".parse().unwrap());
        assert_eq!(seen[1].beacon.name, "two");
        assert_eq!(seen[1].address.port(), 47200);
    }

    #[test]
    fn a_quiet_network_gives_an_empty_list_after_the_window() {
        let listener = Listener::bind_to("127.0.0.1:0".parse().unwrap()).unwrap();
        let started = Instant::now();
        assert!(listener
            .collect(Duration::from_millis(80))
            .unwrap()
            .is_empty());
        assert!(started.elapsed() >= Duration::from_millis(70));
    }
}

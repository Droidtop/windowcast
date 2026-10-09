//! Finding a paired device away from the LAN, the way Syncthing does it:
//! STUN for the address a NAT gives a UDP socket, Syncthing's global
//! discovery protocol (v3) for small announcements of that address, and
//! UDP hole punching between the two devices. Only addresses travel
//! through these services; every byte of a session goes directly between
//! the devices. Syncthing's relays are never used.
//!
//! Shared by windowcast (its sessions away from home) and droidtop-agent
//! (its WireGuard tunnel), which each name their addresses with their own
//! scheme and derive their own discovery identity, so a computer running
//! both announces two devices.
//!
//! - **Discovery identity.** Global discovery names a device by the SHA-256
//!   of the TLS client certificate it announces with (Syncthing's device ID).
//!   Each device's certificate is made from a key derived from its own seed
//!   and a label naming the program, with fixed fields and a deterministic
//!   Ed25519 signature, so the same seed always gives the same certificate
//!   and ID and nothing more is kept. Two devices tell each other their IDs
//!   over a connection they already trust.
//! - **Announce** (`POST <server>` with the client certificate,
//!   `{"addresses": ["<scheme>://<ip>:<port>", ...]}`) and **query**
//!   (`GET <server>?device=<ID>`), honouring `Reannounce-After` and
//!   `Retry-After` exactly as Syncthing's own client does (30 minutes between
//!   announcements, 5 minutes after a failure).
//! - **STUN** (RFC 5389 binding requests) from the same socket the
//!   session's packets use, so the address announced is the one the NAT
//!   keeps for it.

use std::io::Read;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Syncthing's default global discovery servers (its `lib/config`
/// `DefaultDiscoveryServersV4` and `V6`): one for lookups, one per address
/// family for announcements.
pub const SYNCTHING_DISCOVERY: &[&str] = &[
    "https://discovery-lookup.syncthing.net/v2/?noannounce",
    "https://discovery-announce-v4.syncthing.net/v2/?nolookup",
    "https://discovery-announce-v6.syncthing.net/v2/?nolookup",
];

/// The STUN servers Syncthing falls back to (its `DefaultFallbackStunServers`).
pub const SYNCTHING_STUN: &[&str] = &[
    "stun.counterpath.com:3478",
    "stun.hitv.com:3478",
    "stun.internetcalls.com:3478",
    "stun.miwifi.com:3478",
    "stun.schlund.de:3478",
    "stun.sipgate.net:3478",
    "stun.voip.aebc.com:3478",
    "stun.voipbuster.com:3478",
    "stun.voipstunt.com:3478",
];

/// Syncthing's client: 30 minutes between announcements, 5 after a failure.
pub const REANNOUNCE: Duration = Duration::from_secs(30 * 60);
pub const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5 * 60);
/// Syncthing caches a lookup that found the device for 5 minutes, and one
/// that did not for 1 minute unless the server says longer.
pub const FOUND_CACHE: Duration = Duration::from_secs(5 * 60);
pub const NOT_FOUND_CACHE: Duration = Duration::from_secs(60);
/// Syncthing's STUN keepalive starts at 180 s and shrinks to 20 s when the
/// NAT forgets mappings sooner.
pub const STUN_KEEPALIVE_START: Duration = Duration::from_secs(180);
pub const STUN_KEEPALIVE_MIN: Duration = Duration::from_secs(20);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// Syncthing device IDs --------------------------------------------------------

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn base32(data: &[u8]) -> String {
    let mut out = String::new();
    let mut buffer = 0u32;
    let mut bits = 0;
    for &b in data {
        buffer = (buffer << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Syncthing's Luhn mod 32 check character (its `lib/protocol/luhn.go`).
fn luhn32(s: &str) -> char {
    let mut factor = 1;
    let mut sum = 0;
    for c in s.bytes() {
        let cp = BASE32.iter().position(|&b| b == c).unwrap_or(0) as u32;
        let addend = factor * cp;
        factor = if factor == 2 { 1 } else { 2 };
        sum += addend / 32 + addend % 32;
    }
    BASE32[((32 - sum % 32) % 32) as usize] as char
}

/// The canonical device ID of a certificate: base32 of its SHA-256, a check
/// character after each 13, in groups of 7 (`XXXXXXX-XXXXXXX-...`).
pub fn device_id(cert_der: &[u8]) -> String {
    let b32 = base32(&Sha256::digest(cert_der));
    let mut checked = String::new();
    for i in 0..4 {
        let part = &b32[i * 13..(i + 1) * 13];
        checked.push_str(part);
        checked.push(luhn32(part));
    }
    checked
        .as_bytes()
        .chunks(7)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

/// Whether [`id`] is a well-formed canonical device ID.
pub fn valid_device_id(id: &str) -> bool {
    let plain: String = id.chars().filter(|c| *c != '-').collect();
    plain.len() == 56
        && plain.bytes().all(|b| BASE32.contains(&b))
        && (0..4)
            .all(|i| luhn32(&plain[i * 14..i * 14 + 13]) == plain.as_bytes()[i * 14 + 13] as char)
}

// The discovery certificate ---------------------------------------------------

/// A device's certificate for global discovery, and its key.
pub struct DiscoveryCert {
    pub der: Vec<u8>,
    pkcs8: Vec<u8>,
}

impl DiscoveryCert {
    /// The certificate for the device whose Ed25519 seed is `seed`, as the
    /// program `label` names it: a key derived from both (never the seed
    /// itself), fixed fields and an Ed25519 signature, so it comes out the
    /// same every time. droidtop-agent's label is
    /// `droidtop-agent discovery certificate v1`, windowcast's
    /// `windowcast discovery certificate v1`.
    pub fn derive(label: &[u8], seed: &[u8; 32]) -> Result<DiscoveryCert, String> {
        let mut h = Sha256::new();
        h.update(label);
        h.update(seed);
        let seed: [u8; 32] = h.finalize().into();
        let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
        // RFC 8410 PKCS#8 v2: the private key and its public key.
        let mut pkcs8 = vec![
            0x30, 0x53, 0x02, 0x01, 0x01, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        pkcs8.extend_from_slice(&seed);
        pkcs8.extend_from_slice(&[0xa1, 0x23, 0x03, 0x21, 0x00]);
        pkcs8.extend_from_slice(signing.verifying_key().as_bytes());
        let pair = rcgen::KeyPair::try_from(pkcs8.as_slice())
            .map_err(|e| format!("the discovery key could not be made ({e})"))?;
        let mut params =
            rcgen::CertificateParams::new(Vec::<String>::new()).map_err(|e| e.to_string())?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "syncthing");
        params.serial_number = Some(rcgen::SerialNumber::from_slice(&[1]));
        params.not_before = rcgen::date_time_ymd(2026, 1, 1);
        params.not_after = rcgen::date_time_ymd(2049, 12, 31);
        let cert = params
            .self_signed(&pair)
            .map_err(|e| format!("the discovery certificate could not be made ({e})"))?;
        Ok(DiscoveryCert {
            der: cert.der().to_vec(),
            pkcs8,
        })
    }

    pub fn device_id(&self) -> String {
        device_id(&self.der)
    }
}

// The discovery client --------------------------------------------------------

/// One server of a discovery list, as Syncthing writes them: an https
/// address, with `?noannounce` or `?nolookup` to use it for one job only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub url: String,
    pub announce: bool,
    pub lookup: bool,
}

impl Server {
    pub fn parse(text: &str) -> Option<Server> {
        let text = text.trim();
        if !text.starts_with("https://") {
            return None;
        }
        let (base, query) = text.split_once('?').unwrap_or((text, ""));
        let flags: Vec<&str> = query.split('&').collect();
        Some(Server {
            url: base.to_string(),
            announce: !flags.contains(&"noannounce"),
            lookup: !flags.contains(&"nolookup"),
        })
    }

    /// A list of servers, where `default` stands for Syncthing's.
    pub fn list(texts: &[String]) -> Vec<Server> {
        texts
            .iter()
            .flat_map(|t| {
                if t.trim() == "default" {
                    SYNCTHING_DISCOVERY.iter().map(|s| s.to_string()).collect()
                } else {
                    vec![t.clone()]
                }
            })
            .filter_map(|t| Server::parse(&t))
            .collect()
    }
}

/// What the servers answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Announced; announce again after this long.
    Announced(Duration),
    /// The device's addresses.
    Found(Vec<String>),
    /// Not known to the server, or refused; ask again no sooner than this.
    Wait(Duration),
}

#[derive(Serialize, Deserialize, Default)]
struct Addresses {
    #[serde(default)]
    addresses: Vec<String>,
}

fn agent(client_cert: Option<&DiscoveryCert>) -> Result<ureq::Agent, String> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots);
    let config = match client_cert {
        Some(c) => builder
            .with_client_auth_cert(
                vec![rustls::pki_types::CertificateDer::from(c.der.clone())],
                rustls::pki_types::PrivateKeyDer::Pkcs8(c.pkcs8.clone().into()),
            )
            .map_err(|e| e.to_string())?,
        None => builder.with_no_client_auth(),
    };
    Ok(ureq::AgentBuilder::new()
        .tls_config(Arc::new(config))
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("windowcast-rendezvous/", env!("CARGO_PKG_VERSION")))
        .build())
}

fn seconds(header: Option<&str>) -> Option<Duration> {
    header
        .and_then(|h| h.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
}

/// Announces [`addresses`] for [`cert`]'s device to every announce server
/// in [`servers`]. The answer is when to announce next: the shortest
/// `Reannounce-After`, or after a failure its `Retry-After` or 5 minutes.
pub fn announce(servers: &[Server], cert: &DiscoveryCert, addresses: &[String]) -> Answer {
    let Ok(agent) = agent(Some(cert)) else {
        return Answer::Wait(RETRY_AFTER_FAILURE);
    };
    let body = Addresses {
        addresses: addresses.to_vec(),
    };
    let mut next: Option<Duration> = None;
    let mut any = false;
    for server in servers.iter().filter(|s| s.announce) {
        let after = match agent
            .post(&server.url)
            .set("Content-Type", "application/json")
            .send_json(&body)
        {
            Ok(r) => {
                any = true;
                seconds(r.header("Reannounce-After")).unwrap_or(REANNOUNCE)
            }
            Err(ureq::Error::Status(_, r)) => {
                seconds(r.header("Retry-After")).unwrap_or(RETRY_AFTER_FAILURE)
            }
            Err(_) => RETRY_AFTER_FAILURE,
        };
        next = Some(next.map_or(after, |n| n.min(after)));
    }
    match (any, next) {
        (true, Some(n)) => Answer::Announced(n),
        (_, n) => Answer::Wait(n.unwrap_or(RETRY_AFTER_FAILURE)),
    }
}

/// Asks the lookup servers for [`device`]'s addresses.
pub fn lookup(servers: &[Server], device: &str) -> Answer {
    if !valid_device_id(device) {
        return Answer::Wait(NOT_FOUND_CACHE);
    }
    let Ok(agent) = agent(None) else {
        return Answer::Wait(NOT_FOUND_CACHE);
    };
    let mut wait = NOT_FOUND_CACHE;
    for server in servers.iter().filter(|s| s.lookup) {
        match agent.get(&server.url).query("device", device).call() {
            Ok(r) => {
                let mut body = String::new();
                if r.into_reader()
                    .take(64 * 1024)
                    .read_to_string(&mut body)
                    .is_ok()
                {
                    if let Ok(found) = serde_json::from_str::<Addresses>(&body) {
                        if !found.addresses.is_empty() {
                            return Answer::Found(found.addresses);
                        }
                    }
                }
            }
            Err(ureq::Error::Status(_, r)) => {
                wait = wait.max(seconds(r.header("Retry-After")).unwrap_or(NOT_FOUND_CACHE))
            }
            Err(_) => {}
        }
    }
    Answer::Wait(wait)
}

/// The endpoints among announced addresses that use `scheme` (`wg://`,
/// `windowcast://`).
pub fn endpoints(addresses: &[String], scheme: &str) -> Vec<SocketAddr> {
    addresses
        .iter()
        .filter_map(|a| a.strip_prefix(scheme))
        .filter_map(|a| a.parse().ok())
        .collect()
}

// STUN --------------------------------------------------------------------------

const COOKIE: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];

/// A STUN binding request with a new transaction id.
pub fn binding_request() -> ([u8; 20], [u8; 12]) {
    let mut tx = [0u8; 12];
    OsRng.fill_bytes(&mut tx);
    let mut msg = [0u8; 20];
    msg[1] = 0x01;
    msg[4..8].copy_from_slice(&COOKIE);
    msg[8..].copy_from_slice(&tx);
    (msg, tx)
}

/// Whether [`data`] looks like a STUN message (and not WireGuard's).
pub fn is_stun(data: &[u8]) -> bool {
    // WireGuard's messages start with a type and three zero bytes; every
    // STUN binding message has a non-zero second byte.
    data.len() >= 20 && data[0] & 0xc0 == 0 && data[1..4] != [0, 0, 0] && data[4..8] == COOKIE
}

/// The mapped address in a binding success response to [`tx`].
pub fn mapped_address(data: &[u8], tx: &[u8; 12]) -> Option<SocketAddr> {
    if !is_stun(data) || data[0..2] != [0x01, 0x01] || &data[8..20] != tx {
        return None;
    }
    let len = u16::from_be_bytes([data[2], data[3]]) as usize;
    let body = data.get(20..20 + len)?;
    let mut at = 0;
    let mut plain = None;
    while at + 4 <= body.len() {
        let kind = u16::from_be_bytes([body[at], body[at + 1]]);
        let size = u16::from_be_bytes([body[at + 2], body[at + 3]]) as usize;
        let value = body.get(at + 4..at + 4 + size)?;
        let xor = kind == 0x0020;
        if (xor || kind == 0x0001) && value.len() >= 8 && value[1] == 0x01 {
            let mut port = u16::from_be_bytes([value[2], value[3]]);
            let mut ip = [value[4], value[5], value[6], value[7]];
            if xor {
                port ^= 0x2112;
                for (b, c) in ip.iter_mut().zip(COOKIE) {
                    *b ^= c;
                }
                return Some(SocketAddr::from((ip, port)));
            }
            plain = Some(SocketAddr::from((ip, port)));
        }
        at += 4 + size.div_ceil(4) * 4;
    }
    plain
}

/// Resolves STUN servers (`host:port`), IPv4 only: an IPv6 address needs no
/// NAT mapping to be found. Name lookups block, so this runs off any loop.
pub fn resolve_stun(servers: &[String]) -> Vec<SocketAddr> {
    let list: Vec<String> = servers
        .iter()
        .flat_map(|s| {
            if s.trim() == "default" {
                SYNCTHING_STUN.iter().map(|x| x.to_string()).collect()
            } else {
                vec![s.clone()]
            }
        })
        .collect();
    list.iter()
        .filter_map(|s| s.to_socket_addrs().ok())
        .flat_map(|a| a.filter(SocketAddr::is_ipv4).take(1))
        .collect()
}

/// Asks [`servers`] in turn, from [`udp`], which address the NAT gives it.
/// For a socket nothing else reads yet (the handheld's, before its tunnel).
pub fn query_stun(udp: &UdpSocket, servers: &[SocketAddr], each: Duration) -> Option<SocketAddr> {
    let v6 = udp.local_addr().ok()?.is_ipv6();
    let old = udp.read_timeout().ok().flatten();
    let _ = udp.set_read_timeout(Some(Duration::from_millis(200)));
    let mut buf = [0u8; 576];
    let mut found = None;
    'servers: for server in servers.iter().take(4) {
        let (msg, tx) = binding_request();
        let target = if v6 { to_v6(*server) } else { *server };
        if udp.send_to(&msg, target).is_err() {
            continue;
        }
        let until = Instant::now() + each;
        while Instant::now() < until {
            if let Ok((n, _)) = udp.recv_from(&mut buf) {
                if let Some(addr) = mapped_address(&buf[..n], &tx) {
                    found = Some(addr);
                    break 'servers;
                }
            }
        }
    }
    let _ = udp.set_read_timeout(old);
    found
}

/// An IPv4 address as a dual-stack socket sends to it.
pub fn to_v6(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V4(a) => SocketAddr::new(a.ip().to_ipv6_mapped().into(), a.port()),
        v6 => v6,
    }
}

/// A STUN keepalive for a socket another loop reads (droidtop-agent's
/// WireGuard socket, a windowcast host's rendezvous socket): `tick` sends
/// when due, `heard` takes replies.
pub struct StunKeeper {
    pub servers: Vec<SocketAddr>,
    next: usize,
    tx: Option<[u8; 12]>,
    sent: Option<Instant>,
    interval: Duration,
    pub mapped: Option<SocketAddr>,
}

impl StunKeeper {
    pub fn new(servers: Vec<SocketAddr>) -> StunKeeper {
        StunKeeper {
            servers,
            next: 0,
            tx: None,
            sent: None,
            interval: STUN_KEEPALIVE_START,
            mapped: None,
        }
    }

    /// Sends a binding request when one is due: at once, then every keepalive.
    pub fn tick(&mut self, udp: &UdpSocket) {
        if let Some((msg, server)) = self.due() {
            let v6 = udp.local_addr().map(|a| a.is_ipv6()).unwrap_or(false);
            let _ = udp.send_to(&msg, if v6 { to_v6(server) } else { server });
        }
    }

    /// The binding request to send now and where, when one is due: at
    /// once, then every keepalive. For a caller that sends it itself (an
    /// asynchronous socket).
    pub fn due(&mut self) -> Option<([u8; 20], SocketAddr)> {
        if self.servers.is_empty() {
            return None;
        }
        // An unanswered request is retried at the next server after 5 s.
        let due = match (self.sent, self.tx) {
            (None, _) => true,
            (Some(t), Some(_)) => t.elapsed() > Duration::from_secs(5),
            (Some(t), None) => t.elapsed() > self.interval,
        };
        if !due {
            return None;
        }
        if self.tx.is_some() {
            self.next = (self.next + 1) % self.servers.len();
        }
        let (msg, tx) = binding_request();
        self.tx = Some(tx);
        self.sent = Some(Instant::now());
        Some((msg, self.servers[self.next]))
    }

    /// Takes a STUN reply; true when it was one.
    pub fn heard(&mut self, data: &[u8]) -> bool {
        if !is_stun(data) {
            return false;
        }
        if let Some(tx) = self.tx {
            if let Some(addr) = mapped_address(data, &tx) {
                if self.mapped.is_some_and(|m| m != addr) {
                    // The NAT dropped the mapping before the keepalive: keep it more often.
                    self.interval = (self.interval / 2).max(STUN_KEEPALIVE_MIN);
                }
                self.mapped = Some(addr);
                self.tx = None;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_follow_syncthings_format() {
        let id = device_id(b"any certificate bytes");
        assert_eq!(id.len(), 63);
        assert_eq!(id.matches('-').count(), 7);
        assert!(valid_device_id(&id));
        let mut broken = id.into_bytes();
        broken[0] = if broken[0] == b'A' { b'B' } else { b'A' };
        assert!(!valid_device_id(&String::from_utf8(broken).unwrap()));
        // The example in Syncthing's documentation (dev/device-ids) carries valid check characters.
        assert!(valid_device_id(
            "MFZWI3D-BONSGYC-YLTMRWG-C43ENR5-QXGZDMM-FZWI3DP-BONSGYY-LTMRWAD"
        ));
        assert!(!valid_device_id(
            "MFZWI3D-BONSGYC-YLTMRWG-C43ENR5-QXGZDMM-FZWI3DP-BONSGYY-LTMRWAE"
        ));
    }

    #[test]
    fn the_same_seed_gives_the_same_certificate() {
        let label = b"test discovery certificate v1";
        let seed = [7u8; 32];
        let a = DiscoveryCert::derive(label, &seed).unwrap();
        let b = DiscoveryCert::derive(label, &seed).unwrap();
        assert_eq!(a.der, b.der);
        assert_eq!(a.device_id(), b.device_id());
        assert_ne!(
            a.device_id(),
            DiscoveryCert::derive(label, &[8u8; 32])
                .unwrap()
                .device_id()
        );
        // Another program's label gives the same device another ID.
        assert_ne!(
            a.device_id(),
            DiscoveryCert::derive(b"other discovery certificate v1", &seed)
                .unwrap()
                .device_id()
        );
    }

    #[test]
    fn server_lists_read_like_syncthings() {
        let list = Server::list(&["default".to_string()]);
        assert_eq!(list.len(), 3);
        assert!(list[0].lookup && !list[0].announce);
        assert!(list[1].announce && !list[1].lookup);
        assert_eq!(
            list[1].url,
            "https://discovery-announce-v4.syncthing.net/v2/"
        );
        assert!(Server::parse("http://plain.example/v2/").is_none());
        assert_eq!(
            endpoints(
                &[
                    "wg://203.0.113.7:47611".into(),
                    "tcp://203.0.113.7:22000".into()
                ],
                "wg://"
            ),
            vec![SocketAddr::from(([203, 0, 113, 7], 47611))]
        );
    }

    #[test]
    fn stun_replies_give_the_mapped_address() {
        let (req, tx) = binding_request();
        assert!(is_stun(&req));
        // A binding success with XOR-MAPPED-ADDRESS 198.51.100.9:40000.
        let port = 40000u16 ^ 0x2112;
        let ip = [198u8 ^ 0x21, 51 ^ 0x12, 100 ^ 0xa4, 9 ^ 0x42];
        let mut reply = vec![0x01, 0x01, 0x00, 0x0c];
        reply.extend_from_slice(&COOKIE);
        reply.extend_from_slice(&tx);
        reply.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
        reply.extend_from_slice(&port.to_be_bytes());
        reply.extend_from_slice(&ip);
        assert_eq!(
            mapped_address(&reply, &tx),
            Some(SocketAddr::from(([198, 51, 100, 9], 40000)))
        );
        assert_eq!(mapped_address(&reply, &[0u8; 12]), None);
        // A WireGuard message (reserved bytes zero) is never taken for STUN.
        assert!(!is_stun(&[
            1, 0, 0, 0, 0x21, 0x12, 0xa4, 0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
        ]));
    }

    #[test]
    fn a_stun_server_on_loopback_answers_a_query() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let at = server.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            let (n, from) = server.recv_from(&mut buf).unwrap();
            assert_eq!(n, 20);
            let mut reply = vec![0x01, 0x01, 0x00, 0x0c];
            reply.extend_from_slice(&buf[4..20]);
            let SocketAddr::V4(f) = from else { panic!() };
            reply.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
            reply.extend_from_slice(&(f.port() ^ 0x2112).to_be_bytes());
            for (b, c) in f.ip().octets().iter().zip(COOKIE) {
                reply.push(b ^ c);
            }
            server.send_to(&reply, from).unwrap();
        });
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mine = udp.local_addr().unwrap();
        assert_eq!(query_stun(&udp, &[at], Duration::from_secs(3)), Some(mine));
    }
}

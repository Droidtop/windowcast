//! Reaching a paired peer away from the LAN (docs/SECURITY.md, "Away from
//! the LAN"): each side learns the address its NAT gives its rendezvous
//! socket from STUN, announces it under its discovery ID in Syncthing's
//! global discovery (`windowcast-rendezvous`, shared with droidtop-agent),
//! looks the other up, and both send towards each other until the NATs let
//! a [`punched`](crate::punched) stream through. Signaling runs over that
//! stream; the session then crosses the NATs with ICE ([`Session::away`]).
//! Only addresses go to the discovery and STUN servers; no relay is ever
//! used.
//!
//! The two sides learn each other's discovery IDs over a session they
//! already trust (`ControlMessage::Rendezvous`, on the LAN first) and keep
//! them in [`RemotePeers`].

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windowcast_identity::{Identity, PeerId};
use windowcast_rendezvous as rendezvous;

pub use rendezvous::Answer;

/// The scheme of a windowcast rendezvous address in an announcement.
pub const SCHEME: &str = "windowcast://";

/// windowcast's discovery certificate label: the same identity used by
/// droidtop-agent (which uses its own label) is another device to
/// discovery, so the two never overwrite each other's addresses.
pub const LABEL: &[u8] = b"windowcast discovery certificate v1";

/// The UDP port a host's rendezvous socket uses by default (the TCP
/// signaling port's neighbour).
pub const DEFAULT_PORT: u16 = 47101;

/// How long a client keeps trying to reach a host it found: long enough
/// for the host's next lookup of the client (a minute when it had not seen
/// it before) and its punches.
pub const CONNECT_WITHIN: Duration = Duration::from_secs(90);

/// How often each side punches towards an address it found, and for how
/// long after finding it.
pub const PUNCH_EVERY: Duration = Duration::from_secs(2);
pub const PUNCH_FOR: Duration = rendezvous::FOUND_CACHE;

/// Where addresses are announced and looked up. Syncthing's global
/// discovery in use; tests give their own.
pub trait Directory: Send + Sync {
    /// This device's discovery ID.
    fn device_id(&self) -> String;
    /// Announces this device's addresses. Blocks.
    fn announce(&self, addresses: &[String]) -> Answer;
    /// A device's addresses. Blocks.
    fn lookup(&self, device: &str) -> Answer;
}

/// Syncthing's global discovery, as its own client uses it.
pub struct Syncthing {
    servers: Vec<rendezvous::Server>,
    cert: rendezvous::DiscoveryCert,
}

impl Syncthing {
    /// `servers` as Syncthing writes them (`default` for Syncthing's own).
    pub fn new(identity: &Identity, servers: &[String]) -> Result<Self, String> {
        Ok(Syncthing {
            servers: rendezvous::Server::list(servers),
            cert: certificate(identity)?,
        })
    }
}

fn certificate(identity: &Identity) -> Result<rendezvous::DiscoveryCert, String> {
    let pair = identity.to_keypair_bytes();
    let seed: [u8; 32] = pair[..32].try_into().expect("a 32-byte seed");
    rendezvous::DiscoveryCert::derive(LABEL, &seed)
}

/// This identity's discovery ID, as windowcast announces it.
pub fn discovery_id(identity: &Identity) -> Result<String, String> {
    certificate(identity).map(|c| c.device_id())
}

impl Directory for Syncthing {
    fn device_id(&self) -> String {
        self.cert.device_id()
    }

    fn announce(&self, addresses: &[String]) -> Answer {
        rendezvous::announce(&self.servers, &self.cert, addresses)
    }

    fn lookup(&self, device: &str) -> Answer {
        rendezvous::lookup(&self.servers, device)
    }
}

/// The rendezvous settings both sides share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfig {
    /// Discovery servers, as Syncthing writes them; `default` is its own.
    pub servers: Vec<String>,
    /// STUN servers (`host:port`); `default` is Syncthing's list.
    pub stun: Vec<String>,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        RemoteConfig {
            servers: vec!["default".into()],
            stun: vec!["default".into()],
        }
    }
}

impl RemoteConfig {
    /// The STUN servers as `host:port`, `default` expanded.
    pub fn stun_names(&self) -> Vec<String> {
        self.stun
            .iter()
            .flat_map(|s| {
                if s.trim() == "default" {
                    rendezvous::SYNCTHING_STUN
                        .iter()
                        .map(|x| x.to_string())
                        .collect()
                } else {
                    vec![s.clone()]
                }
            })
            .collect()
    }

    /// The STUN servers resolved (blocks on name lookups).
    pub fn stun_addresses(&self) -> Vec<SocketAddr> {
        rendezvous::resolve_stun(&self.stun)
    }
}

/// The peers' discovery IDs this side learned, by identity (hex), in a
/// small JSON file beside the trust store.
pub struct RemotePeers {
    path: PathBuf,
    ids: Mutex<BTreeMap<String, String>>,
}

impl RemotePeers {
    pub fn load(path: &Path) -> Self {
        let ids = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        RemotePeers {
            path: path.to_owned(),
            ids: Mutex::new(ids),
        }
    }

    pub fn get(&self, peer: &PeerId) -> Option<String> {
        self.ids.lock().expect("ids").get(&peer.to_hex()).cloned()
    }

    /// Every peer with a discovery ID.
    pub fn all(&self) -> Vec<(String, String)> {
        let ids = self.ids.lock().expect("ids");
        ids.iter().map(|(p, d)| (p.clone(), d.clone())).collect()
    }

    /// Keeps `peer`'s discovery ID (a well-formed one only) and saves.
    pub fn set(&self, peer: &PeerId, id: &str) -> std::io::Result<()> {
        if !rendezvous::valid_device_id(id) {
            return Ok(());
        }
        let mut ids = self.ids.lock().expect("ids");
        if ids.get(&peer.to_hex()).map(String::as_str) == Some(id) {
            return Ok(());
        }
        ids.insert(peer.to_hex(), id.to_owned());
        let text = serde_json::to_string_pretty(&*ids).map_err(std::io::Error::other)?;
        std::fs::write(&self.path, text)
    }
}

/// Asks STUN servers which address the NAT gives the rendezvous socket,
/// reading the replies from its other datagrams.
pub async fn mapped_address(
    rendezvous: &crate::punched::Rendezvous,
    servers: &[SocketAddr],
    within: Duration,
) -> Option<SocketAddr> {
    let deadline = Instant::now() + within;
    for server in servers.iter().take(4) {
        let (msg, tx) = rendezvous::binding_request();
        if rendezvous.socket().send_to(&msg, server).await.is_err() {
            continue;
        }
        let each = Instant::now() + Duration::from_millis(700);
        while Instant::now() < each.min(deadline) {
            let wait = each.min(deadline).saturating_duration_since(Instant::now());
            match tokio::time::timeout(wait, rendezvous.next_datagram()).await {
                Ok(Some((data, _))) => {
                    if let Some(addr) = rendezvous::mapped_address(&data, &tx) {
                        return Some(addr);
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
    }
    None
}

/// The addresses to announce for a rendezvous socket: the NAT's mapping
/// of it.
pub fn announced(mapped: Option<SocketAddr>) -> Vec<String> {
    mapped.into_iter().map(|a| format!("{SCHEME}{a}")).collect()
}

/// The rendezvous endpoints among a peer's announced addresses.
pub fn endpoints(addresses: &[String]) -> Vec<SocketAddr> {
    rendezvous::endpoints(addresses, SCHEME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_keep_their_discovery_ids_across_loads() {
        let dir = std::env::temp_dir().join(format!("windowcast-remote-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("remote-peers.json");
        let peer = Identity::generate().peer_id();
        let id = discovery_id(&Identity::generate()).unwrap();
        let peers = RemotePeers::load(&path);
        peers.set(&peer, &id).unwrap();
        peers.set(&peer, "not an id").unwrap();
        assert_eq!(RemotePeers::load(&path).get(&peer), Some(id));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_discovery_id_is_windowcasts_own() {
        let identity = Identity::generate();
        let id = discovery_id(&identity).unwrap();
        assert!(rendezvous::valid_device_id(&id));
        assert_eq!(discovery_id(&identity).unwrap(), id);
        assert_eq!(
            endpoints(&[
                format!("{SCHEME}203.0.113.9:47101"),
                "wg://203.0.113.9:47611".into()
            ]),
            vec![SocketAddr::from(([203, 0, 113, 9], 47101))]
        );
    }
}

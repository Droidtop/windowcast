//! A host reachable away from the LAN (`windowcast_transport::remote`):
//! one UDP rendezvous socket that keeps its NAT mapping with STUN,
//! announces it to discovery, looks up the clients it trusts and punches
//! towards each one found, and takes punched signaling streams from
//! clients it already paired with. Sessions then cross the NATs with ICE.
//! Only addresses go to the discovery and STUN servers; nothing is relayed.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use windowcast_identity::PeerId;
use windowcast_rendezvous::{
    Answer, StunKeeper, FOUND_CACHE, NOT_FOUND_CACHE, RETRY_AFTER_FAILURE,
};
use windowcast_transport::punched::Rendezvous;
use windowcast_transport::remote::{self, Directory, RemoteConfig, PUNCH_EVERY};
use windowcast_transport::{Session, TransportError};

use crate::{Host, HostControl, WindowSource};

/// How a host is reached from away.
pub struct RemoteAccess {
    /// The rendezvous socket's UDP port ([`remote::DEFAULT_PORT`]).
    pub port: u16,
    pub config: RemoteConfig,
    pub directory: Arc<dyn Directory>,
}

/// What the host's rendezvous knows, for people watching it.
#[derive(Debug, Clone, Default)]
pub struct RemoteStatus {
    /// The address the NAT gives the rendezvous socket.
    pub mapped: Option<SocketAddr>,
    /// When it was last announced, and what the servers said.
    pub announced: Option<String>,
    /// Clients found, by identity.
    pub found: Vec<(String, Vec<SocketAddr>)>,
}

/// A change of mapping waits this long before it is announced, so a NAT
/// that is still settling is not announced twice.
const SETTLE: Duration = Duration::from_secs(5);

/// Runs the host's rendezvous until the socket fails.
pub async fn serve_remote(
    control: Arc<HostControl>,
    source: Arc<dyn WindowSource>,
    access: RemoteAccess,
    status: Arc<Mutex<RemoteStatus>>,
) -> std::io::Result<()> {
    let socket = UdpSocket::bind(("0.0.0.0", access.port)).await?;
    let rendezvous = Rendezvous::new(socket, true);
    println!(
        "reachable away from the LAN on UDP {} as {}",
        access.port,
        access.directory.device_id()
    );
    let host = Arc::new(Host {
        control: Arc::clone(&control),
        source,
    });
    let stun_names = access.config.stun_names();
    let config = access.config.clone();
    let stun = tokio::task::spawn_blocking(move || config.stun_addresses())
        .await
        .unwrap_or_default();
    let keeper = Arc::new(Mutex::new(StunKeeper::new(stun)));
    tokio::spawn(keep_mapping(
        Arc::clone(&rendezvous),
        Arc::clone(&keeper),
        Arc::clone(&status),
    ));
    tokio::spawn(announce_and_punch(
        Arc::clone(&rendezvous),
        Arc::clone(&control),
        Arc::clone(&access.directory),
        Arc::clone(&status),
    ));
    while let Some((stream, from)) = rendezvous.accept().await {
        let host = Arc::clone(&host);
        let stun_names = stun_names.clone();
        tokio::spawn(async move {
            let address = format!("{from} (away)");
            let result = match Session::away(&stun_names).await {
                Ok(session) => {
                    host.serve_session(stream, address.clone(), session, false)
                        .await
                }
                Err(e) => Err(e),
            };
            match result {
                Ok(()) | Err(TransportError::Closed) => println!("{address}: session ended"),
                Err(e) => eprintln!("{address}: {e}"),
            }
        });
    }
    Ok(())
}

/// STUN keepalives from the rendezvous socket, and the replies.
async fn keep_mapping(
    rendezvous: Arc<Rendezvous>,
    keeper: Arc<Mutex<StunKeeper>>,
    status: Arc<Mutex<RemoteStatus>>,
) {
    let replies = {
        let rendezvous = Arc::clone(&rendezvous);
        let keeper = Arc::clone(&keeper);
        let status = Arc::clone(&status);
        tokio::spawn(async move {
            while let Some((data, _)) = rendezvous.next_datagram().await {
                let mut keeper = keeper.lock().expect("keeper");
                if keeper.heard(&data) {
                    status.lock().expect("status").mapped = keeper.mapped;
                }
            }
        })
    };
    loop {
        let due = keeper.lock().expect("keeper").due();
        if let Some((msg, server)) = due {
            let _ = rendezvous.socket().send_to(&msg, server).await;
        }
        if replies.is_finished() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Announces the mapping when it changes (and when the servers ask), looks
/// the trusted clients up on Syncthing's schedule, and punches towards each
/// one found while that address is fresh.
async fn announce_and_punch(
    rendezvous: Arc<Rendezvous>,
    control: Arc<HostControl>,
    directory: Arc<dyn Directory>,
    status: Arc<Mutex<RemoteStatus>>,
) {
    let mut announced: Option<SocketAddr> = None;
    // A new mapping and when it was first seen.
    let mut candidate: Option<(SocketAddr, Instant)> = None;
    let mut announce_after = Instant::now();
    let mut retry_after = Instant::now();
    let mut lookup_after: HashMap<String, Instant> = HashMap::new();
    let mut punch: HashMap<SocketAddr, Instant> = HashMap::new();
    let mut found: HashMap<String, Vec<SocketAddr>> = HashMap::new();
    loop {
        let now = Instant::now();
        // Announce a new mapping once it has settled, and again when due
        // (never before a server's Retry-After).
        let mapped = status.lock().expect("status").mapped;
        if let Some(mapped) = mapped {
            let due = if Some(mapped) == announced {
                now >= announce_after
            } else {
                if candidate.is_none_or(|(at, _)| at != mapped) {
                    candidate = Some((mapped, now));
                }
                candidate.is_some_and(|(_, since)| now.duration_since(since) >= SETTLE)
                    && now >= retry_after
            };
            if due {
                let directory = Arc::clone(&directory);
                let addresses = remote::announced(Some(mapped));
                let answer = tokio::task::spawn_blocking(move || directory.announce(&addresses))
                    .await
                    .unwrap_or(Answer::Wait(RETRY_AFTER_FAILURE));
                let said = match answer {
                    Answer::Announced(after) => {
                        announced = Some(mapped);
                        announce_after = Instant::now() + after;
                        format!("{mapped} announced")
                    }
                    Answer::Wait(after) => {
                        retry_after = Instant::now() + after;
                        announce_after = retry_after;
                        format!("{mapped} not announced: the servers said wait")
                    }
                    Answer::Found(_) => format!("{mapped} not announced"),
                };
                status.lock().expect("status").announced = Some(said);
            }
        }

        // Look up the trusted clients that told us their discovery IDs.
        let trusted: Vec<PeerId> = control.admitted().await.peers().copied().collect();
        for (peer_hex, id) in control.remote_peers.all() {
            if !trusted.iter().any(|p| p.to_hex() == peer_hex) {
                continue;
            }
            if lookup_after.get(&id).is_some_and(|at| now < *at) {
                continue;
            }
            let directory = Arc::clone(&directory);
            let device = id.clone();
            let answer = tokio::task::spawn_blocking(move || directory.lookup(&device))
                .await
                .unwrap_or(Answer::Wait(NOT_FOUND_CACHE));
            match answer {
                Answer::Found(addresses) => {
                    let endpoints = remote::endpoints(&addresses);
                    let until = Instant::now() + remote::PUNCH_FOR;
                    for endpoint in &endpoints {
                        punch.insert(*endpoint, until);
                    }
                    found.insert(peer_hex.clone(), endpoints);
                    lookup_after.insert(id, Instant::now() + FOUND_CACHE);
                }
                Answer::Wait(after) | Answer::Announced(after) => {
                    lookup_after.insert(id, Instant::now() + after.max(NOT_FOUND_CACHE));
                }
            }
        }
        status.lock().expect("status").found =
            found.iter().map(|(p, a)| (p.clone(), a.clone())).collect();

        // Punch towards every fresh address.
        punch.retain(|_, until| Instant::now() < *until);
        for to in punch.keys() {
            rendezvous.punch(*to).await;
        }
        tokio::time::sleep(PUNCH_EVERY).await;
    }
}

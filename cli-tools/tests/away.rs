//! A session away from the LAN, across two NATs: the host and the client
//! each sit on a private network behind a Linux router that masquerades
//! (network namespaces wired with veth pairs), with only the routers' own
//! addresses on the shared "internet" segment, where a STUN server
//! answers. Discovery is a board in memory both sides share. The client,
//! paired earlier, finds the host through it, both punch, signaling runs
//! over the punched UDP stream, ICE crosses both NATs, and test pattern
//! frames arrive. Nothing could have gone through a relay: there is none.
//!
//! Needs root (it makes namespaces, links and NAT rules) and runs only with
//! WINDOWCAST_TEST_NAT set, as CI runs it.
#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::{TestPatternSource, WINDOW};
use windowcast_client::{Client, FramePoll};
use windowcast_host::remote::{serve_remote, RemoteAccess, RemoteStatus};
use windowcast_host::{HostConfig, HostControl};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::VideoCodec;
use windowcast_transport::remote::{discovery_id, Answer, Directory, RemoteConfig, RemotePeers};

const STUN: &str = "203.0.113.1:3478";
const NAMESPACES: [&str; 5] = ["wc-wan", "wc-hr", "wc-cr", "wc-h", "wc-c"];

fn sh(line: &str) {
    let status = Command::new("sh").arg("-c").arg(line).status().expect("sh");
    assert!(status.success(), "failed: {line}");
}

/// The two private networks, their routers and the segment between. The
/// routers drop what comes in unasked, as home routers do; a router that
/// took it would remember the stranger's packets and map the host's own
/// punches to another port.
fn build_networks() {
    for ns in NAMESPACES {
        let _ = Command::new("ip").args(["netns", "del", ns]).status();
        sh(&format!("ip netns add {ns} && ip -n {ns} link set lo up"));
    }
    sh("ip -n wc-wan link add br0 type bridge && ip -n wc-wan addr add 203.0.113.1/24 dev br0 && ip -n wc-wan link set br0 up");
    for (router, wan_ip, lan_ip, inside, inside_ip, net) in [
        (
            "wc-hr",
            "203.0.113.2",
            "192.168.10.1",
            "wc-h",
            "192.168.10.2",
            "h",
        ),
        (
            "wc-cr",
            "203.0.113.3",
            "192.168.20.1",
            "wc-c",
            "192.168.20.2",
            "c",
        ),
    ] {
        sh(&format!(
            "ip link add {net}r-wan netns {router} type veth peer name wan-{net} netns wc-wan && \
             ip -n wc-wan link set wan-{net} master br0 up && \
             ip -n {router} addr add {wan_ip}/24 dev {net}r-wan && ip -n {router} link set {net}r-wan up && \
             ip link add {net}r-lan netns {router} type veth peer name {net}-lan netns {inside} && \
             ip -n {router} addr add {lan_ip}/24 dev {net}r-lan && ip -n {router} link set {net}r-lan up && \
             ip -n {inside} addr add {inside_ip}/24 dev {net}-lan && ip -n {inside} link set {net}-lan up && \
             ip -n {inside} route add default via {lan_ip} && \
             ip netns exec {router} sysctl -qw net.ipv4.ip_forward=1 && \
             ip netns exec {router} iptables -t nat -A POSTROUTING -o {net}r-wan -j MASQUERADE &&              ip netns exec {router} iptables -A INPUT -i {net}r-wan -m conntrack --ctstate NEW -j DROP &&              ip netns exec {router} iptables -A FORWARD -i {net}r-wan -m conntrack --ctstate NEW -j DROP"
        ));
    }
}

fn remove_networks() {
    for ns in NAMESPACES {
        let _ = Command::new("ip").args(["netns", "del", ns]).status();
    }
}

/// Moves the calling thread into a namespace; threads it starts follow.
fn enter(ns: &str) {
    let file = std::fs::File::open(format!("/var/run/netns/{ns}")).unwrap();
    // SAFETY: setns on a namespace file descriptor this thread owns.
    let done = unsafe { libc::setns(file.as_raw_fd(), libc::CLONE_NEWNET) };
    assert_eq!(done, 0, "setns {ns}");
}

/// A STUN server: answers binding requests with the address they came from.
fn stun_server() {
    enter("wc-wan");
    let socket = UdpSocket::bind(STUN).unwrap();
    let mut buf = [0u8; 576];
    while let Ok((n, from)) = socket.recv_from(&mut buf) {
        if n < 20 {
            continue;
        }
        let SocketAddr::V4(from4) = from else {
            continue;
        };
        let mut reply = vec![0x01, 0x01, 0x00, 0x0c];
        reply.extend_from_slice(&buf[4..20]);
        reply.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
        reply.extend_from_slice(&(from4.port() ^ 0x2112).to_be_bytes());
        for (b, c) in from4.ip().octets().iter().zip([0x21, 0x12, 0xa4, 0x42]) {
            reply.push(b ^ c);
        }
        let _ = socket.send_to(&reply, from);
    }
}

/// Discovery: addresses by device ID, in memory.
#[derive(Clone, Default)]
struct Board(Arc<Mutex<HashMap<String, Vec<String>>>>);

struct Listing {
    id: String,
    board: Board,
}

impl Directory for Listing {
    fn device_id(&self) -> String {
        self.id.clone()
    }
    fn announce(&self, addresses: &[String]) -> Answer {
        println!("{} announces {addresses:?}", &self.id[..7]);
        self.board
            .0
            .lock()
            .unwrap()
            .insert(self.id.clone(), addresses.to_vec());
        Answer::Announced(Duration::from_secs(1800))
    }
    fn lookup(&self, device: &str) -> Answer {
        match self.board.0.lock().unwrap().get(device) {
            Some(addresses) => Answer::Found(addresses.clone()),
            None => Answer::Wait(Duration::from_secs(60)),
        }
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("windowcast-away-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_paired_client_reaches_its_host_across_two_nats() {
    if std::env::var_os("WINDOWCAST_TEST_NAT").is_none() {
        println!("skipped: builds networks with NAT (root); set WINDOWCAST_TEST_NAT to run");
        return;
    }
    build_networks();
    std::thread::spawn(stun_server);

    // Paired earlier, on a LAN: each trusts the other and knows its
    // discovery ID.
    let host_dir = temp_dir("host");
    let client_dir = temp_dir("client");
    let host_identity = Identity::load_or_generate(&host_dir.join("agent-identity.key")).unwrap();
    let client_identity =
        Identity::load_or_generate(&client_dir.join("client-identity.key")).unwrap();
    let mut trust = TrustStore::default();
    trust.pin(client_identity.peer_id());
    trust.save(&host_dir.join("agent-trusted-clients")).unwrap();
    let mut trust = TrustStore::default();
    trust.pin(host_identity.peer_id());
    trust
        .save(&client_dir.join("client-trusted-hosts"))
        .unwrap();
    let host_disco = discovery_id(&host_identity).unwrap();
    let client_disco = discovery_id(&client_identity).unwrap();
    RemotePeers::load(&host_dir.join("remote-peers.json"))
        .set(&client_identity.peer_id(), &client_disco)
        .unwrap();
    RemotePeers::load(&client_dir.join("remote-hosts.json"))
        .set(&host_identity.peer_id(), &host_disco)
        .unwrap();

    let board = Board::default();
    let config = RemoteConfig {
        servers: vec![],
        stun: vec![STUN.into()],
    };

    // The host, behind its NAT.
    let status = Arc::new(Mutex::new(RemoteStatus::default()));
    {
        let (board, config, host_dir, status, host_disco) = (
            board.clone(),
            config.clone(),
            host_dir.clone(),
            Arc::clone(&status),
            host_disco.clone(),
        );
        std::thread::spawn(move || {
            enter("wc-h");
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async move {
                let control = HostControl::open(&HostConfig {
                    listen: String::new(),
                    pairing: false,
                    data_dir: host_dir,
                })
                .unwrap();
                serve_remote(
                    control,
                    Arc::new(TestPatternSource),
                    RemoteAccess {
                        port: 47101,
                        config,
                        directory: Arc::new(Listing {
                            id: host_disco,
                            board,
                        }),
                    },
                    status,
                )
                .await
                .unwrap();
            });
        });
    }
    // Until the host has announced the address its NAT gave it.
    let start = Instant::now();
    while !board.0.lock().unwrap().contains_key(&host_disco) {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "the host never announced"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let host_addresses = board.0.lock().unwrap()[&host_disco].clone();
    assert_eq!(
        host_addresses,
        ["windowcast://203.0.113.2:47101"],
        "not the NAT's address"
    );

    // The client, behind its own NAT.
    let (frames, took) = {
        let host_id = host_identity.peer_id().to_hex();
        let board = board.clone();
        std::thread::spawn(move || {
            enter("wc-c");
            let client = Client::new(&client_dir).unwrap();
            assert!(client.reachable_away(&host_id));
            let started = Instant::now();
            let session = client
                .connect_away_with(
                    &host_id,
                    &config,
                    &Listing {
                        id: client_disco,
                        board,
                    },
                    Duration::from_secs(100),
                )
                .unwrap();
            let took = started.elapsed();
            session.start_window(WINDOW, &[VideoCodec::H264]).unwrap();
            let mut frames = 0;
            let deadline = Instant::now() + Duration::from_secs(20);
            while frames < 10 && Instant::now() < deadline {
                if let FramePoll::Frame(_) = session.next_frame(WINDOW, Duration::from_secs(1)) {
                    frames += 1;
                }
            }
            (frames, took)
        })
        .join()
        .unwrap()
    };
    println!(
        "connected away in {:.1} s; {frames} frames; host saw {:?}",
        took.as_secs_f32(),
        status.lock().unwrap()
    );
    remove_networks();
    let _ = std::fs::remove_dir_all(host_dir);
    assert!(frames >= 10, "only {frames} frames came across the NATs");
}

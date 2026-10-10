//! RDP on the command line, for trying windowcast's RDP against other RDP
//! hosts and clients.
//!
//! Usage:
//!   windowcast-rdp connect HOST[:PORT] USER [SECONDS] [--app PROGRAM]
//!       logs in (the password from WINDOWCAST_RDP_PASSWORD), reports the
//!       host's certificate fingerprint, the desktop and the pictures seen;
//!       with --app, runs PROGRAM on the host as a RemoteApp and shows only
//!       its window
//!   windowcast-rdp serve [ADDRESS] [USER]
//!       serves the test pattern window over RDP (default 127.0.0.1:3389),
//!       the password from WINDOWCAST_RDP_PASSWORD

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::TestPatternSource;
use windowcast_host::WindowSource;
use windowcast_rdp::client::{connect, ClientConfig};
use windowcast_rdp::host::{serve_window, HostStats, WindowServer};
use windowcast_rdp::tls::HostIdentity;
use windowcast_rdp::Credentials;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let password = std::env::var("WINDOWCAST_RDP_PASSWORD").unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("connect") => {
            let (Some(host), Some(user)) = (args.get(1), args.get(2)) else {
                fail("usage: windowcast-rdp connect HOST[:PORT] USER [SECONDS]")
            };
            let seconds: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);
            let remote_app = args
                .iter()
                .position(|a| a == "--app")
                .and_then(|i| args.get(i + 1))
                .map(|program| windowcast_rdp::remoteapp::RemoteApp {
                    program: program.clone(),
                    arguments: String::new(),
                    working_dir: String::new(),
                });
            let with_port = if host.contains(':') && !host.starts_with('[') {
                host.clone()
            } else {
                format!("{host}:3389")
            };
            let address: SocketAddr = with_port
                .to_socket_addrs()
                .ok()
                .and_then(|mut a| a.next())
                .unwrap_or_else(|| fail("cannot resolve the host"));
            let name = with_port
                .rsplit_once(':')
                .map_or(with_port.as_str(), |(n, _)| n);
            let (domain, user) = match user.split_once('\\') {
                Some((domain, user)) => (Some(domain.to_owned()), user.to_owned()),
                None => (None, user.clone()),
            };
            let stream = connect(&ClientConfig {
                address,
                server_name: name.trim_matches(['[', ']']).to_owned(),
                username: user,
                password,
                domain,
                size: (1280, 720),
                pinned: None,
                remote_app,
            })
            .unwrap_or_else(|e| fail(&e.to_string()));
            let hex: String = stream
                .fingerprint
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            println!(
                "logged in: desktop {}x{}, host certificate SHA-256 {hex}",
                stream.size.0, stream.size.1
            );
            let started = Instant::now();
            let mut pictures = 0u64;
            while started.elapsed() < Duration::from_secs(seconds) {
                if let Ok(picture) = stream.next_picture(Duration::from_millis(200)) {
                    if pictures == 0 {
                        println!("first picture: {}x{}", picture.width, picture.height);
                    }
                    pictures += 1;
                }
                if stream.ended.load(std::sync::atomic::Ordering::SeqCst) {
                    println!("the host ended the session");
                    break;
                }
            }
            println!(
                "{pictures} picture updates in {:.1} s",
                started.elapsed().as_secs_f64()
            );
        }
        Some("serve") => {
            let address = args.get(1).map_or("127.0.0.1:3389", String::as_str);
            let user = args.get(2).map_or("windowcast", String::as_str);
            if password.is_empty() {
                fail("set WINDOWCAST_RDP_PASSWORD to the password clients log in with");
            }
            let listener =
                std::net::TcpListener::bind(address).unwrap_or_else(|e| fail(&e.to_string()));
            let identity = HostIdentity::generate("windowcast test host")
                .unwrap_or_else(|e| fail(&e.to_string()));
            let hex: String = identity
                .fingerprint()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            println!("serving the test pattern over RDP on {address} as {user}; certificate SHA-256 {hex}");
            let source: Arc<dyn WindowSource> = Arc::new(TestPatternSource);
            let window = source.list_windows()[0].id;
            let server = WindowServer {
                source,
                window,
                credentials: Credentials {
                    username: user.to_owned(),
                    password,
                    domain: None,
                },
                identity: Arc::new(identity),
                stats: Arc::new(HostStats::default()),
                stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            };
            if let Err(e) = serve_window(listener, server) {
                fail(&e.to_string());
            }
        }
        _ => fail(
            "usage: windowcast-rdp connect HOST[:PORT] USER [SECONDS] | serve [ADDRESS] [USER]",
        ),
    }
}

fn fail(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1)
}

//! Reference client CLI: connects to a host agent over the LAN, pairs by
//! PIN the first time (or resumes with the pinned identity afterwards),
//! and lists the host's windows over the authenticated session.
//!
//! Usage: `windowcast-client HOST:PORT [--pin PIN]`

use std::path::PathBuf;

use tokio::net::TcpStream;
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::ControlMessage;
use windowcast_transport::{connect, ClientCredential, Session};

#[tokio::main]
async fn main() {
    let mut host = None;
    let mut pin = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--pin" => pin = Some(args.next().expect("--pin needs the PIN the host shows")),
            other if host.is_none() => host = Some(other.to_owned()),
            other => panic!("unknown argument {other}"),
        }
    }
    let host = host.expect("usage: windowcast-client HOST:PORT [--pin PIN]");

    let dir = data_dir();
    let identity = Identity::load_or_generate(&dir.join("client-identity.key"))
        .expect("failed to load or generate this client's persistent identity");
    let trust_path = dir.join("client-trusted-hosts");
    let mut trust = TrustStore::load(&trust_path).expect("failed to read the trusted-host list");
    println!("client identity: {}", identity.peer_id());

    let stream = TcpStream::connect(&host)
        .await
        .unwrap_or_else(|e| panic!("cannot reach {host}: {e}"));
    let session = Session::new()
        .await
        .expect("failed to create WebRTC session");
    let credential = match &pin {
        Some(pin) => ClientCredential::Pin(pin),
        None => ClientCredential::Pinned(&trust),
    };
    let established = connect(stream, session, &identity, credential)
        .await
        .unwrap_or_else(|e| panic!("could not connect to {host}: {e}"));

    if established.paired {
        trust.pin(established.peer);
        trust
            .save(&trust_path)
            .expect("failed to save the trusted-host list");
        println!("paired with host {}", established.peer);
    } else {
        println!("connected to host {}", established.peer);
    }

    let session = established.session;
    session
        .send_control(&ControlMessage::ListWindowsRequest)
        .await
        .expect("failed to send the window-list request");
    loop {
        match session.recv_control().await.expect("session ended") {
            ControlMessage::ListWindowsResponse(windows) => {
                println!("{} open windows:", windows.len());
                for window in windows {
                    println!(
                        "  {:>4}  {:<30}  app_id={}",
                        window.id.0, window.title, window.app_id
                    );
                }
                break;
            }
            other => eprintln!("ignoring {other:?}"),
        }
    }
    let _ = session.close().await;
}

fn data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").expect("HOME must be set")).join(".local/share")
        });
    let dir = base.join("windowcast");
    std::fs::create_dir_all(&dir).expect("failed to create windowcast data directory");
    dir
}

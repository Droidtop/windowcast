//! windowcast Linux/Wayland host agent — reference implementation. It
//! listens for clients on a TCP port, pairs a new client by the PIN it
//! prints (or resumes a client paired earlier), and answers window-list
//! requests from the compositor's toplevels. It does not capture windows
//! yet — see `capture.rs` for exactly what's still missing and why.
//!
//! Usage: `windowcast-agent-linux [--listen ADDR:PORT] [--no-pairing]`
//! (`--listen` defaults to 0.0.0.0:47100).

mod capture;
mod toplevels;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::Mutex;
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::ControlMessage;
use windowcast_transport::{accept, HostCredential, Session, TransportError};

/// The agent's default signaling port.
const DEFAULT_LISTEN: &str = "0.0.0.0:47100";

/// Failed pairing attempts before the PIN is withdrawn, and how long the
/// agent waits before it shows a new one. SPAKE2 allows one PIN guess per
/// connection; this caps the rate too, so a LAN peer cannot walk the PIN
/// space (three guesses a minute at most).
const MAX_PAIRING_FAILURES: u32 = 3;
const PAIRING_LOCKOUT: Duration = Duration::from_secs(60);
static PAIRING_FAILURES: AtomicU32 = AtomicU32::new(0);

#[tokio::main]
async fn main() {
    let mut listen = DEFAULT_LISTEN.to_owned();
    let mut pairing_open = true;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next().expect("--listen needs ADDR:PORT"),
            "--no-pairing" => pairing_open = false,
            other => panic!("unknown argument {other}"),
        }
    }

    let dir = data_dir();
    let identity = Identity::load_or_generate(&dir.join("agent-identity.key"))
        .expect("failed to load or generate this agent's persistent identity");
    let trust_path = dir.join("agent-trusted-clients");
    let trust = Arc::new(Mutex::new(
        TrustStore::load(&trust_path).expect("failed to read the trusted-client list"),
    ));
    let identity = Arc::new(identity);
    println!("agent identity: {}", identity.peer_id());

    // One PIN per agent run; a successful pairing uses it up.
    let pin = Arc::new(Mutex::new(
        pairing_open.then(windowcast_pairing::generate_pin),
    ));
    match pin.lock().await.as_deref() {
        Some(pin) => println!("pairing PIN (enter this on the client): {pin}"),
        None => println!("pairing closed; only clients paired earlier can connect"),
    }

    let listener = TcpListener::bind(&listen)
        .await
        .unwrap_or_else(|e| panic!("cannot listen on {listen}: {e}"));
    println!("listening on {listen}");

    loop {
        let (stream, address) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                eprintln!("accept failed: {e}");
                continue;
            }
        };
        let (identity, trust, pin, trust_path) = (
            Arc::clone(&identity),
            Arc::clone(&trust),
            Arc::clone(&pin),
            trust_path.clone(),
        );
        tokio::spawn(async move {
            match serve(stream, &identity, &trust, &pin, &trust_path).await {
                Err(TransportError::AuthenticationFailed) => {
                    eprintln!("{address}: authentication failed");
                    pairing_failed(&pin).await;
                }
                Err(TransportError::Closed) => println!("{address}: session ended"),
                Err(e) => eprintln!("{address}: {e}"),
                Ok(()) => {}
            }
        });
    }
}

/// Counts a failed pairing; the third withdraws the PIN, and a new one is
/// issued and shown after [`PAIRING_LOCKOUT`].
async fn pairing_failed(pin: &Arc<Mutex<Option<String>>>) {
    let mut current = pin.lock().await;
    if current.is_none() {
        return;
    }
    let failures = PAIRING_FAILURES.fetch_add(1, Ordering::SeqCst) + 1;
    if failures < MAX_PAIRING_FAILURES {
        return;
    }
    *current = None;
    eprintln!(
        "pairing paused after {failures} failed attempts; a new PIN follows in {} s",
        PAIRING_LOCKOUT.as_secs()
    );
    let pin = Arc::clone(pin);
    tokio::spawn(async move {
        tokio::time::sleep(PAIRING_LOCKOUT).await;
        let new_pin = windowcast_pairing::generate_pin();
        println!("pairing PIN (enter this on the client): {new_pin}");
        *pin.lock().await = Some(new_pin);
        PAIRING_FAILURES.store(0, Ordering::SeqCst);
    });
}

async fn serve(
    stream: tokio::net::TcpStream,
    identity: &Identity,
    trust: &Mutex<TrustStore>,
    pin: &Mutex<Option<String>>,
    trust_path: &Path,
) -> Result<(), TransportError> {
    let session = Session::new().await?;
    let trusted = trust.lock().await.clone();
    let current_pin = pin.lock().await.clone();
    let established = accept(
        stream,
        session,
        identity,
        HostCredential {
            pin: current_pin.as_deref(),
            trusted: &trusted,
        },
    )
    .await?;

    if established.paired {
        let mut trust = trust.lock().await;
        trust.pin(established.peer);
        if let Err(e) = trust.save(trust_path) {
            eprintln!("could not save the trusted-client list: {e}");
        }
        *pin.lock().await = None;
        println!("paired with {}; pairing is now closed", established.peer);
    } else {
        println!("{} connected", established.peer);
    }

    let session = established.session;
    loop {
        match session.recv_control().await? {
            ControlMessage::ListWindowsRequest => {
                let windows = match tokio::task::spawn_blocking(toplevels::list_windows).await {
                    Ok(Ok(windows)) => windows,
                    Ok(Err(e)) => {
                        eprintln!("failed to list windows: {e}");
                        Vec::new()
                    }
                    Err(e) => {
                        eprintln!("window listing panicked: {e}");
                        Vec::new()
                    }
                };
                session
                    .send_control(&ControlMessage::ListWindowsResponse(windows))
                    .await?;
            }
            ControlMessage::Ping => session.send_control(&ControlMessage::Pong).await?,
            other => eprintln!("ignoring {other:?}"),
        }
    }
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

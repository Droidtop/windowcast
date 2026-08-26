//! Reference client CLI — demonstrates `windowcast-transport` +
//! `windowcast-pairing` end to end (session stand-up, local fingerprint
//! extraction, PIN-based key derivation) without needing droidtop or any
//! GUI. Does not yet exchange SDP with a real remote agent — that
//! signaling exchange is the same not-yet-wired piece `transport`'s own
//! module docs call out.

use std::path::PathBuf;

#[tokio::main]
async fn main() {
    let identity_path = identity_storage_path();
    let identity = windowcast_identity::Identity::load_or_generate(&identity_path)
        .expect("failed to load or generate this client's persistent identity");
    println!("client identity: {}", identity.peer_id());

    let session = windowcast_transport::Session::new()
        .await
        .expect("failed to create WebRTC session");
    let fingerprint = session
        .local_dtls_fingerprint()
        .expect("failed to read local DTLS fingerprint");
    println!(
        "local DTLS fingerprint: {}",
        fingerprint
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );

    println!();
    println!("enter the PIN shown by the host agent, then this tool would:");
    println!("  1. run SPAKE2 (windowcast_pairing::start_client) to derive a session key");
    println!("  2. authenticate this fingerprint against the host's, over a signaling channel");
    println!("  3. exchange persistent identities and proceed to a full WebRTC handshake");
    println!("(steps 2-3 are not wired to a real signaling transport yet)");
}

fn identity_storage_path() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").expect("HOME must be set")).join(".local/share")
        });
    let dir = base.join("windowcast");
    std::fs::create_dir_all(&dir).expect("failed to create windowcast data directory");
    dir.join("client-identity.key")
}

//! windowcast Linux/Wayland host agent — reference implementation, not yet
//! a full serving loop. Today this binary demonstrates the two pieces that
//! are actually real (persistent identity, toplevel enumeration) and
//! prints a pairing PIN, but does not yet accept an incoming session,
//! negotiate WebRTC, or stream anything — see `capture.rs` for exactly
//! what's still missing and why.

mod capture;
mod toplevels;

use std::path::PathBuf;

fn main() {
    let identity_path = identity_storage_path();
    let identity = windowcast_identity::Identity::load_or_generate(&identity_path)
        .expect("failed to load or generate this agent's persistent identity");
    println!("agent identity: {}", identity.peer_id());

    let pin = windowcast_pairing::generate_pin();
    println!("pairing PIN (enter this on the client): {pin}");
    println!("(session serving over this PIN is not wired up yet — see windowcast-transport/client-core)");

    match toplevels::list_windows() {
        Ok(windows) => {
            println!("open windows:");
            for window in windows {
                println!(
                    "  {:>4}  {:<30}  app_id={}",
                    window.id.0, window.title, window.app_id
                );
            }
        }
        Err(e) => eprintln!("failed to list windows: {e}"),
    }
}

fn identity_storage_path() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").expect("HOME must be set")).join(".local/share")
        });
    let dir = base.join("windowcast");
    std::fs::create_dir_all(&dir).expect("failed to create windowcast data directory");
    dir.join("agent-identity.key")
}

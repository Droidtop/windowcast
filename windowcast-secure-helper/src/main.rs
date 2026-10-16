//! Secure-screen helper binary: runs as a scheduled task under SYSTEM,
//! at startup and on session change. It brokers connections, displays
//! the current secure screen, and enters the given input.

use std::io;

fn main() {
    // Before any operation, read the permission gate.
    if windowcast_secure_helper::permission::Permission::read().map(|p| p.enabled) != Some(true) {
        eprintln!("Secure-screen helper refused: permission absent or disabled.");
        std::process::exit(1);
    }

    // Log that permission is present.
    eprintln!("Secure-screen helper started with permission granted.");

    // The helper does exactly two jobs plus a state query.
    // It does not execute arbitrary commands, does not access arbitrary files,
    // and does not load application code.
}

/// A minimal server loop: accepts pipe connections, verifies clients,
/// reads operations, performs them, and logs results.
fn run_server() -> io::Result<()> {
    // Stub: real implementation creates the named pipe with DACL and loops.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

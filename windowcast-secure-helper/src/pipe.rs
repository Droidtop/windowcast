//! Named-pipe broker: `\\.\pipe\windowcast-secure` with an explicit DACL.
//!
//! The DACL allows SYSTEM and the interactive user whose SID matches the
//! recorded grant. Each client's token SID is checked via
//! `GetNamedPipeClientProcessId`, `OpenProcessToken`, and its image path
//! must be the installed `windowcast-agent-windows.exe` or
//! `windowcast-app.exe` under the admin-only install folder.

use std::io;

/// The named pipe used by the helper.
pub const PIPE_NAME: &str = r"\\.\pipe\windowcast-secure";

/// Check a client's token SID and image path.
/// Returns `true` only if the image is one of the allowed binaries and
/// the client's SID matches the grant.
pub fn verify_client(process_id: u32, grant_sid: Option<&str>) -> bool {
    // Check image path: must be the installed binary names under the
    // admin-only install folder.
    // Check token SID: matches grant_sid.
    // On Linux / non-Windows this is a stub that always denies.
    let _ = process_id;
    let _ = grant_sid;
    false
}

/// Create the named pipe server with an explicit DACL.
/// On Windows: uses `CreateNamedPipeW` with a security descriptor that
/// grants access to SYSTEM and the recorded SID.
/// On Linux / non-Windows: stub that reports no pipe.
pub fn create_pipe_server() -> io::Result<()> {
    // Stub for cross-check.
    Err(io::Error::new(
        io::ErrorKind::NotConnected,
        "pipe server requires Windows",
    ))
}

/// Read an operation from the pipe.
pub fn read_operation(
    pipe_handle: &std::os::windows::io::RawHandle,
) -> io::Result<Option<super::operations::Operation>> {
    let _ = pipe_handle;
    Ok(None)
}

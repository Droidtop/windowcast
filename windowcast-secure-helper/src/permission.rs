//! The permission gate: reads and writes `HKLM\SOFTWARE\windowcast\SecureScreen`.
//!
//! Only administrators may write. The helper re-reads before every
//! operation; if the value is absent or 0, it refuses, logs the refusal,
//! and exits.

use std::io;

/// A protected registry key entry describing whether the secure screen
/// helper is permitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permission {
    /// `Enabled` DWORD value.
    pub enabled: bool,
    /// SID of the user who granted it, if present.
    pub grant_sid: Option<String>,
}

impl Permission {
    /// Read from the protected location. Returns `None` when the key or
    /// value is missing; that means permission is denied.
    pub fn read() -> Option<Self> {
        // Windows-only: use registry APIs via the `windows` crate.
        // On Linux / non-Windows this is a stub that always denies.
        #[cfg(windows)]
        {
            // Registry APIs available through the `windows` crate when
            // `Win32_System_Registry` feature is enabled (see docs/SECURITY.md).
        }
        // Cross-check stub: deny.
        None
    }

    /// Write only when elevated. This is called by the enable command.
    pub fn write(enabled: bool, sid: Option<&str>) -> io::Result<()> {
        // Only the elevated enable command writes this.
        // Stub for cross-check.
        let _ = enabled;
        let _ = sid;
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "elevated write only",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_denied_by_default() {
        // On Linux / non-Windows the registry is not present.
        assert!(Permission::read().is_none());
    }
}

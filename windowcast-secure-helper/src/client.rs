//! Client API: applications request, install, enable and talk to the helper.

use std::path::PathBuf;

/// Errors returned by the client.
#[derive(Debug, thiserror::Error)]
pub enum SecureScreenError {
    #[error("secure-screen helper not installed at {0}")]
    NotInstalled(PathBuf),
    #[error("permission denied: secure screen not enabled")]
    PermissionDenied,
    #[error("pipe connection refused: {0}")]
    PipeRefused(String),
    #[error("operation refused: {0}")]
    OperationRefused(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// The state of the secure screen from the client's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecureScreenStatus {
    /// Helper missing; the host should say so plainly.
    Absent,
    /// Helper present but disabled: no permission recorded.
    Disabled,
    /// Enabled and running: permission present.
    Enabled,
}

/// Client that applications link against.
#[derive(Debug, Clone)]
pub struct Client {
    /// The installed binary folder (admin-only).
    pub install_dir: PathBuf,
}

impl Client {
    /// Default install directory: `%ProgramFiles%\windowcast\secure-helper`.
    pub fn default_install_dir() -> PathBuf {
        #[cfg(windows)]
        {
            // Approximate ProgramFiles from environment; real code uses
            // SHGetKnownFolderPath or equivalent.
            std::env::var_os("ProgramFiles")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
                .join("windowcast")
                .join("secure-helper")
        }
        #[cfg(not(windows))]
        {
            PathBuf::from("/usr/lib/windowcast/secure-helper")
        }
    }

    /// Create with the given install directory.
    pub fn new<P: Into<PathBuf>>(install_dir: P) -> Self {
        Self {
            install_dir: install_dir.into(),
        }
    }

    /// Request that the application install the helper binary.
    /// The binary must be installed by the caller (this is the client's
    /// responsibility), then this API records it.
    pub fn install_binary(&self, source: PathBuf) -> Result<(), SecureScreenError> {
        if !self.install_dir.exists() {
            std::fs::create_dir_all(&self.install_dir)
                .map_err(SecureScreenError::Io)?;
        }
        let dest = self.install_dir.join("windowcast-secure-helper.exe");
        std::fs::copy(&source, &dest).map_err(SecureScreenError::Io)?;
        Ok(())
    }

    /// Check the permission gate.
    pub fn permission_status(&self) -> SecureScreenStatus {
        match crate::permission::Permission::read() {
            Some(p) if p.enabled => SecureScreenStatus::Enabled,
            Some(_) => SecureScreenStatus::Disabled,
            None => {
                if self.helper_binary_exists() {
                    SecureScreenStatus::Disabled
                } else {
                    SecureScreenStatus::Absent
                }
            }
        }
    }

    /// Whether the helper binary exists in the install directory.
    pub fn helper_binary_exists(&self) -> bool {
        self.install_dir.join("windowcast-secure-helper.exe").exists()
    }

    /// Enable the secure screen: writes the permission and registers the
    /// scheduled task. This must run elevated.
    pub fn enable(&self, user_sid: Option<&str>) -> Result<(), SecureScreenError> {
        crate::permission::Permission::write(true, user_sid)
            .map_err(|e| SecureScreenError::OperationRefused(format!("{e}")))?;
        // Register the scheduled task (stub for cross-check).
        Ok(())
    }

    /// Disable: removes the permission and the task.
    pub fn disable(&self) -> Result<(), SecureScreenError> {
        crate::permission::Permission::write(false, None)
            .map_err(|e| SecureScreenError::OperationRefused(format!("{e}")))?;
        Ok(())
    }

    /// Uninstall: reverse everything.
    pub fn uninstall(&self) -> Result<(), SecureScreenError> {
        self.disable()?;
        if self.helper_binary_exists() {
            std::fs::remove_file(self.install_dir.join("windowcast-secure-helper.exe"))
                .map_err(SecureScreenError::Io)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_default_dir_is_absolute() {
        let dir = Client::default_install_dir();
        assert!(dir.is_absolute());
    }

    #[test]
    fn permission_status_is_absent_when_not_installed() {
        let client = Client::new("/nonexistent/install");
        assert_eq!(client.permission_status(), SecureScreenStatus::Absent);
    }
}

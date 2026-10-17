//! Unprivileged client API and elevated lifecycle management.

use std::io;
use std::path::{Path, PathBuf};

use crate::operations::{self, Operation, Response};

const TASK_NAME: &str = r"\windowcast\Secure screen helper";

#[derive(Debug, thiserror::Error)]
pub enum SecureScreenError {
    #[error("secure-screen helper not installed at {0}")]
    NotInstalled(PathBuf),
    #[error("secure screen is not available on this platform")]
    NotAvailable,
    #[error("permission denied: secure screen is not enabled")]
    PermissionDenied,
    #[error("secure-screen helper refused: {0}")]
    OperationRefused(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecureScreenStatus {
    Absent,
    Disabled,
    Enabled,
    NotAvailable,
}

#[derive(Debug, Clone)]
pub struct Client {
    pub install_dir: PathBuf,
}

impl Default for Client {
    fn default() -> Self {
        Self::new(Self::default_install_dir())
    }
}

impl Client {
    pub fn default_install_dir() -> PathBuf {
        #[cfg(windows)]
        {
            program_files_dir().join("windowcast").join("secure-helper")
        }
        #[cfg(not(windows))]
        {
            PathBuf::from("/usr/lib/windowcast/secure-helper")
        }
    }

    pub fn admin_install_root() -> PathBuf {
        Self::default_install_dir()
            .parent()
            .expect("secure-helper has a parent")
            .to_owned()
    }

    pub fn new<P: Into<PathBuf>>(install_dir: P) -> Self {
        Self {
            install_dir: install_dir.into(),
        }
    }

    pub fn helper_path(&self) -> PathBuf {
        self.install_dir.join("windowcast-secure-helper.exe")
    }

    pub fn install_binary(&self, source: impl AsRef<Path>) -> Result<(), SecureScreenError> {
        self.require_windows_install_dir()?;
        std::fs::create_dir_all(&self.install_dir)?;
        std::fs::copy(source, self.helper_path())?;
        Ok(())
    }

    pub fn permission_status(&self) -> SecureScreenStatus {
        if !cfg!(windows) {
            return SecureScreenStatus::NotAvailable;
        }
        if !self.helper_binary_exists() {
            return SecureScreenStatus::Absent;
        }
        if crate::permission::Permission::read().is_some() {
            SecureScreenStatus::Enabled
        } else {
            SecureScreenStatus::Disabled
        }
    }

    pub fn helper_binary_exists(&self) -> bool {
        self.helper_path().is_file()
    }

    /// Enable after the caller has shown the consent text. Must run elevated.
    pub fn enable(&self, user_sid: Option<&str>) -> Result<(), SecureScreenError> {
        self.require_windows_install_dir()?;
        if !self.helper_binary_exists() {
            return Err(SecureScreenError::NotInstalled(self.helper_path()));
        }
        let sid = match user_sid {
            Some(sid) => sid.to_owned(),
            None => crate::permission::current_user_sid()?,
        };
        crate::permission::Permission::write(true, Some(&sid))?;
        if let Err(error) = register_task(&self.helper_path()) {
            let _ = crate::permission::Permission::write(false, None);
            return Err(error.into());
        }
        Ok(())
    }

    pub fn disable(&self) -> Result<(), SecureScreenError> {
        self.require_windows_install_dir()?;
        unregister_task()?;
        crate::permission::Permission::write(false, None)?;
        // The broker polls the protected grant while idle and exits promptly.
        std::thread::sleep(std::time::Duration::from_millis(200));
        Ok(())
    }

    pub fn uninstall(&self) -> Result<(), SecureScreenError> {
        self.disable()?;
        if self.helper_binary_exists() {
            std::fs::remove_file(self.helper_path())?;
        }
        if self.install_dir.is_dir() {
            let _ = std::fs::remove_dir(&self.install_dir);
        }
        Ok(())
    }

    pub fn request(&self, operation: &Operation) -> Result<Response, SecureScreenError> {
        if !cfg!(windows) {
            return Err(SecureScreenError::NotAvailable);
        }
        if !self.helper_binary_exists() {
            return Err(SecureScreenError::NotInstalled(self.helper_path()));
        }
        if crate::permission::Permission::read().is_none() {
            return Err(SecureScreenError::PermissionDenied);
        }
        let mut pipe = crate::pipe::connect()?;
        operations::write_request(&mut pipe, operation)?;
        match operations::read_response(&mut pipe)? {
            Some(Response::Refused(reason)) => Err(SecureScreenError::OperationRefused(reason)),
            Some(response) => Ok(response),
            None => {
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "helper closed the pipe").into())
            }
        }
    }

    fn require_windows_install_dir(&self) -> Result<(), SecureScreenError> {
        if !cfg!(windows) {
            return Err(SecureScreenError::NotAvailable);
        }
        if !same_path(&self.install_dir, &Self::default_install_dir()) {
            return Err(SecureScreenError::OperationRefused(
                "the helper may only be managed in the admin-only Program Files location".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
fn program_files_dir() -> PathBuf {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

    // Microsoft: SHGetKnownFolderPath(FOLDERID_ProgramFiles) resolves the protected system
    // location rather than trusting a caller-controlled environment variable.
    let path = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, None) };
    path.ok()
        .and_then(|path| {
            let result = unsafe { path.to_string() }.ok().map(PathBuf::from);
            // Microsoft: SHGetKnownFolderPath returns memory owned by the COM task allocator.
            unsafe { CoTaskMemFree(Some(path.0.cast())) };
            result
        })
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .trim_end_matches(['/', '\\'])
        .eq_ignore_ascii_case(right.to_string_lossy().trim_end_matches(['/', '\\']))
}

#[cfg(windows)]
fn register_task(helper: &Path) -> io::Result<()> {
    use std::io::Write;
    use std::process::Command;

    let command = xml_escape(&helper.to_string_lossy());
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Shows and controls the secure desktop only when the windowcast owner has enabled it.</Description></RegistrationInfo>
  <Triggers><BootTrigger><Enabled>true</Enabled></BootTrigger><SessionStateChangeTrigger><Enabled>true</Enabled><StateChange>ConsoleConnect</StateChange></SessionStateChangeTrigger><SessionStateChangeTrigger><Enabled>true</Enabled><StateChange>SessionUnlock</StateChange></SessionStateChangeTrigger></Triggers>
  <Principals><Principal id="System"><UserId>S-1-5-18</UserId><LogonType>ServiceAccount</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>
  <Actions Context="System"><Exec><Command>{command}</Command></Exec></Actions>
</Task>"#
    );
    let path =
        std::env::temp_dir().join(format!("windowcast-secure-task-{}.xml", std::process::id()));
    let mut file = std::fs::File::create(&path)?;
    // The declaration says UTF-16; the BOM lets Task Scheduler identify UTF-16LE reliably.
    file.write_all(&[0xff, 0xfe])?;
    for unit in xml.encode_utf16() {
        file.write_all(&unit.to_le_bytes())?;
    }
    drop(file);
    // Microsoft: schtasks /Create /XML registers this fixed SYSTEM task definition.
    let status = Command::new("schtasks.exe")
        .args(["/Create", "/TN", TASK_NAME, "/XML"])
        .arg(&path)
        .arg("/F")
        .status();
    let _ = std::fs::remove_file(path);
    let status = status?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(
            "Task Scheduler refused the secure-screen task",
        ))
    }
}

#[cfg(not(windows))]
fn register_task(_helper: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure screen is not available on this platform",
    ))
}

#[cfg(windows)]
fn unregister_task() -> io::Result<()> {
    let query = std::process::Command::new("schtasks.exe")
        .args(["/Query", "/TN", TASK_NAME])
        .output()?;
    if !query.status.success() {
        return Ok(());
    }
    let output = std::process::Command::new("schtasks.exe")
        .args(["/Delete", "/TN", TASK_NAME, "/F"])
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(
            "Task Scheduler refused to remove the secure-screen task",
        ))
    }
}

#[cfg(not(windows))]
fn unregister_task() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure screen is not available on this platform",
    ))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_command_is_xml_escaped() {
        assert_eq!(xml_escape("a&<b>\"'"), "a&amp;&lt;b&gt;&quot;&apos;");
    }

    #[cfg(not(windows))]
    #[test]
    fn api_plainly_reports_unavailable_off_windows() {
        let client = Client::default();
        assert_eq!(client.permission_status(), SecureScreenStatus::NotAvailable);
        assert!(matches!(
            client.request(&Operation::CaptureFrame),
            Err(SecureScreenError::NotAvailable)
        ));
    }
}

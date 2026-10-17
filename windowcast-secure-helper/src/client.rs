//! Unprivileged client API and elevated lifecycle management.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::operations::{self, Operation, Response};

#[cfg(windows)]
const TASK_FOLDER: &str = r"\windowcast";
#[cfg(windows)]
const TASK_NAME: &str = "Secure screen helper";

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
    pipe: Arc<Mutex<Option<std::fs::File>>>,
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
            pipe: Arc::new(Mutex::new(None)),
        }
    }

    pub fn helper_path(&self) -> PathBuf {
        self.install_dir.join("windowcast-secure-helper.exe")
    }

    pub fn install_binary(&self, source: impl AsRef<Path>) -> Result<(), SecureScreenError> {
        self.require_windows_install_dir()?;
        std::fs::create_dir_all(&self.install_dir)?;
        secure_install_directories(&self.install_dir)?;
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
    pub fn enable(&self, user_sid: &str) -> Result<(), SecureScreenError> {
        self.require_windows_install_dir()?;
        if !self.helper_binary_exists() {
            return Err(SecureScreenError::NotInstalled(self.helper_path()));
        }
        if !crate::permission::validate_grant_sid(user_sid) {
            return Err(SecureScreenError::OperationRefused(
                "the unelevated granting SID is invalid".into(),
            ));
        }
        crate::permission::Permission::write(true, Some(user_sid))?;
        if let Err(error) = register_task(&self.helper_path()) {
            let _ = crate::permission::Permission::write(false, None);
            return Err(error.into());
        }
        Ok(())
    }

    pub fn disable(&self) -> Result<(), SecureScreenError> {
        self.require_windows_install_dir()?;
        crate::permission::Permission::write(false, None)?;
        if let Ok(mut pipe) = self.pipe.lock() {
            *pipe = None;
        }
        unregister_task()?;
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
        let mut slot = self
            .pipe
            .lock()
            .map_err(|_| io::Error::other("secure-screen pipe lock was poisoned"))?;
        for attempt in 0..2 {
            if slot.is_none() {
                *slot = Some(crate::pipe::connect(&self.helper_path())?);
            }
            let pipe = slot.as_mut().expect("pipe was connected");
            let exchanged = operations::write_request(pipe, operation)
                .and_then(|()| operations::read_response(pipe));
            match exchanged {
                Ok(Some(Response::Refused(reason))) => {
                    return Err(SecureScreenError::OperationRefused(reason));
                }
                Ok(Some(response)) => return Ok(response),
                Ok(None) => {
                    *slot = None;
                    if attempt == 1 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "helper closed the pipe",
                        )
                        .into());
                    }
                }
                Err(error) => {
                    *slot = None;
                    if attempt == 1 {
                        return Err(error.into());
                    }
                }
            }
        }
        unreachable!()
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
fn secure_install_directories(helper_dir: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        SetFileSecurityW, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR,
    };

    fn apply(path: &Path, sddl: &str) -> io::Result<()> {
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(|error| io::Error::other(error.to_string()))?;
        // Microsoft: SetFileSecurity applies the protected DACL to the install directory.
        let applied = unsafe {
            SetFileSecurityW(
                PCWSTR(path.as_ptr()),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor,
            )
        };
        unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
        applied
            .ok()
            .map_err(|error| io::Error::other(error.to_string()))
    }

    let root = helper_dir
        .parent()
        .ok_or_else(|| io::Error::other("install directory has no parent"))?;
    // Users may execute installed clients, but only administrators and SYSTEM may modify them.
    apply(
        root,
        "D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)",
    )?;
    // Users can inspect and execute the helper, but only administrators and SYSTEM can replace it.
    apply(
        helper_dir,
        "D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)",
    )
}

#[cfg(not(windows))]
fn secure_install_directories(_helper_dir: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure screen is not available on this platform",
    ))
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
    use windows::core::BSTR;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::TaskScheduler::{
        ITaskService, TaskScheduler, TASK_CREATE_OR_UPDATE, TASK_LOGON_SERVICE_ACCOUNT,
    };
    use windows::Win32::System::Variant::VARIANT;

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
    let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let result = (|| -> windows::core::Result<()> {
        // Microsoft: CoCreateInstance opens the Task Scheduler 2.0 service in-process.
        let service: ITaskService =
            unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER) }?;
        let empty = VARIANT::default();
        unsafe { service.Connect(&empty, &empty, &empty, &empty) }?;
        let root = unsafe { service.GetFolder(&BSTR::from("\\")) }?;
        let folder = unsafe { service.GetFolder(&BSTR::from(TASK_FOLDER)) }
            .or_else(|_| unsafe { root.CreateFolder(&BSTR::from("windowcast"), &empty) })?;
        let system = VARIANT::from("SYSTEM");
        // Microsoft: ITaskFolder::RegisterTask accepts the fixed XML directly; no temporary
        // file or command interpreter participates in registration.
        unsafe {
            folder.RegisterTask(
                &BSTR::from(TASK_NAME),
                &BSTR::from(xml),
                TASK_CREATE_OR_UPDATE.0,
                &system,
                &empty,
                TASK_LOGON_SERVICE_ACCOUNT,
                &empty,
            )
        }?;
        Ok(())
    })();
    if initialized {
        unsafe { CoUninitialize() };
    }
    result.map_err(|error| io::Error::other(error.to_string()))
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
    use windows::core::BSTR;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::TaskScheduler::{ITaskService, TaskScheduler};
    use windows::Win32::System::Variant::VARIANT;

    let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let result = (|| -> windows::core::Result<()> {
        let service: ITaskService =
            unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER) }?;
        let empty = VARIANT::default();
        unsafe { service.Connect(&empty, &empty, &empty, &empty) }?;
        let folder = match unsafe { service.GetFolder(&BSTR::from(TASK_FOLDER)) } {
            Ok(folder) => folder,
            Err(error) if error.code().0 as u32 == 0x8007_0002 => return Ok(()),
            Err(error) => return Err(error),
        };
        match unsafe { folder.DeleteTask(&BSTR::from(TASK_NAME), 0) } {
            Ok(()) => Ok(()),
            Err(error) if error.code().0 as u32 == 0x8007_0002 => Ok(()),
            Err(error) => Err(error),
        }
    })();
    if initialized {
        unsafe { CoUninitialize() };
    }
    result.map_err(|error| io::Error::other(error.to_string()))
}

#[cfg(not(windows))]
fn unregister_task() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure screen is not available on this platform",
    ))
}

#[cfg(windows)]
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

    #[cfg(windows)]
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

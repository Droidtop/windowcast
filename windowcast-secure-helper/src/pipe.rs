//! Authenticated local named-pipe transport.

use std::io;
use std::path::{Path, PathBuf};

pub const PIPE_NAME: &str = r"\\.\pipe\windowcast-secure";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    pub sid: String,
    pub image: PathBuf,
}

pub trait ClientInspector {
    fn inspect(&self, process_id: u32) -> io::Result<ClientIdentity>;
}

pub fn verify_client_with(
    inspector: &impl ClientInspector,
    process_id: u32,
    grant_sid: &str,
    install_root: &Path,
) -> bool {
    inspector.inspect(process_id).is_ok_and(|identity| {
        identity.sid.eq_ignore_ascii_case(grant_sid) && allowed_image(&identity.image, install_root)
    })
}

fn allowed_image(image: &Path, install_root: &Path) -> bool {
    let Some(name) = image.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let allowed_name = name.eq_ignore_ascii_case("windowcast-agent-windows.exe")
        || name.eq_ignore_ascii_case("windowcast-app.exe");
    allowed_name
        && image
            .parent()
            .is_some_and(|parent| same_path(parent, install_root))
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .eq_ignore_ascii_case(right.to_string_lossy().trim_end_matches(['\\', '/']))
}

#[cfg(windows)]
pub use windows_impl::{connect, PipeServer, WindowsClientInspector};

#[cfg(not(windows))]
pub fn connect() -> io::Result<std::fs::File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure screen is not available on this platform",
    ))
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, LocalFree, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
    };
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY};
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
        SetNamedPipeHandleState, PIPE_NOWAIT, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    pub struct PipeServer {
        file: File,
    }

    impl PipeServer {
        pub fn create(grant_sid: &str) -> io::Result<Self> {
            // Protected DACL: full access for SYSTEM; read/write for exactly the grant SID.
            let sddl = format!("D:P(A;;GA;;;SY)(A;;GRGW;;;{grant_sid})");
            let sddl = wide(&sddl);
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            // Microsoft: ConvertStringSecurityDescriptor... builds a self-relative descriptor from SDDL.
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    PCWSTR(sddl.as_ptr()),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    None,
                )
            }
            .map_err(io_error)?;
            let security = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: false.into(),
            };
            let name = wide(PIPE_NAME);
            // Microsoft: CreateNamedPipe creates one local-only, byte-mode broker instance.
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(name.as_ptr()),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    64 * 1024,
                    64 * 1024,
                    0,
                    Some(&security),
                )
            };
            unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                file: unsafe { File::from_raw_handle(handle.0 as RawHandle) },
            })
        }

        pub fn accept(&mut self) -> io::Result<Option<u32>> {
            let handle = HANDLE(self.file.as_raw_handle());
            // Microsoft: ConnectNamedPipe waits for the one client of this instance.
            if let Err(error) = unsafe { ConnectNamedPipe(handle, None) } {
                // Microsoft: ERROR_PIPE_LISTENING means the nonblocking instance is still idle.
                if error.code().0 as u32 == 536 {
                    return Ok(None);
                }
                // Microsoft: ERROR_PIPE_CONNECTED means the client won the connect race.
                if error.code().0 as u32 != 535 {
                    return Err(io_error(error));
                }
            }
            let mode = PIPE_READMODE_BYTE | PIPE_WAIT;
            // Microsoft: SetNamedPipeHandleState restores blocking reads after polled accept.
            unsafe { SetNamedPipeHandleState(handle, Some(&mode), None, None) }
                .map_err(io_error)?;
            let mut process_id = 0;
            // Microsoft: GetNamedPipeClientProcessId identifies the peer for token checks.
            unsafe { GetNamedPipeClientProcessId(handle, &mut process_id) }.map_err(io_error)?;
            Ok(Some(process_id))
        }

        pub fn file_mut(&mut self) -> &mut File {
            &mut self.file
        }

        pub fn disconnect(&mut self) {
            // Microsoft: DisconnectNamedPipe makes the instance available for a new client.
            let handle = HANDLE(self.file.as_raw_handle());
            let _ = unsafe { DisconnectNamedPipe(handle) };
            let mode = PIPE_READMODE_BYTE | PIPE_NOWAIT;
            let _ = unsafe { SetNamedPipeHandleState(handle, Some(&mode), None, None) };
        }
    }

    use std::os::windows::io::AsRawHandle;

    pub fn connect() -> io::Result<File> {
        OpenOptions::new().read(true).write(true).open(PIPE_NAME)
    }

    pub struct WindowsClientInspector;

    impl ClientInspector for WindowsClientInspector {
        fn inspect(&self, process_id: u32) -> io::Result<ClientIdentity> {
            // Microsoft: OpenProcess grants only query-limited access to the pipe client.
            let process =
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
                    .map_err(io_error)?;
            let result = inspect_process(process);
            unsafe { CloseHandle(process) }.ok();
            result
        }
    }

    fn inspect_process(process: HANDLE) -> io::Result<ClientIdentity> {
        let mut token = HANDLE::default();
        // Microsoft: OpenProcessToken opens only enough access to read TokenUser.
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.map_err(io_error)?;
        let sid = crate::permission::windows_impl::token_sid(token);
        unsafe { CloseHandle(token) }.ok();
        let sid = sid?;

        let mut path = vec![0u16; 32_768];
        let mut length = path.len() as u32;
        // Microsoft: QueryFullProcessImageName returns the executable's full Win32 path.
        unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(path.as_mut_ptr()),
                &mut length,
            )
        }
        .map_err(io_error)?;
        path.truncate(length as usize);
        Ok(ClientIdentity {
            sid,
            image: PathBuf::from(String::from_utf16_lossy(&path)),
        })
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn io_error(error: windows::core::Error) -> io::Error {
        io::Error::other(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Result<ClientIdentity, io::Error>);

    impl ClientInspector for Fake {
        fn inspect(&self, _process_id: u32) -> io::Result<ClientIdentity> {
            self.0
                .as_ref()
                .map(Clone::clone)
                .map_err(|e| io::Error::new(e.kind(), e.to_string()))
        }
    }

    #[test]
    fn client_needs_matching_sid_name_and_exact_folder() {
        let root = Path::new(r"C:\Program Files\windowcast");
        let identity = |sid: &str, path: &str| {
            Fake(Ok(ClientIdentity {
                sid: sid.into(),
                image: path.into(),
            }))
        };
        assert!(verify_client_with(
            &identity(
                "S-1-5-21-7",
                r"C:\Program Files\windowcast\windowcast-app.exe"
            ),
            4,
            "S-1-5-21-7",
            root
        ));
        assert!(!verify_client_with(
            &identity(
                "S-1-5-21-8",
                r"C:\Program Files\windowcast\windowcast-app.exe"
            ),
            4,
            "S-1-5-21-7",
            root
        ));
        assert!(!verify_client_with(
            &identity("S-1-5-21-7", r"C:\Users\me\windowcast-app.exe"),
            4,
            "S-1-5-21-7",
            root
        ));
        assert!(!verify_client_with(
            &identity("S-1-5-21-7", r"C:\Program Files\windowcast\other.exe"),
            4,
            "S-1-5-21-7",
            root
        ));
    }

    #[test]
    fn inspection_failure_denies() {
        let fake = Fake(Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "no token",
        )));
        assert!(!verify_client_with(
            &fake,
            4,
            "S-1-5-21-7",
            Path::new(r"C:\Program Files\windowcast")
        ));
    }
}

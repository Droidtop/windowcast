//! Authenticated local named-pipe transport.

use std::io;
use std::path::{Path, PathBuf};

pub const PIPE_NAME: &str = r"\\.\pipe\windowcast-secure";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    pub sid: String,
    pub image: PathBuf,
    pub session_id: u32,
}

pub trait ClientInspector {
    fn inspect(&self, process_id: u32) -> io::Result<ClientIdentity>;
}

pub fn verify_client_with(
    inspector: &impl ClientInspector,
    process_id: u32,
    grant_sid: &str,
    install_root: &Path,
    session_id: u32,
) -> bool {
    inspector.inspect(process_id).is_ok_and(|identity| {
        identity.sid.eq_ignore_ascii_case(grant_sid)
            && identity.session_id == session_id
            && allowed_client_image(&identity.image, install_root)
    })
}

pub fn verify_server_with(
    inspector: &impl ClientInspector,
    process_id: u32,
    helper_path: &Path,
) -> bool {
    inspector.inspect(process_id).is_ok_and(|identity| {
        identity.sid.eq_ignore_ascii_case("S-1-5-18") && same_path(&identity.image, helper_path)
    })
}

fn allowed_client_image(image: &Path, install_root: &Path) -> bool {
    let normalized = image.to_string_lossy().replace('/', "\\");
    let Some((parent, name)) = normalized.rsplit_once('\\') else {
        return false;
    };
    let allowed_name = name.eq_ignore_ascii_case("windowcast-agent-windows.exe")
        || name.eq_ignore_ascii_case("windowcast-app.exe");
    allowed_name && same_path(Path::new(parent), install_root)
}

fn same_path(left: &Path, right: &Path) -> bool {
    let left = left.to_string_lossy().replace('/', "\\");
    let right = right.to_string_lossy().replace('/', "\\");
    left.trim_end_matches('\\')
        .eq_ignore_ascii_case(right.trim_end_matches('\\'))
}

#[cfg(windows)]
pub use windows_impl::{connect, request_ready, PipeServer, WindowsClientInspector};

#[cfg(not(windows))]
pub fn connect(_helper_path: &Path) -> io::Result<std::fs::File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure screen is not available on this platform",
    ))
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, LocalFree, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
    };
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY};
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION,
        SECURITY_SQOS_PRESENT,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
        GetNamedPipeServerProcessId, PeekNamedPipe, SetNamedPipeHandleState, PIPE_NOWAIT,
        PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    pub struct PipeServer {
        file: File,
    }

    impl PipeServer {
        pub fn create(grant_sid: &str) -> io::Result<Self> {
            // Protected DACL: SYSTEM full access; the grant SID gets generic read plus only
            // FILE_WRITE_DATA (0x2), rather than generic write.
            let sddl = format!("D:P(A;;GA;;;SY)(A;;0x80000002;;;{grant_sid})");
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

    pub fn connect(helper_path: &Path) -> io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags((SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION).0)
            .open(PIPE_NAME)?;
        let mut process_id = 0;
        // Microsoft: GetNamedPipeServerProcessId identifies the broker behind this handle.
        unsafe { GetNamedPipeServerProcessId(HANDLE(file.as_raw_handle()), &mut process_id) }
            .map_err(io_error)?;
        if !verify_server_with(&WindowsClientInspector, process_id, helper_path) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "named-pipe server is not the installed LocalSystem helper",
            ));
        }
        Ok(file)
    }

    pub fn request_ready(file: &File) -> io::Result<bool> {
        let mut prefix = [0u8; 4];
        let mut prefix_read = 0;
        let mut available = 0;
        // Microsoft: PeekNamedPipe checks for an entire framed request without consuming it or
        // allowing a client with a partial payload to block the broker forever.
        unsafe {
            PeekNamedPipe(
                HANDLE(file.as_raw_handle()),
                Some(prefix.as_mut_ptr().cast()),
                prefix.len() as u32,
                Some(&mut prefix_read),
                Some(&mut available),
                None,
            )
        }
        .map_err(io_error)?;
        if prefix_read < 4 {
            return Ok(false);
        }
        let payload = u32::from_le_bytes(prefix) as usize;
        if payload == 0 || payload > crate::operations::MAX_REQUEST_BYTES {
            return Ok(true);
        }
        Ok(available as usize >= 4 + payload)
    }

    pub struct WindowsClientInspector;

    impl ClientInspector for WindowsClientInspector {
        fn inspect(&self, process_id: u32) -> io::Result<ClientIdentity> {
            // Microsoft: OpenProcess grants only query-limited access to the pipe client.
            let process =
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
                    .map_err(io_error)?;
            let result = inspect_process(process, process_id);
            unsafe { CloseHandle(process) }.ok();
            result
        }
    }

    fn inspect_process(process: HANDLE, process_id: u32) -> io::Result<ClientIdentity> {
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
        let mut session_id = u32::MAX;
        // Microsoft: ProcessIdToSessionId binds the peer to the console session being served.
        unsafe { ProcessIdToSessionId(process_id, &mut session_id) }.map_err(io_error)?;
        Ok(ClientIdentity {
            sid,
            image: PathBuf::from(String::from_utf16_lossy(&path)),
            session_id,
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
                session_id: 7,
            }))
        };
        assert!(verify_client_with(
            &identity(
                "S-1-5-21-7",
                r"C:\Program Files\windowcast\windowcast-app.exe"
            ),
            4,
            "S-1-5-21-7",
            root,
            7,
        ));
        assert!(!verify_client_with(
            &identity(
                "S-1-5-21-8",
                r"C:\Program Files\windowcast\windowcast-app.exe"
            ),
            4,
            "S-1-5-21-7",
            root,
            7,
        ));
        assert!(!verify_client_with(
            &identity("S-1-5-21-7", r"C:\Users\me\windowcast-app.exe"),
            4,
            "S-1-5-21-7",
            root,
            7,
        ));
        assert!(!verify_client_with(
            &identity("S-1-5-21-7", r"C:\Program Files\windowcast\other.exe"),
            4,
            "S-1-5-21-7",
            root,
            7,
        ));
        assert!(!verify_client_with(
            &identity(
                "S-1-5-21-7",
                r"C:\Program Files\windowcast\windowcast-app.exe"
            ),
            4,
            "S-1-5-21-7",
            root,
            8,
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
            Path::new(r"C:\Program Files\windowcast"),
            7,
        ));
    }

    #[test]
    fn server_needs_system_and_the_exact_helper_path() {
        let expected =
            Path::new(r"C:\Program Files\windowcast\secure-helper\windowcast-secure-helper.exe");
        let good = Fake(Ok(ClientIdentity {
            sid: "S-1-5-18".into(),
            image: expected.into(),
            session_id: 7,
        }));
        assert!(verify_server_with(&good, 4, expected));
        let wrong_user = Fake(Ok(ClientIdentity {
            sid: "S-1-5-21-7".into(),
            image: expected.into(),
            session_id: 7,
        }));
        assert!(!verify_server_with(&wrong_user, 4, expected));
        let wrong_path = Fake(Ok(ClientIdentity {
            sid: "S-1-5-18".into(),
            image: r"C:\Users\me\windowcast-secure-helper.exe".into(),
            session_id: 7,
        }));
        assert!(!verify_server_with(&wrong_path, 4, expected));
    }
}

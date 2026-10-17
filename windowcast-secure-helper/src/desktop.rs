//! Following the active input desktop (Default or Winlogon).

pub fn active_console_session() -> u32 {
    #[cfg(windows)]
    {
        // Microsoft: WTSGetActiveConsoleSessionId returns the physical console session.
        unsafe { windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId() }
    }
    #[cfg(not(windows))]
    {
        u32::MAX
    }
}

pub fn running_in_active_console_session() -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
        use windows::Win32::System::Threading::GetCurrentProcessId;
        let mut current = u32::MAX;
        // Microsoft: ProcessIdToSessionId maps this helper process to its terminal session.
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut current) }.is_ok()
            && current == active_console_session()
            && current != u32::MAX
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// SID logged into the physical console, or `None` when the console has no user token.
pub fn active_console_user_sid() -> std::io::Result<Option<String>> {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, ERROR_NO_TOKEN, HANDLE};
        use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;

        let session = active_console_session();
        if session == u32::MAX {
            return Ok(None);
        }
        let mut token = HANDLE::default();
        // Microsoft: WTSQueryUserToken returns the primary token for the active console user.
        match unsafe { WTSQueryUserToken(session, &mut token) } {
            Ok(()) => {
                let sid = crate::permission::windows_impl::token_sid(token);
                unsafe { CloseHandle(token) }.ok();
                sid.map(Some)
            }
            Err(error) if error.code().0 as u32 == ERROR_NO_TOKEN.0 => Ok(None),
            Err(error) => Err(io_error(error)),
        }
    }
    #[cfg(not(windows))]
    {
        Ok(None)
    }
}

/// Move the fixed helper executable, with the same SYSTEM token, from Task
/// Scheduler's Session 0 into the physical console session.
pub fn launch_in_active_console_session() -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::ffi::c_void;
        use std::os::windows::ffi::OsStrExt;
        use windows::core::{PCWSTR, PWSTR};
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::Security::{
            DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary,
            TokenSessionId, TOKEN_ALL_ACCESS, TOKEN_QUERY,
        };
        use windows::Win32::System::Threading::{
            CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, CREATE_NO_WINDOW,
            PROCESS_INFORMATION, STARTUPINFOW,
        };

        let session = active_console_session();
        if session == u32::MAX {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no active console session",
            ));
        }
        let mut process_token = HANDLE::default();
        // Microsoft: OpenProcessToken reads the fixed task's SYSTEM token.
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_ALL_ACCESS,
                &mut process_token,
            )
        }
        .map_err(io_error)?;
        let mut session_token = HANDLE::default();
        // Microsoft: DuplicateTokenEx makes a primary token suitable for CreateProcessAsUser.
        let duplicated = unsafe {
            DuplicateTokenEx(
                process_token,
                TOKEN_ALL_ACCESS,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut session_token,
            )
        };
        unsafe { CloseHandle(process_token) }.ok();
        duplicated.map_err(io_error)?;
        // Microsoft: SetTokenInformation(TokenSessionId) assigns the active console session.
        let set_session = unsafe {
            SetTokenInformation(
                session_token,
                TokenSessionId,
                &session as *const u32 as *const c_void,
                std::mem::size_of::<u32>() as u32,
            )
        };
        if let Err(error) = set_session {
            unsafe { CloseHandle(session_token) }.ok();
            return Err(io_error(error));
        }

        let executable = std::env::current_exe()?;
        let executable_wide: Vec<u16> = executable
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut command: Vec<u16> = format!("\"{}\" --session-child", executable.display())
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut desktop: Vec<u16> = "winsta0\\default"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();
        // Microsoft: CreateProcessAsUser starts only this fixed, installed helper in the console.
        let created = unsafe {
            CreateProcessAsUserW(
                Some(session_token),
                PCWSTR(executable_wide.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW,
                None,
                None,
                &startup,
                &mut info,
            )
        };
        unsafe { CloseHandle(session_token) }.ok();
        created.map_err(io_error)?;
        unsafe {
            CloseHandle(info.hThread).ok();
            CloseHandle(info.hProcess).ok();
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "secure screen is not available on this platform",
        ))
    }
}

#[cfg(windows)]
fn io_error(error: windows::core::Error) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

#[cfg(windows)]
thread_local! {
    static INPUT_DESKTOP: std::cell::RefCell<Option<windows::Win32::System::StationsAndDesktops::HDESK>> = const { std::cell::RefCell::new(None) };
}

/// Attach the calling thread to the desktop currently receiving input.
pub fn sync_to_input_desktop() -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::System::StationsAndDesktops::{
            CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS,
            DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, DESKTOP_WRITEOBJECTS,
        };
        // Microsoft: OpenInputDesktop opens the desktop receiving user input without allowing
        // cross-account hooks or requesting desktop-switch permission.
        let desktop = unsafe {
            OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(DESKTOP_READOBJECTS.0 | DESKTOP_WRITEOBJECTS.0),
            )
        };
        let Ok(desktop) = desktop else { return false };
        // Microsoft: SetThreadDesktop must happen before this thread creates UI objects/hooks.
        if unsafe { SetThreadDesktop(desktop) }.is_err() {
            unsafe { CloseDesktop(desktop) }.ok();
            return false;
        }
        INPUT_DESKTOP.with(|slot| {
            if let Some(old) = slot.replace(Some(desktop)) {
                // Microsoft: CloseDesktop releases the previous, no-longer-attached desktop.
                unsafe { CloseDesktop(old) }.ok();
            }
        });
        true
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub fn input_desktop_name() -> Option<String> {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::StationsAndDesktops::{GetUserObjectInformationW, UOI_NAME};
        INPUT_DESKTOP.with(|slot| {
            let slot = slot.borrow();
            let desktop = slot.as_ref()?;
            let mut needed = 0;
            // Microsoft: GetUserObjectInformation(UOI_NAME) first reports the buffer length.
            let _ = unsafe {
                GetUserObjectInformationW(HANDLE(desktop.0), UOI_NAME, None, 0, Some(&mut needed))
            };
            if !(2..=1024).contains(&needed) {
                return None;
            }
            let mut units = vec![0u16; (needed as usize).div_ceil(2)];
            unsafe {
                GetUserObjectInformationW(
                    HANDLE(desktop.0),
                    UOI_NAME,
                    Some(units.as_mut_ptr().cast()),
                    needed,
                    Some(&mut needed),
                )
            }
            .ok()?;
            units.truncate(needed as usize / 2);
            String::from_utf16(units.split(|unit| *unit == 0).next().unwrap_or_default()).ok()
        })
    }
    #[cfg(not(windows))]
    {
        None
    }
}

pub fn winlogon_active() -> bool {
    sync_to_input_desktop()
        && input_desktop_name().is_some_and(|name| name.eq_ignore_ascii_case("winlogon"))
}

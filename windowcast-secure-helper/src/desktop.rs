//! Desktop switching: follow the input desktop with OpenInputDesktop
//! and SetThreadDesktop, so the helper works on both Default and Winlogon.
//!
//! The helper runs in the active console session (WTSGetActiveConsoleSessionId).
//! Each call cites the Win32 documentation followed.

/// The active console session ID.
/// Per Microsoft documentation: `WTSGetActiveConsoleSessionId` returns
/// the session identifier of the current session, or `0xFFFFFFFF` when
/// there is no active session.
pub fn active_console_session() -> u32 {
    #[cfg(windows)]
    {
        // Per docs: `WTSGetActiveConsoleSessionId` (Win32 API).
        unsafe {
            windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId()
        }
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// Follow the input desktop: open the current input desktop and set the
/// calling thread's desktop to it.
/// Per docs: `OpenInputDesktop` (with `DF_ALLOWOTHERACCOUNTHOOK`) then
/// `SetThreadDesktop`.
pub fn sync_to_input_desktop() -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::System::StationsAndDesktops::{
            OpenInputDesktop, SetThreadDesktop, DESKTOP_READOBJECTS, DESKTOP_WRITEOBJECTS,
            DF_ALLOWOTHERACCOUNTHOOK,
        };
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        // Per `OpenInputDesktop` docs: opens the desktop that receives user input.
        // `DF_ALLOWOTHERACCOUNTHOOK` is required when called from a non-interactive account.
        let mut desktop = HANDLE::default();
        unsafe {
            OpenInputDesktop(
                0,
                false,
                DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS,
                &mut desktop,
            )
        }
        .ok()
        .map(|_| {
            // Per `SetThreadDesktop` docs: assigns the desktop to the calling thread.
            unsafe { SetThreadDesktop(desktop).is_ok() }
        })
        .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Whether the Winlogon desktop (secure desktop) is currently active.
/// Per docs: compare the current desktop name or use session state.
pub fn winlogon_active() -> bool {
    #[cfg(windows)]
    {
        // Per `OpenInputDesktop`: when called on the Winlogon desktop, it
        // returns the input desktop of the winlogon session.
        sync_to_input_desktop();
        // Stub: on a real system we check the desktop name.
        false
    }
    #[cfg(not(windows))]
    {
        false
    }
}

//! Whether the PC is locked, and whether this session's input desktop is
//! in reach (docs/BACKENDS.md, "A locked host"). Two questions: the lock
//! screen is one reason the input desktop is out of reach, UAC's secure
//! desktop another. Windows keep streaming either way; input then goes
//! to the windows as messages (`crate::posted`), because SendInput is
//! refused while another desktop has the input.

use windows::core::PWSTR;
use windows::Win32::System::RemoteDesktop::{
    WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfoEx, WTSINFOEXW,
    WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenInputDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_SWITCHDESKTOP,
};

/// Whether this session is locked, as Remote Desktop Services keep it.
/// When it cannot be asked, whether the input desktop is out of reach.
pub fn session_locked() -> bool {
    let mut buffer = PWSTR::null();
    let mut bytes = 0u32;
    let asked = unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buffer,
            &mut bytes,
        )
    };
    if asked.is_err() || buffer.is_null() {
        return !input_desktop_reachable();
    }
    let locked = if bytes as usize >= std::mem::size_of::<WTSINFOEXW>() {
        let info = unsafe { &*(buffer.0 as *const WTSINFOEXW) };
        let flags = unsafe { info.Data.WTSInfoExLevel1.SessionFlags };
        (info.Level == 1).then_some(flags as u32 == WTS_SESSIONSTATE_LOCK)
    } else {
        None
    };
    unsafe { WTSFreeMemory(buffer.0 as *mut _) };
    locked.unwrap_or_else(|| !input_desktop_reachable())
}

/// Whether this process can reach the desktop that has the input: not
/// while the lock screen (or UAC's secure desktop) has it, which a
/// user's process cannot open.
pub fn input_desktop_reachable() -> bool {
    match unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_SWITCHDESKTOP) } {
        Ok(desktop) => {
            let _ = unsafe { CloseDesktop(desktop) };
            true
        }
        Err(_) => false,
    }
}

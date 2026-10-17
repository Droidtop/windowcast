//! The fail-closed permission gate in HKLM.

use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permission {
    pub enabled: bool,
    pub grant_sid: Option<String>,
}

impl Permission {
    /// Read the protected grant. A malformed or partial grant is denial.
    pub fn read() -> Option<Self> {
        #[cfg(windows)]
        {
            windows_impl::read().ok().flatten()
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// Write or remove the grant. HKLM makes this fail unless the caller is elevated.
    pub fn write(enabled: bool, sid: Option<&str>) -> io::Result<()> {
        #[cfg(windows)]
        {
            if enabled {
                let sid = sid.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "a granting SID is required")
                })?;
                windows_impl::write(sid)
            } else {
                windows_impl::clear()
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (enabled, sid);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure screen is not available on this platform",
            ))
        }
    }

    #[cfg(windows)]
    pub(crate) fn from_values(enabled: Option<u32>, sid: Option<String>) -> Option<Self> {
        let enabled = enabled? == 1;
        let sid = sid.filter(|value| valid_sid_text(value));
        if !enabled || sid.is_none() {
            return None;
        }
        Some(Self {
            enabled,
            grant_sid: sid,
        })
    }
}

#[cfg(windows)]
fn valid_sid_text(value: &str) -> bool {
    if value.len() > 184 {
        return false;
    }
    let mut parts = value.split('-');
    if parts.next() != Some("S") || parts.next() != Some("1") {
        return false;
    }
    let Some(authority) = parts.next() else {
        return false;
    };
    if authority.is_empty() || authority.parse::<u64>().is_err() {
        return false;
    }
    let subauthorities: Vec<_> = parts.collect();
    !subauthorities.is_empty()
        && subauthorities.len() <= 15
        && subauthorities
            .iter()
            .all(|part| !part.is_empty() && part.parse::<u32>().is_ok())
}

/// Validate a SID before crossing the elevation boundary.
pub fn validate_grant_sid(value: &str) -> bool {
    #[cfg(windows)]
    {
        valid_sid_text(value)
    }
    #[cfg(not(windows))]
    {
        let _ = value;
        false
    }
}

/// SID of the account running the management command.
pub fn current_user_sid() -> io::Result<String> {
    #[cfg(windows)]
    {
        windows_impl::current_user_sid()
    }
    #[cfg(not(windows))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows SID is not available on this platform",
        ))
    }
}

pub fn is_elevated() -> io::Result<bool> {
    #[cfg(windows)]
    {
        windows_impl::is_elevated()
    }
    #[cfg(not(windows))]
    {
        Ok(false)
    }
}

#[cfg(windows)]
pub(crate) mod windows_impl {
    use super::*;
    use std::ffi::c_void;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegGetValueW, RegSetValueExW, HKEY,
        HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
        RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub(super) fn read() -> io::Result<Option<Permission>> {
        let path = wide(crate::REG_KEY);
        let enabled_name = wide(crate::REG_VALUE_ENABLED);
        let sid_name = wide(crate::REG_VALUE_GRANT_SID);
        let mut enabled = 0u32;
        let mut enabled_size = std::mem::size_of::<u32>() as u32;
        // Microsoft: RegGetValue retrieves and type-checks a registry value.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(path.as_ptr()),
                PCWSTR(enabled_name.as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some(&mut enabled as *mut u32 as *mut c_void),
                Some(&mut enabled_size),
            )
        };
        if !status.is_ok() {
            return Ok(None);
        }

        let mut sid_size = 0u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(path.as_ptr()),
                PCWSTR(sid_name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                None,
                Some(&mut sid_size),
            )
        };
        if !status.is_ok() || !(2..=512).contains(&sid_size) {
            return Ok(None);
        }
        let mut sid_units = vec![0u16; (sid_size as usize).div_ceil(2)];
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(path.as_ptr()),
                PCWSTR(sid_name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(sid_units.as_mut_ptr().cast()),
                Some(&mut sid_size),
            )
        };
        if !status.is_ok() {
            return Ok(None);
        }
        let units = &sid_units[..sid_size as usize / 2];
        let sid =
            String::from_utf16(units.split(|unit| *unit == 0).next().unwrap_or_default()).ok();
        Ok(Permission::from_values(Some(enabled), sid))
    }

    pub(super) fn write(sid: &str) -> io::Result<()> {
        if !super::valid_sid_text(sid) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid SID"));
        }
        let path = wide(crate::REG_KEY);
        let mut key = HKEY::default();
        // Microsoft: RegCreateKeyEx creates or opens the HKLM permission key.
        win32(unsafe {
            RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(path.as_ptr()),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE,
                None,
                &mut key,
                None,
            )
        })?;
        let result = write_values(key, sid);
        // Microsoft: every registry handle returned by RegCreateKeyEx is closed.
        let _ = unsafe { RegCloseKey(key) };
        result
    }

    fn write_values(key: HKEY, sid: &str) -> io::Result<()> {
        let enabled_name = wide(crate::REG_VALUE_ENABLED);
        let enabled = 1u32.to_le_bytes();
        // Microsoft: RegSetValueEx writes the fixed DWORD grant flag.
        win32(unsafe {
            RegSetValueExW(
                key,
                PCWSTR(enabled_name.as_ptr()),
                None,
                REG_DWORD,
                Some(&enabled),
            )
        })?;
        let sid_name = wide(crate::REG_VALUE_GRANT_SID);
        let sid = wide(sid);
        let sid_bytes = unsafe {
            std::slice::from_raw_parts(
                sid.as_ptr().cast::<u8>(),
                sid.len() * std::mem::size_of::<u16>(),
            )
        };
        // Microsoft: RegSetValueEx stores the granting SID as REG_SZ.
        win32(unsafe {
            RegSetValueExW(
                key,
                PCWSTR(sid_name.as_ptr()),
                None,
                REG_SZ,
                Some(sid_bytes),
            )
        })
    }

    pub(super) fn clear() -> io::Result<()> {
        let path = wide(crate::REG_KEY);
        // Microsoft: RegDeleteKey removes the grant atomically for readers.
        let status = unsafe { RegDeleteKeyW(HKEY_LOCAL_MACHINE, PCWSTR(path.as_ptr())) };
        // ERROR_FILE_NOT_FOUND: already disabled.
        if status.0 == 2 {
            Ok(())
        } else {
            win32(status)
        }
    }

    pub(super) fn current_user_sid() -> io::Result<String> {
        let mut token = HANDLE::default();
        // Microsoft: OpenProcessToken opens the caller token for TOKEN_QUERY only.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
            .map_err(io_error)?;
        let result = token_sid(token);
        unsafe { CloseHandle(token) }.ok();
        result
    }

    pub(super) fn is_elevated() -> io::Result<bool> {
        use windows::Win32::Security::{TokenElevation, TOKEN_ELEVATION};
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
            .map_err(io_error)?;
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0;
        // Microsoft: GetTokenInformation(TokenElevation) reports whether UAC elevation is active.
        let result = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut size,
            )
        };
        unsafe { CloseHandle(token) }.ok();
        result.map_err(io_error)?;
        Ok(elevation.TokenIsElevated != 0)
    }

    pub(crate) fn token_sid(token: HANDLE) -> io::Result<String> {
        let mut size = 0u32;
        // Microsoft: first GetTokenInformation call obtains the required buffer size.
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut size) };
        if size < std::mem::size_of::<TOKEN_USER>() as u32 || size > 64 * 1024 {
            return Err(io::Error::last_os_error());
        }
        let words = (size as usize).div_ceil(std::mem::size_of::<usize>());
        let mut storage = vec![0usize; words];
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(storage.as_mut_ptr().cast()),
                size,
                &mut size,
            )
        }
        .map_err(io_error)?;
        let user = unsafe { &*(storage.as_ptr().cast::<TOKEN_USER>()) };
        let mut text = PWSTR::null();
        // Microsoft: ConvertSidToStringSid returns canonical S-1-... text.
        unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) }.map_err(io_error)?;
        let result = unsafe { text.to_string() }
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()));
        unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
        result
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn win32(status: windows::Win32::Foundation::WIN32_ERROR) -> io::Result<()> {
        if status.is_ok() {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status.0 as i32))
        }
    }

    fn io_error(error: windows::core::Error) -> io::Error {
        io::Error::other(error.to_string())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn permission_parsing_is_fail_closed() {
        assert_eq!(
            Permission::from_values(Some(1), Some("S-1-5-21-42".into())),
            Some(Permission {
                enabled: true,
                grant_sid: Some("S-1-5-21-42".into())
            })
        );
        assert_eq!(
            Permission::from_values(Some(0), Some("S-1-5-21-42".into())),
            None
        );
        assert_eq!(Permission::from_values(Some(1), None), None);
        assert_eq!(
            Permission::from_values(Some(2), Some("S-1-5-21-42".into())),
            None
        );
        assert_eq!(
            Permission::from_values(Some(1), Some("not a sid".into())),
            None
        );
        assert_eq!(
            Permission::from_values(Some(1), Some("S-1---".into())),
            None
        );
    }
}

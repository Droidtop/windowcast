//! Launches served as RemoteApps of Windows' own Remote Desktop
//! (docs/BACKENDS.md, "RemoteApp"): the host hands the client a login for
//! its Remote Desktop, pins Remote Desktop's certificate, and the client
//! runs the program there itself (`client::ClientConfig::remote_app`).
//!
//! The decisions (when it is on, which Windows user, the host-made
//! accounts and their passwords) are here for every platform; the system
//! queries and account calls are Windows only, and a host on another
//! system reports that it serves no RemoteApps.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::RwLock;

use windowcast_host::command::{Principal, RemoteAppLogin, RemoteApps};
use windowcast_protocol::HandoffTarget;

/// Whether launches may become RemoteApps. Off unless the host's owner
/// turns it on: a RemoteApp runs in a Remote Desktop session of its own,
/// which Windows licenses (one session on Windows 10 and 11, two on a
/// server without Remote Desktop Services licences), and a launch is
/// otherwise in the user's own session, on the host's desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Availability {
    /// On for Windows Server with a session free, off for Windows 10 and
    /// 11, where a Remote Desktop login takes over the console.
    Auto,
    On,
    #[default]
    Off,
}

/// Which Windows user a RemoteApp logs in as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Login {
    /// The user the client signed in as (the host's Windows account), with
    /// the password the client keeps for the session; for any other
    /// sign-in, the user the host runs as, with a password the user types.
    #[default]
    SignedInUser,
    /// A local user the host makes per windowcast account (and per
    /// PIN-paired device), with a random password it keeps under DPAPI.
    HostAccount,
}

/// The host's RemoteApp setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Setting {
    pub availability: Availability,
    pub login: Login,
}

/// What a host's settings page shows about RemoteApps here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// Windows Server rather than Windows 10 or 11.
    pub server: bool,
    /// Remote Desktop's port, `None` while Remote Desktop is off.
    pub port: Option<u16>,
    /// Another Remote Desktop session can start without ending one.
    pub session_free: bool,
    /// Windows runs any program as a RemoteApp (its allow list is off);
    /// otherwise only programs on the list run.
    pub any_program: bool,
}

impl Status {
    /// Whether `Auto` serves RemoteApps.
    pub fn auto(&self) -> bool {
        self.server && self.port.is_some() && self.session_free
    }

    /// The warning a host shows next to the setting, when there is one.
    pub fn warning(&self) -> Option<&'static str> {
        if self.port.is_none() {
            Some("Remote Desktop is off on this computer, so launches run on its own screen.")
        } else if !self.server {
            Some(
                "On Windows 10 and 11 a RemoteApp signs in to Remote Desktop, which takes over \
                 this computer's screen and locks it.",
            )
        } else if !self.session_free {
            Some("Every Remote Desktop session this server allows is in use.")
        } else {
            None
        }
    }
}

/// The host side of RemoteApp launches, for `HostControl::set_remote_apps`.
pub struct WindowsRemoteApps {
    setting: RwLock<Setting>,
    /// Where host-made accounts' protected passwords are kept.
    data_dir: PathBuf,
}

impl WindowsRemoteApps {
    pub fn new(data_dir: PathBuf, setting: Setting) -> Self {
        WindowsRemoteApps {
            setting: RwLock::new(setting),
            data_dir,
        }
    }

    pub fn set(&self, setting: Setting) {
        *self.setting.write().expect("setting") = setting;
    }

    /// This computer's RemoteApp status, or why it cannot be read.
    pub fn status() -> Result<Status, String> {
        system::status()
    }

    fn login(
        &self,
        who: &Principal,
        login: Login,
        status: &Status,
    ) -> Result<RemoteAppLogin, String> {
        let port = status
            .port
            .ok_or("Remote Desktop is off on this computer")?;
        let certificate = crate::tls::remote_desktop_certificate(port)
            .map_err(|e| format!("could not reach Remote Desktop: {e}"))?;
        let (username, password, sign_in_password) = match login {
            Login::SignedInUser => match &who.account {
                Some(account) if account.provider == "os" => {
                    (account.name.clone(), String::new(), true)
                }
                _ => (system::current_user()?, String::new(), false),
            },
            Login::HostAccount => {
                let name = host_account_name(who);
                let password = self.host_account(&name)?;
                (name, password, false)
            }
        };
        Ok(RemoteAppLogin {
            target: HandoffTarget {
                address: String::new(),
                port,
                username,
                password,
                certificate_sha256: Some(certificate),
            },
            sign_in_password,
        })
    }

    /// The password of the host-made user `name`, making the user (or
    /// giving it a new password, when the kept one is gone) as needed.
    fn host_account(&self, name: &str) -> Result<String, String> {
        let path = self.data_dir.join("remoteapp-accounts");
        let mut kept = read_kept(&path);
        if let Some(sealed) = kept.get(name) {
            if let Ok(password) = system::unprotect(sealed) {
                if system::user_exists(name)? {
                    return Ok(password);
                }
            }
        }
        let password = new_password();
        system::make_user(name, &password)?;
        kept.insert(name.to_owned(), system::protect(&password)?);
        write_kept(&path, &kept)
            .map_err(|e| format!("could not keep the account's password: {e}"))?;
        Ok(password)
    }
}

impl RemoteApps for WindowsRemoteApps {
    fn launch(&self, who: &Principal, _argv: &[String]) -> Option<Result<RemoteAppLogin, String>> {
        let setting = *self.setting.read().expect("setting");
        let status = match system::status() {
            Ok(status) => status,
            Err(e) if setting.availability == Availability::On => return Some(Err(e)),
            Err(_) => return None,
        };
        match setting.availability {
            Availability::Off => None,
            Availability::Auto if !status.auto() => None,
            _ => Some(self.login(who, setting.login, &status)),
        }
    }
}

/// The local user made for `who`: `wc-` and the account's name, or the
/// device's id for a PIN-paired device, in the characters and length
/// (20) Windows takes for a user name.
pub fn host_account_name(who: &Principal) -> String {
    let base = match &who.account {
        // Without a domain (`DOMAIN\user`) or a realm (`user@realm`).
        Some(account) => {
            let user = account.name.rsplit('\\').next().unwrap_or(&account.name);
            user.split('@').next().unwrap_or(user).to_owned()
        }
        None => {
            let peer = who.peer.to_string();
            format!("dev-{}", &peer[..peer.len().min(8)])
        }
    };
    let clean: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let mut name = format!("wc-{clean}");
    name.truncate(20);
    name
}

/// A random password Windows' complexity rules accept.
fn new_password() -> String {
    use rand_core::RngCore;
    let mut secret = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut secret);
    let hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();
    format!("Wc-{hex}-9A")
}

/// The kept passwords: one `name hex` line each, the hex DPAPI's blob.
fn read_kept(path: &std::path::Path) -> BTreeMap<String, Vec<u8>> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            let (name, hex) = line.split_once(' ')?;
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
                .collect::<Option<Vec<u8>>>()?;
            Some((name.to_owned(), bytes))
        })
        .collect()
}

fn write_kept(path: &std::path::Path, kept: &BTreeMap<String, Vec<u8>>) -> std::io::Result<()> {
    let text: String = kept
        .iter()
        .map(|(name, blob)| {
            let hex: String = blob.iter().map(|b| format!("{b:02x}")).collect();
            format!("{name} {hex}\n")
        })
        .collect();
    std::fs::write(path, text)
}

#[cfg(windows)]
mod system {
    use super::Status;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::NetworkManagement::NetManagement::{
        NetApiBufferFree, NetLocalGroupAddMembers, NetUserAdd, NetUserGetInfo, NetUserSetInfo,
        LOCALGROUP_MEMBERS_INFO_3, UF_DONT_EXPIRE_PASSWD, UF_SCRIPT, USER_INFO_1, USER_INFO_1003,
        USER_PRIV_USER,
    };
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
    };
    use windows::Win32::Security::{
        CreateWellKnownSid, LookupAccountSidW, WinBuiltinRemoteDesktopUsersSid, PSID, SID_NAME_USE,
    };
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
    };
    use windows::Win32::System::RemoteDesktop::{
        WTSActive, WTSClientProtocolType, WTSDisconnected, WTSEnumerateSessionsW, WTSFreeMemory,
        WTSQuerySessionInformationW, WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW,
    };
    use windows::Win32::System::WindowsProgramming::GetUserNameW;

    const TERMINAL_SERVER: &str = r"SYSTEM\CurrentControlSet\Control\Terminal Server";
    /// What a Windows Server without the Session Host role allows.
    const ADMIN_SESSIONS: usize = 2;
    const NERR_USER_EXISTS: u32 = 2224;
    const NERR_USER_NOT_FOUND: u32 = 2221;
    const ERROR_MEMBER_IN_ALIAS: u32 = 1378;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    fn dword(key: &str, value: &str) -> Option<u32> {
        let (key, value) = (wide(key), wide(value));
        let mut data = 0u32;
        let mut size = 4u32;
        let result = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(key.as_ptr()),
                PCWSTR(value.as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some(&mut data as *mut u32 as *mut _),
                Some(&mut size),
            )
        };
        result.is_ok().then_some(data)
    }

    fn string(key: &str, value: &str) -> Option<String> {
        let (key, value) = (wide(key), wide(value));
        let mut data = [0u16; 128];
        let mut size = (data.len() * 2) as u32;
        let result = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(key.as_ptr()),
                PCWSTR(value.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(data.as_mut_ptr() as *mut _),
                Some(&mut size),
            )
        };
        result.is_ok().then(|| {
            let end = data.iter().position(|&c| c == 0).unwrap_or(data.len());
            String::from_utf16_lossy(&data[..end])
        })
    }

    /// Remote Desktop sessions (active or disconnected) on this computer.
    fn rdp_sessions() -> usize {
        let mut info: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
        let mut count = 0u32;
        if unsafe {
            WTSEnumerateSessionsW(Some(WTS_CURRENT_SERVER_HANDLE), 0, 1, &mut info, &mut count)
        }
        .is_err()
        {
            return 0;
        }
        let sessions = unsafe { std::slice::from_raw_parts(info, count as usize) };
        let mut rdp = 0;
        for session in sessions {
            if session.State != WTSActive && session.State != WTSDisconnected {
                continue;
            }
            let mut buffer = PWSTR::null();
            let mut bytes = 0u32;
            if unsafe {
                WTSQuerySessionInformationW(
                    Some(WTS_CURRENT_SERVER_HANDLE),
                    session.SessionId,
                    WTSClientProtocolType,
                    &mut buffer,
                    &mut bytes,
                )
            }
            .is_ok()
            {
                // 2: the session is a Remote Desktop one.
                if bytes >= 2 && unsafe { *(buffer.0 as *const u16) } == 2 {
                    rdp += 1;
                }
                unsafe { WTSFreeMemory(buffer.0 as *mut _) };
            }
        }
        unsafe { WTSFreeMemory(info as *mut _) };
        rdp
    }

    pub fn status() -> Result<Status, String> {
        let server = string(
            r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            "InstallationType",
        )
        .is_some_and(|kind| kind.eq_ignore_ascii_case("Server"));
        let on = dword(TERMINAL_SERVER, "fDenyTSConnections") == Some(0);
        let port = on.then(|| {
            dword(
                &format!(r"{TERMINAL_SERVER}\WinStations\RDP-Tcp"),
                "PortNumber",
            )
            .and_then(|p| u16::try_from(p).ok())
            .unwrap_or(3389)
        });
        // TSAppCompat is 1 with the Session Host role installed.
        let session_host = dword(TERMINAL_SERVER, "TSAppCompat") == Some(1);
        let session_free = session_host || rdp_sessions() < ADMIN_SESSIONS;
        let any_program = dword(
            r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Terminal Server\TSAppAllowList",
            "fDisabledAllowList",
        ) == Some(1);
        Ok(Status {
            server,
            port,
            session_free,
            any_program,
        })
    }

    pub fn current_user() -> Result<String, String> {
        let mut buffer = [0u16; 257];
        let mut size = buffer.len() as u32;
        unsafe { GetUserNameW(Some(PWSTR(buffer.as_mut_ptr())), &mut size) }
            .map_err(|e| format!("could not read this host's user: {e}"))?;
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        Ok(String::from_utf16_lossy(&buffer[..end]))
    }

    pub fn user_exists(name: &str) -> Result<bool, String> {
        let name = wide(name);
        let mut buffer: *mut u8 = std::ptr::null_mut();
        let result =
            unsafe { NetUserGetInfo(PCWSTR::null(), PCWSTR(name.as_ptr()), 0, &mut buffer) };
        if !buffer.is_null() {
            unsafe { NetApiBufferFree(Some(buffer as *const _)) };
        }
        match result {
            0 => Ok(true),
            NERR_USER_NOT_FOUND => Ok(false),
            code => Err(format!("could not look up a Windows user (error {code})")),
        }
    }

    /// The localized name of the "Remote Desktop Users" group.
    fn remote_desktop_users() -> Result<Vec<u16>, String> {
        let mut sid = [0u8; 68];
        let mut size = sid.len() as u32;
        unsafe {
            CreateWellKnownSid(
                WinBuiltinRemoteDesktopUsersSid,
                None,
                Some(PSID(sid.as_mut_ptr() as *mut _)),
                &mut size,
            )
        }
        .map_err(|e| e.to_string())?;
        let mut name = [0u16; 256];
        let mut name_len = name.len() as u32;
        let mut domain = [0u16; 256];
        let mut domain_len = domain.len() as u32;
        let mut kind = SID_NAME_USE::default();
        unsafe {
            LookupAccountSidW(
                PCWSTR::null(),
                PSID(sid.as_mut_ptr() as *mut _),
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_len,
                Some(PWSTR(domain.as_mut_ptr())),
                &mut domain_len,
                &mut kind,
            )
        }
        .map_err(|e| e.to_string())?;
        Ok(name[..name_len as usize]
            .iter()
            .copied()
            .chain(Some(0))
            .collect())
    }

    /// Makes the local user `name` with `password` (or sets the password
    /// of the one there) and lets it use Remote Desktop.
    pub fn make_user(name: &str, password: &str) -> Result<(), String> {
        let (mut wname, mut wpassword) = (wide(name), wide(password));
        let mut comment = wide("windowcast RemoteApp user");
        let info = USER_INFO_1 {
            usri1_name: PWSTR(wname.as_mut_ptr()),
            usri1_password: PWSTR(wpassword.as_mut_ptr()),
            usri1_password_age: 0,
            usri1_priv: USER_PRIV_USER,
            usri1_home_dir: PWSTR::null(),
            usri1_comment: PWSTR(comment.as_mut_ptr()),
            usri1_flags: UF_SCRIPT | UF_DONT_EXPIRE_PASSWD,
            usri1_script_path: PWSTR::null(),
        };
        let result = unsafe {
            NetUserAdd(
                PCWSTR::null(),
                1,
                &info as *const USER_INFO_1 as *const u8,
                None,
            )
        };
        match result {
            0 => {}
            NERR_USER_EXISTS => {
                let reset = USER_INFO_1003 {
                    usri1003_password: PWSTR(wpassword.as_mut_ptr()),
                };
                let code = unsafe {
                    NetUserSetInfo(
                        PCWSTR::null(),
                        PCWSTR(wname.as_ptr()),
                        1003,
                        &reset as *const USER_INFO_1003 as *const u8,
                        None,
                    )
                };
                if code != 0 {
                    return Err(format!(
                        "could not set the password of the Windows user {name} (error {code})"
                    ));
                }
            }
            5 => return Err(
                "making a Windows user for RemoteApps needs the host to run as an administrator"
                    .into(),
            ),
            code => {
                return Err(format!(
                    "could not make the Windows user {name} (error {code})"
                ))
            }
        }
        let group = remote_desktop_users()?;
        let member = LOCALGROUP_MEMBERS_INFO_3 {
            lgrmi3_domainandname: PWSTR(wname.as_mut_ptr()),
        };
        let code = unsafe {
            NetLocalGroupAddMembers(
                PCWSTR::null(),
                PCWSTR(group.as_ptr()),
                3,
                &member as *const LOCALGROUP_MEMBERS_INFO_3 as *const u8,
                1,
            )
        };
        if code != 0 && code != ERROR_MEMBER_IN_ALIAS {
            return Err(format!(
                "could not let the Windows user {name} use Remote Desktop (error {code})"
            ));
        }
        Ok(())
    }

    pub fn protect(password: &str) -> Result<Vec<u8>, String> {
        let mut data = password.as_bytes().to_vec();
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_mut_ptr(),
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        unsafe { CryptProtectData(&input, PCWSTR::null(), None, None, None, 0, &mut output) }
            .map_err(|e| format!("could not protect the password: {e}"))?;
        let sealed =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe { LocalFree(Some(HLOCAL(output.pbData as *mut _))) };
        Ok(sealed)
    }

    pub fn unprotect(sealed: &[u8]) -> Result<String, String> {
        let mut data = sealed.to_vec();
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_mut_ptr(),
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        unsafe { CryptUnprotectData(&input, None, None, None, None, 0, &mut output) }
            .map_err(|e| e.to_string())?;
        let plain =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe { LocalFree(Some(HLOCAL(output.pbData as *mut _))) };
        String::from_utf8(plain).map_err(|e| e.to_string())
    }
}

#[cfg(not(windows))]
mod system {
    use super::Status;

    const NOT_HERE: &str = "RemoteApps need a Windows host";

    pub fn status() -> Result<Status, String> {
        Err(NOT_HERE.into())
    }
    pub fn current_user() -> Result<String, String> {
        Err(NOT_HERE.into())
    }
    pub fn user_exists(_name: &str) -> Result<bool, String> {
        Err(NOT_HERE.into())
    }
    pub fn make_user(_name: &str, _password: &str) -> Result<(), String> {
        Err(NOT_HERE.into())
    }
    pub fn protect(_password: &str) -> Result<Vec<u8>, String> {
        Err(NOT_HERE.into())
    }
    pub fn unprotect(_sealed: &[u8]) -> Result<String, String> {
        Err(NOT_HERE.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(server: bool, port: Option<u16>, session_free: bool) -> Status {
        Status {
            server,
            port,
            session_free,
            any_program: true,
        }
    }

    #[test]
    fn remoteapp_launches_are_off_until_the_owner_turns_them_on() {
        assert_eq!(Setting::default().availability, Availability::Off);
        assert_eq!(Setting::default().login, Login::SignedInUser);
    }

    #[test]
    fn auto_is_on_only_for_a_server_with_remote_desktop_and_a_free_session() {
        assert!(status(true, Some(3389), true).auto());
        assert!(!status(false, Some(3389), true).auto(), "Windows 10 or 11");
        assert!(!status(true, None, true).auto(), "Remote Desktop off");
        assert!(!status(true, Some(3389), false).auto(), "sessions in use");
        assert!(status(false, Some(3389), true)
            .warning()
            .is_some_and(|w| w.contains("locks")));
        assert_eq!(status(true, Some(3389), true).warning(), None);
    }

    #[test]
    fn host_account_names_fit_windows() {
        let peer = windowcast_identity::Identity::generate().peer_id();
        let account = |name: &str| windowcast_accounts::Account {
            name: name.into(),
            groups: Vec::new(),
            method: windowcast_accounts::Method::Password,
            provider: "local".into(),
        };
        let who = |account| Principal { peer, account };
        assert_eq!(host_account_name(&who(Some(account("Mech")))), "wc-mech");
        assert_eq!(
            host_account_name(&who(Some(account("mech@example.org")))),
            "wc-mech"
        );
        assert_eq!(
            host_account_name(&who(Some(account(r"CORP\Jane Doe")))),
            "wc-jane-doe"
        );
        let long = host_account_name(&who(Some(account("a-very-long-account-name"))));
        assert_eq!(long.len(), 20);
        let device = host_account_name(&who(None));
        assert!(
            device.starts_with("wc-dev-") && device.len() <= 20,
            "{device}"
        );
    }

    #[test]
    fn kept_passwords_read_back() {
        let dir = std::env::temp_dir().join(format!("wc-kept-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("remoteapp-accounts");
        let mut kept = BTreeMap::new();
        kept.insert("wc-mech".to_owned(), vec![1u8, 2, 255]);
        write_kept(&path, &kept).unwrap();
        assert_eq!(read_kept(&path), kept);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

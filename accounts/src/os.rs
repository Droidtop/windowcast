//! The host operating system's own accounts as a password source: PAM on
//! Linux (feature `pam`), `LogonUserW` on Windows. Both check the password
//! and that the account may log in now (PAM's account management; a
//! network logon on Windows), and give the account's groups.
//!
//! PAM's `pam_unix` reads `/etc/shadow`, so a host that is not running as
//! root can check only its own user's password through it; other PAM
//! modules (SSSD, LDAP, Kerberos) have no such limit.

use crate::{Account, CheckError, Method};

/// This machine's host name, which policy rules' `hosts` globs match.
pub fn host_name() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|name| name.trim().to_owned())
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_default()
    }
}

/// Checks `username`'s password with the OS. `Err(None)`: the OS does not
/// know the user (the next source may); `Err(Some(_))`: it decided no.
pub fn check_password(
    pam_service: &str,
    username: &str,
    password: &str,
) -> Result<Account, Option<CheckError>> {
    imp::check_password(pam_service, username, password).map(|groups| Account {
        name: username.to_owned(),
        groups,
        method: Method::Password,
        provider: "os".into(),
    })
}

#[cfg(all(target_os = "linux", feature = "pam"))]
mod imp {
    use std::ffi::{c_char, c_int, c_void, CStr, CString};

    use crate::CheckError;

    const PAM_SUCCESS: c_int = 0;
    const PAM_CONV_ERR: c_int = 19;
    const PAM_USER_UNKNOWN: c_int = 10;
    const PAM_PROMPT_ECHO_OFF: c_int = 1;
    const PAM_PROMPT_ECHO_ON: c_int = 2;
    const PAM_SILENT: c_int = 0x8000;

    #[repr(C)]
    struct PamMessage {
        msg_style: c_int,
        msg: *const c_char,
    }

    #[repr(C)]
    struct PamResponse {
        resp: *mut c_char,
        resp_retcode: c_int,
    }

    type Conversation = unsafe extern "C" fn(
        num_msg: c_int,
        msg: *mut *const PamMessage,
        resp: *mut *mut PamResponse,
        appdata_ptr: *mut c_void,
    ) -> c_int;

    #[repr(C)]
    struct PamConv {
        conv: Conversation,
        appdata_ptr: *mut c_void,
    }

    #[link(name = "pam")]
    extern "C" {
        fn pam_start(
            service_name: *const c_char,
            user: *const c_char,
            pam_conversation: *const PamConv,
            pamh: *mut *mut c_void,
        ) -> c_int;
        fn pam_authenticate(pamh: *mut c_void, flags: c_int) -> c_int;
        fn pam_acct_mgmt(pamh: *mut c_void, flags: c_int) -> c_int;
        fn pam_end(pamh: *mut c_void, pam_status: c_int) -> c_int;
        fn pam_strerror(pamh: *mut c_void, errnum: c_int) -> *const c_char;
    }

    /// The user's name and password, answering PAM's prompts: the
    /// password for prompts that do not echo, the name for those that do.
    struct Answers {
        username: CString,
        password: CString,
    }

    /// PAM frees the responses and their strings with `free`, so they are
    /// made with the C allocator.
    unsafe extern "C" fn converse(
        num_msg: c_int,
        msg: *mut *const PamMessage,
        resp: *mut *mut PamResponse,
        appdata_ptr: *mut c_void,
    ) -> c_int {
        if num_msg <= 0 || msg.is_null() || resp.is_null() || appdata_ptr.is_null() {
            return PAM_CONV_ERR;
        }
        let answers = &*(appdata_ptr as *const Answers);
        let replies =
            libc::calloc(num_msg as usize, std::mem::size_of::<PamResponse>()) as *mut PamResponse;
        if replies.is_null() {
            return PAM_CONV_ERR;
        }
        for i in 0..num_msg as usize {
            // Linux-PAM passes an array of pointers to messages.
            let message = *msg.add(i);
            if message.is_null() {
                continue;
            }
            let answer = match (*message).msg_style {
                PAM_PROMPT_ECHO_OFF => Some(&answers.password),
                PAM_PROMPT_ECHO_ON => Some(&answers.username),
                // Information and error texts need no answer.
                _ => None,
            };
            if let Some(answer) = answer {
                (*replies.add(i)).resp = libc::strdup(answer.as_ptr());
            }
        }
        *resp = replies;
        PAM_SUCCESS
    }

    pub fn check_password(
        service: &str,
        username: &str,
        password: &str,
    ) -> Result<Vec<String>, Option<CheckError>> {
        let refused = |what: &str| Some(CheckError::Refused(what.to_owned()));
        let (Ok(service), Ok(user), Ok(pass)) = (
            CString::new(service),
            CString::new(username),
            CString::new(password),
        ) else {
            return Err(refused("a name or password holds a NUL"));
        };
        let answers = Answers {
            username: user.clone(),
            password: pass,
        };
        let conversation = PamConv {
            conv: converse,
            appdata_ptr: &answers as *const Answers as *mut c_void,
        };
        let mut handle: *mut c_void = std::ptr::null_mut();
        // SAFETY: every pointer outlives the handle, which is ended below.
        unsafe {
            let status = pam_start(service.as_ptr(), user.as_ptr(), &conversation, &mut handle);
            if status != PAM_SUCCESS {
                return Err(Some(CheckError::Unavailable(format!(
                    "PAM did not start (status {status})"
                ))));
            }
            let mut status = pam_authenticate(handle, PAM_SILENT);
            if status == PAM_SUCCESS {
                status = pam_acct_mgmt(handle, PAM_SILENT);
            }
            let reason = if status == PAM_SUCCESS {
                String::new()
            } else {
                let text = pam_strerror(handle, status);
                if text.is_null() {
                    format!("PAM status {status}")
                } else {
                    CStr::from_ptr(text).to_string_lossy().into_owned()
                }
            };
            pam_end(handle, status);
            match status {
                PAM_SUCCESS => Ok(groups_of(username)),
                PAM_USER_UNKNOWN => Err(None),
                _ => Err(refused(&reason)),
            }
        }
    }

    /// The names of the user's groups, primary first.
    fn groups_of(username: &str) -> Vec<String> {
        let Ok(user) = CString::new(username) else {
            return Vec::new();
        };
        // SAFETY: getpwnam/getgrgid return static buffers read at once;
        // the group list is sized by getgrouplist itself.
        unsafe {
            let passwd = libc::getpwnam(user.as_ptr());
            if passwd.is_null() {
                return Vec::new();
            }
            let primary = (*passwd).pw_gid;
            let mut count: c_int = 32;
            let mut ids: Vec<libc::gid_t> = vec![0; count as usize];
            if libc::getgrouplist(user.as_ptr(), primary, ids.as_mut_ptr(), &mut count) < 0 {
                ids = vec![0; count.max(0) as usize];
                if libc::getgrouplist(user.as_ptr(), primary, ids.as_mut_ptr(), &mut count) < 0 {
                    return Vec::new();
                }
            }
            ids.truncate(count.max(0) as usize);
            ids.iter()
                .filter_map(|&id| {
                    let group = libc::getgrgid(id);
                    (!group.is_null()).then(|| {
                        CStr::from_ptr((*group).gr_name)
                            .to_string_lossy()
                            .into_owned()
                    })
                })
                .collect()
        }
    }
}

#[cfg(windows)]
mod imp {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{LogonUserW, LOGON32_LOGON_NETWORK, LOGON32_PROVIDER_DEFAULT};

    use crate::CheckError;

    /// `DOMAIN\user` or `user@domain` signs in to that domain; a bare name
    /// is a local account.
    pub fn check_password(
        _service: &str,
        username: &str,
        password: &str,
    ) -> Result<Vec<String>, Option<CheckError>> {
        let (domain, user) = match username.split_once('\\') {
            Some((domain, user)) => (Some(domain), user),
            None => (None, username),
        };
        let mut token = HANDLE::default();
        let domain = domain.map(HSTRING::from);
        let user = HSTRING::from(user);
        let password = HSTRING::from(password);
        // SAFETY: the strings outlive the call; the token is closed below.
        let result = unsafe {
            match &domain {
                Some(domain) => LogonUserW(
                    &user,
                    domain,
                    &password,
                    LOGON32_LOGON_NETWORK,
                    LOGON32_PROVIDER_DEFAULT,
                    &mut token,
                ),
                None => LogonUserW(
                    &user,
                    windows::core::PCWSTR::null(),
                    &password,
                    LOGON32_LOGON_NETWORK,
                    LOGON32_PROVIDER_DEFAULT,
                    &mut token,
                ),
            }
        };
        match result {
            Ok(()) => {
                // SAFETY: a token LogonUserW returned.
                unsafe {
                    let _ = CloseHandle(token);
                }
                // Group names from the token are not read yet: policy
                // reaches Windows accounts by user name.
                Ok(Vec::new())
            }
            Err(e) => Err(Some(CheckError::Refused(e.message()))),
        }
    }
}

#[cfg(not(any(windows, all(target_os = "linux", feature = "pam"))))]
mod imp {
    use crate::CheckError;

    pub fn check_password(
        _service: &str,
        _username: &str,
        _password: &str,
    ) -> Result<Vec<String>, Option<CheckError>> {
        Err(Some(CheckError::Unavailable(
            "this build cannot check the operating system's accounts".into(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against a user the CI makes on the runner, through a PAM service
    /// file of its own (`/etc/pam.d/windowcast`); run as root, as
    /// pam_unix needs to read /etc/shadow for another user's password.
    #[test]
    fn checks_an_os_account() {
        let (Ok(user), Ok(password)) = (
            std::env::var("WINDOWCAST_TEST_OS_USER"),
            std::env::var("WINDOWCAST_TEST_OS_PASSWORD"),
        ) else {
            eprintln!("WINDOWCAST_TEST_OS_USER not set; skipping");
            return;
        };
        let service = std::env::var("WINDOWCAST_TEST_PAM_SERVICE").unwrap_or("login".into());
        let account = check_password(&service, &user, &password).unwrap();
        assert_eq!(account.name, user);
        assert_eq!(account.provider, "os");
        #[cfg(target_os = "linux")]
        assert!(account.groups.contains(&user), "{:?}", account.groups);
        assert!(matches!(
            check_password(&service, &user, "not the password"),
            Err(Some(CheckError::Refused(_)))
        ));
    }
}

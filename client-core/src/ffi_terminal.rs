//! The C interface to the command stream (`include/windowcast.h`): a
//! terminal on a windowcast host or on an SSH server, drawn by the screen
//! model, and application launches. Same conventions as [`crate::ffi`]:
//! blocking calls, UTF-8 NUL-terminated strings, output buffers with their
//! capacity.

use std::ffi::c_char;
use std::time::Duration;

use windowcast_protocol::command::TerminalSize;
use windowcast_terminal::{HostKeyPolicy, Key, SshAuth, SshError, SshTarget, Terminal};

use crate::ffi::{
    str_arg, write_text, WINDOWCAST_BUFFER_TOO_SMALL, WINDOWCAST_ENDED, WINDOWCAST_ERROR,
    WINDOWCAST_TIMEOUT,
};
use crate::{Client, ClientError, ClientSession, SshSession};

pub const WINDOWCAST_SSH_PASSWORD: i32 = 0;
pub const WINDOWCAST_SSH_KEY: i32 = 1;
/// A certificate a windowcast host issued for this client's own SSH key
/// (`windowcast_client_ssh_public_key`) after an account sign-in.
pub const WINDOWCAST_SSH_CERTIFICATE: i32 = 2;

/// A server not pinned yet: pin what it presents.
pub const WINDOWCAST_HOSTKEY_FIRST_USE: i32 = 0;
/// A server not pinned yet is refused (the error then carries its
/// fingerprint, for the user to confirm).
pub const WINDOWCAST_HOSTKEY_PINNED: i32 = 1;
/// A server not pinned yet is accepted only with the given fingerprint.
pub const WINDOWCAST_HOSTKEY_FINGERPRINT: i32 = 2;

/// A terminal as the C interface holds it: the screen, and for an SSH
/// terminal the login it runs on.
pub struct WindowcastTerminal {
    terminal: Terminal,
    _login: Option<SshSession>,
}

/// Copies `bytes` into `out`, or reports the room needed.
unsafe fn write_bytes(bytes: &[u8], out: *mut u8, cap: usize, needed: *mut usize) -> i64 {
    if !needed.is_null() {
        *needed = bytes.len();
    }
    if bytes.len() > cap || out.is_null() {
        return WINDOWCAST_BUFFER_TOO_SMALL;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, bytes.len());
    bytes.len() as i64
}

fn size(cols: u16, rows: u16) -> TerminalSize {
    TerminalSize { cols, rows }.clamped()
}

/// Opens a shell on the host of `session`. Returns null on failure, with
/// the reason (a refusal from the host is worded for the user) in `error`.
///
/// # Safety
/// `session` must be valid; `error` null or valid for `error_cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_open_terminal(
    session: *const ClientSession,
    cols: u16,
    rows: u16,
    error: *mut c_char,
    error_cap: usize,
) -> *mut WindowcastTerminal {
    let Some(session) = session.as_ref() else {
        write_text("invalid arguments", error, error_cap);
        return std::ptr::null_mut();
    };
    match session.open_terminal(size(cols, rows)) {
        Ok(terminal) => Box::into_raw(Box::new(WindowcastTerminal {
            terminal,
            _login: None,
        })),
        Err(e) => {
            write_text(&e.to_string(), error, error_cap);
            std::ptr::null_mut()
        }
    }
}

/// Logs in to an SSH server and opens a shell. `auth_kind` is
/// `WINDOWCAST_SSH_PASSWORD` (`secret` is the password),
/// `WINDOWCAST_SSH_KEY` (`secret` is the private key in PEM form,
/// `passphrase` its passphrase or null) or `WINDOWCAST_SSH_CERTIFICATE`
/// (`secret` is the certificate from an `ssh_certificate` event, for this
/// client's own key; `passphrase` unused). `host_key_policy` is one of
/// `WINDOWCAST_HOSTKEY_*`; `fingerprint` is used with
/// `WINDOWCAST_HOSTKEY_FINGERPRINT`. A server whose key is not trusted
/// leaves the key it presented in `seen_fingerprint`. Returns null on
/// failure, with the reason in `error`.
///
/// # Safety
/// `client` must be valid; strings NUL-terminated (`passphrase` and
/// `fingerprint` may be null); output buffers null or valid for their
/// capacities.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn windowcast_client_ssh_terminal(
    client: *const Client,
    host: *const c_char,
    port: u16,
    user: *const c_char,
    auth_kind: i32,
    secret: *const c_char,
    passphrase: *const c_char,
    host_key_policy: i32,
    fingerprint: *const c_char,
    cols: u16,
    rows: u16,
    error: *mut c_char,
    error_cap: usize,
    seen_fingerprint: *mut c_char,
    seen_cap: usize,
) -> *mut WindowcastTerminal {
    let (Some(client), Some(host), Some(user), Some(secret)) = (
        client.as_ref(),
        str_arg(host),
        str_arg(user),
        str_arg(secret),
    ) else {
        write_text("invalid arguments", error, error_cap);
        return std::ptr::null_mut();
    };
    let auth = match auth_kind {
        WINDOWCAST_SSH_PASSWORD => SshAuth::Password(secret.to_owned()),
        WINDOWCAST_SSH_KEY => SshAuth::Key {
            pem: secret.to_owned(),
            passphrase: str_arg(passphrase).map(str::to_owned),
        },
        WINDOWCAST_SSH_CERTIFICATE => match client.ssh_certificate_auth(secret) {
            Ok(auth) => auth,
            Err(e) => {
                write_text(&e.to_string(), error, error_cap);
                return std::ptr::null_mut();
            }
        },
        _ => {
            write_text("unknown login method", error, error_cap);
            return std::ptr::null_mut();
        }
    };
    let policy = match (host_key_policy, str_arg(fingerprint)) {
        (WINDOWCAST_HOSTKEY_FIRST_USE, _) => HostKeyPolicy::TrustOnFirstUse,
        (WINDOWCAST_HOSTKEY_PINNED, _) => HostKeyPolicy::Pinned,
        (WINDOWCAST_HOSTKEY_FINGERPRINT, Some(fingerprint)) => {
            HostKeyPolicy::Fingerprint(fingerprint.to_owned())
        }
        _ => {
            write_text("unknown host key policy", error, error_cap);
            return std::ptr::null_mut();
        }
    };
    let target = SshTarget {
        host: host.to_owned(),
        port,
        user: user.to_owned(),
    };
    let login = match client.ssh_connect(&target, &auth, policy) {
        Ok(login) => login,
        Err(e) => {
            if let ClientError::Ssh(SshError::HostKeyRefused { fingerprint, .. }) = &e {
                write_text(fingerprint, seen_fingerprint, seen_cap);
            }
            write_text(&e.to_string(), error, error_cap);
            return std::ptr::null_mut();
        }
    };
    match login.open_terminal(size(cols, rows)) {
        Ok(terminal) => Box::into_raw(Box::new(WindowcastTerminal {
            terminal,
            _login: Some(login),
        })),
        Err(e) => {
            write_text(&e.to_string(), error, error_cap);
            std::ptr::null_mut()
        }
    }
}

/// Ends the terminal's shell and frees it.
///
/// # Safety
/// `terminal` must come from one of the open calls and not be in use by
/// another thread.
#[no_mangle]
pub unsafe extern "C" fn windowcast_terminal_free(terminal: *mut WindowcastTerminal) {
    if !terminal.is_null() {
        drop(Box::from_raw(terminal));
    }
}

/// Types text. Returns 0 or `WINDOWCAST_ERROR`.
///
/// # Safety
/// `terminal` valid; `text` NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn windowcast_terminal_send_text(
    terminal: *const WindowcastTerminal,
    text: *const c_char,
) -> i64 {
    match (terminal.as_ref(), str_arg(text)) {
        (Some(t), Some(text)) => {
            t.terminal.send_text(text);
            0
        }
        _ => WINDOWCAST_ERROR,
    }
}

/// Types a key by name: `Enter`, `Backspace`, `Tab`, `Escape`, `Up`,
/// `Down`, `Left`, `Right`, `Home`, `End`, `PageUp`, `PageDown`, `Insert`,
/// `Delete`, `F1` to `F12`. Returns 0 or `WINDOWCAST_ERROR`.
///
/// # Safety
/// `terminal` valid; `name` NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn windowcast_terminal_send_key(
    terminal: *const WindowcastTerminal,
    name: *const c_char,
) -> i64 {
    match (terminal.as_ref(), str_arg(name).and_then(Key::from_name)) {
        (Some(t), Some(key)) => {
            t.terminal.send_key(key);
            0
        }
        _ => WINDOWCAST_ERROR,
    }
}

/// Types Ctrl and a letter (or one of `@[\]^_`), given as a code point.
///
/// # Safety
/// `terminal` valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_terminal_send_control(
    terminal: *const WindowcastTerminal,
    code_point: u32,
) -> i64 {
    match (terminal.as_ref(), char::from_u32(code_point)) {
        (Some(t), Some(c)) => {
            t.terminal.send_control(c);
            0
        }
        _ => WINDOWCAST_ERROR,
    }
}

/// Pastes text (bracketed when the program asked for it).
///
/// # Safety
/// `terminal` valid; `text` NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn windowcast_terminal_paste(
    terminal: *const WindowcastTerminal,
    text: *const c_char,
) -> i64 {
    match (terminal.as_ref(), str_arg(text)) {
        (Some(t), Some(text)) => {
            t.terminal.paste(text);
            0
        }
        _ => WINDOWCAST_ERROR,
    }
}

/// The view changed size, in character cells.
///
/// # Safety
/// `terminal` valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_terminal_resize(
    terminal: *const WindowcastTerminal,
    cols: u16,
    rows: u16,
) -> i64 {
    match terminal.as_ref() {
        Some(t) => {
            t.terminal.resize(size(cols, rows));
            0
        }
        None => WINDOWCAST_ERROR,
    }
}

/// Scrolls the view back `lines` from the live screen (0 returns to it).
///
/// # Safety
/// `terminal` valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_terminal_scroll(
    terminal: *const WindowcastTerminal,
    lines: u32,
) -> i64 {
    match terminal.as_ref() {
        Some(t) => {
            t.terminal.scroll_to(lines as usize);
            0
        }
        None => WINDOWCAST_ERROR,
    }
}

/// Waits up to `timeout_ms` for the screen to change since `seen` (the
/// `version` of the last snapshot; 0 for "anything"). Returns
/// `WINDOWCAST_TIMEOUT` or 1 when it changed.
///
/// # Safety
/// `terminal` valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_terminal_wait(
    terminal: *const WindowcastTerminal,
    seen: u64,
    timeout_ms: u32,
) -> i64 {
    match terminal.as_ref() {
        Some(t) => {
            if t.terminal
                .wait_change(seen, Duration::from_millis(u64::from(timeout_ms)))
            {
                1
            } else {
                WINDOWCAST_TIMEOUT
            }
        }
        None => WINDOWCAST_ERROR,
    }
}

/// The screen as JSON: `{"cols","rows","lines":[[{"text","fg","bg",
/// "bold","italic","underline","inverse"}...]...],"cursor":[row,col]|null,
/// "alternate_screen","scrollback","version"}`. `fg` and `bg` are
/// `0xRRGGBB` or null for the viewer's default. Returns the length, or
/// `WINDOWCAST_BUFFER_TOO_SMALL` with the room needed in `needed`.
///
/// # Safety
/// `terminal` valid; `out` valid for `cap` bytes; `needed` null or valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_terminal_snapshot(
    terminal: *const WindowcastTerminal,
    out: *mut u8,
    cap: usize,
    needed: *mut usize,
) -> i64 {
    let Some(t) = terminal.as_ref() else {
        return WINDOWCAST_ERROR;
    };
    match serde_json::to_vec(&t.terminal.snapshot()) {
        Ok(json) => write_bytes(&json, out, cap, needed),
        Err(_) => WINDOWCAST_ERROR,
    }
}

/// The texts programs put on the clipboard since the last call, as a JSON
/// array of strings (`[]` for none).
///
/// # Safety
/// As [`windowcast_terminal_snapshot`].
#[no_mangle]
pub unsafe extern "C" fn windowcast_terminal_take_clipboard(
    terminal: *const WindowcastTerminal,
    out: *mut u8,
    cap: usize,
    needed: *mut usize,
) -> i64 {
    let Some(t) = terminal.as_ref() else {
        return WINDOWCAST_ERROR;
    };
    match serde_json::to_vec(&t.terminal.take_clipboard()) {
        Ok(json) => write_bytes(&json, out, cap, needed),
        Err(_) => WINDOWCAST_ERROR,
    }
}

/// `WINDOWCAST_TIMEOUT` (0) while the shell runs; `WINDOWCAST_ENDED` once it
/// ended, with its exit code in `code` (-1 when there is none).
///
/// # Safety
/// `terminal` valid; `code` null or valid.
#[no_mangle]
pub unsafe extern "C" fn windowcast_terminal_ended(
    terminal: *const WindowcastTerminal,
    code: *mut i32,
) -> i64 {
    let Some(t) = terminal.as_ref() else {
        return WINDOWCAST_ERROR;
    };
    match t.terminal.ended() {
        None => WINDOWCAST_TIMEOUT,
        Some(exit) => {
            if let Some(code) = code.as_mut() {
                *code = exit.unwrap_or(-1);
            }
            WINDOWCAST_ENDED
        }
    }
}

/// Starts an application on the host of `session`: `argv_json` is a JSON
/// array of strings (the program and its arguments). The windows it makes
/// arrive in the window list. Returns the process id (0 when the host does
/// not know it) or `WINDOWCAST_ERROR`, with the host's reason in `error`.
/// A launch the host runs as a RemoteApp may return
/// `WINDOWCAST_PASSWORD_NEEDED` with the Windows user name in `error`:
/// ask the user and call [`windowcast_session_launch_with_password`].
///
/// # Safety
/// `session` valid; `argv_json` NUL-terminated; `error` null or valid for
/// `error_cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_launch(
    session: *const ClientSession,
    argv_json: *const c_char,
    error: *mut c_char,
    error_cap: usize,
) -> i64 {
    windowcast_session_launch_with_password(session, argv_json, std::ptr::null(), error, error_cap)
}

/// [`windowcast_session_launch`] with the Windows password the user typed
/// for a RemoteApp (`password` may be null).
///
/// # Safety
/// As [`windowcast_session_launch`]; `password` null or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn windowcast_session_launch_with_password(
    session: *const ClientSession,
    argv_json: *const c_char,
    password: *const c_char,
    error: *mut c_char,
    error_cap: usize,
) -> i64 {
    let (Some(session), Some(json)) = (session.as_ref(), str_arg(argv_json)) else {
        write_text("invalid arguments", error, error_cap);
        return WINDOWCAST_ERROR;
    };
    let Ok(argv) = serde_json::from_str::<Vec<String>>(json) else {
        write_text(
            "the command line is not a JSON array of strings",
            error,
            error_cap,
        );
        return WINDOWCAST_ERROR;
    };
    match session.launch_with_password(argv, str_arg(password)) {
        Ok(pid) => i64::from(pid.unwrap_or(0)),
        Err(crate::ClientError::PasswordNeeded(user)) => {
            write_text(&user, error, error_cap);
            crate::ffi::WINDOWCAST_PASSWORD_NEEDED
        }
        Err(e) => {
            write_text(&e.to_string(), error, error_cap);
            WINDOWCAST_ERROR
        }
    }
}

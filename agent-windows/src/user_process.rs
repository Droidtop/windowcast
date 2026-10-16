//! What a client starts runs as the user signed in at the PC, never as
//! SYSTEM (docs/BACKENDS.md, "The secure screen"). A host running as a
//! service is SYSTEM in the console session; its terminals, commands and
//! launched programs start with the signed-in user's token
//! (WTSQueryUserToken, as Sunshine's `retrieve_users_token`), that user's
//! environment (CreateEnvironmentBlock) and profile folder, through
//! CreateProcessAsUserW. Terminals start the relay
//! (`windowcast-agent-windows pty-relay`, `crate::relay`) that way, which
//! opens the pseudo-terminal as the user.

use std::fs::File;
use std::io::Write;
use std::os::windows::io::FromRawHandle;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use windowcast_host::command::{ProcessStarter, RunningProcess, Started};
use windowcast_protocol::command::TerminalSize;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, SetHandleInformation, ERROR_NO_TOKEN, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT,
};
use windows::Win32::Security::{
    CheckTokenMembership, CreateRestrictedToken, CreateWellKnownSid, WinLocalSystemSid,
    CREATE_RESTRICTED_TOKEN_FLAGS, PSID, SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE,
    TOKEN_ALL_ACCESS,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, DeleteProcThreadAttributeList, GetCurrentProcess, GetExitCodeProcess,
    InitializeProcThreadAttributeList, OpenProcessToken, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};
use windows::Win32::UI::Shell::GetUserProfileDirectoryW;

use crate::cmdline;
use crate::relay::{self, Frame};

/// What a client is told when nobody is signed in at the PC.
pub const NOBODY_SIGNED_IN: &str = "Nobody is signed in at this PC, so there is nobody to run \
     this as. Sign in at the PC, then try again.";

/// Whether this process runs as SYSTEM (Sunshine's
/// `is_running_as_system`).
pub fn running_as_system() -> bool {
    let mut sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut size = sid.len() as u32;
    let psid = PSID(sid.as_mut_ptr() as *mut _);
    if unsafe { CreateWellKnownSid(WinLocalSystemSid, None, Some(psid), &mut size) }.is_err() {
        return false;
    }
    let mut member = windows::core::BOOL(0);
    unsafe { CheckTokenMembership(None, psid, &mut member) }.is_ok() && member.as_bool()
}

/// A handle closed when dropped.
struct Owned(HANDLE);

// Kernel handles may be used from any thread.
unsafe impl Send for Owned {}
unsafe impl Sync for Owned {}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// The token of the user signed in at the PC's console. Only SYSTEM may
/// ask for it.
fn console_user_token() -> Result<Owned, String> {
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    if session == u32::MAX {
        return Err(NOBODY_SIGNED_IN.into());
    }
    let mut token = HANDLE::default();
    match unsafe { WTSQueryUserToken(session, &mut token) } {
        Ok(()) => Ok(Owned(token)),
        Err(e) if e.code() == ERROR_NO_TOKEN.to_hresult() => Err(NOBODY_SIGNED_IN.into()),
        Err(e) => Err(format!(
            "could not get the signed-in user's token to run this as them: {e}"
        )),
    }
}

/// This process's own token, restricted in nothing: a token derived from
/// the caller's needs no special privilege to start a process with, so
/// the same path can be tested by a process that is not SYSTEM.
fn own_token() -> Result<Owned, String> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut token) }
        .map_err(|e| format!("could not open this process's token: {e}"))?;
    let token = Owned(token);
    let mut copy = HANDLE::default();
    unsafe {
        CreateRestrictedToken(
            token.0,
            CREATE_RESTRICTED_TOKEN_FLAGS(0),
            None,
            None,
            None,
            &mut copy,
        )
    }
    .map_err(|e| format!("could not copy this process's token: {e}"))?;
    Ok(Owned(copy))
}

/// Starts what clients ask for with a user's token instead of the host's.
pub struct UserProcesses {
    token: fn() -> Result<Owned, String>,
    /// The agent's executable, which runs the terminal relay.
    relay: PathBuf,
}

impl UserProcesses {
    /// As the user signed in at the console: what a host running as SYSTEM
    /// installs.
    pub fn console_user(relay: PathBuf) -> Self {
        UserProcesses {
            token: console_user_token,
            relay,
        }
    }

    /// As this process's own user, through the same CreateProcessAsUserW
    /// path (tests).
    pub fn own_user(relay: PathBuf) -> Self {
        UserProcesses {
            token: own_token,
            relay,
        }
    }

    fn start(
        &self,
        argv: &[String],
        env: &[(String, String)],
        piped: bool,
    ) -> Result<Child, String> {
        let token = (self.token)()?;
        let block = cmdline::env_block(user_environment(&token)?, env);
        let profile = profile_dir(&token);
        start_as(
            &token,
            &cmdline::join(argv),
            &block,
            profile.as_deref(),
            piped,
        )
    }
}

impl ProcessStarter for UserProcesses {
    fn terminal(&self, spawn: &windowcast_pty::Spawn) -> Result<Started, String> {
        let mut argv = vec![self.relay.to_string_lossy().into_owned()];
        argv.extend(relay::args(spawn));
        let child = self.start(&argv, &spawn.env, true)?;
        Ok(child.started(true))
    }

    fn piped(&self, argv: &[String], env: &[(String, String)]) -> Result<Started, String> {
        if argv.is_empty() {
            return Err("no command".into());
        }
        Ok(self.start(argv, env, true)?.started(false))
    }

    fn detached(&self, argv: &[String], env: &[(String, String)]) -> Result<Option<u32>, String> {
        if argv.is_empty() {
            return Err("no application".into());
        }
        Ok(Some(self.start(argv, env, false)?.pid))
    }
}

/// The user's own variables, as Windows would give a program they start.
fn user_environment(token: &Owned) -> Result<Vec<(String, String)>, String> {
    let mut block: *mut core::ffi::c_void = std::ptr::null_mut();
    unsafe { CreateEnvironmentBlock(&mut block, Some(token.0), false) }
        .map_err(|e| format!("could not read the user's environment: {e}"))?;
    let mut len = 0usize;
    let units = block as *const u16;
    // The block ends with two NULs in a row.
    unsafe {
        while !(*units.add(len) == 0 && *units.add(len + 1) == 0) {
            len += 1;
        }
    }
    let vars = cmdline::parse_env_block(unsafe { std::slice::from_raw_parts(units, len + 2) });
    let _ = unsafe { DestroyEnvironmentBlock(block) };
    Ok(vars)
}

fn profile_dir(token: &Owned) -> Option<Vec<u16>> {
    let mut size = 0u32;
    let _ = unsafe { GetUserProfileDirectoryW(token.0, None, &mut size) };
    let mut dir = vec![0u16; size as usize];
    unsafe { GetUserProfileDirectoryW(token.0, Some(PWSTR(dir.as_mut_ptr())), &mut size) }.ok()?;
    Some(dir)
}

struct Child {
    process: Arc<Owned>,
    pid: u32,
    /// Standard input, output and error, when piped.
    pipes: Option<(File, File, File)>,
}

impl Child {
    fn started(self, framed: bool) -> Started {
        let (stdin, stdout, stderr) = self.pipes.expect("piped");
        Started {
            process: Arc::new(UserProcess {
                process: self.process,
                stdin: Mutex::new(Some(stdin)),
                framed,
            }),
            output: vec![Box::new(stdout), Box::new(stderr)],
            pid: Some(self.pid),
        }
    }
}

/// An inheritable pipe; the end that stays with the host is not.
fn pipe(child_reads: bool) -> Result<(Owned, Owned), String> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
    unsafe { CreatePipe(&mut read, &mut write, Some(&attributes), 0) }
        .map_err(|e| format!("could not make a pipe: {e}"))?;
    let (read, write) = (Owned(read), Owned(write));
    let (ours, theirs) = if child_reads {
        (write, read)
    } else {
        (read, write)
    };
    unsafe { SetHandleInformation(ours.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) }
        .map_err(|e| format!("could not make a pipe: {e}"))?;
    Ok((ours, theirs))
}

fn into_file(handle: Owned) -> File {
    let raw = handle.0 .0;
    std::mem::forget(handle);
    unsafe { File::from_raw_handle(raw) }
}

/// CreateProcessAsUserW on the user's interactive desktop. Piped: its
/// standard handles are pipes, and it inherits those three handles only
/// (PROC_THREAD_ATTRIBUTE_HANDLE_LIST), whatever else the host has open.
fn start_as(
    token: &Owned,
    command_line: &str,
    env: &[u16],
    cwd: Option<&[u16]>,
    piped: bool,
) -> Result<Child, String> {
    let mut line: Vec<u16> = command_line.encode_utf16().chain(Some(0)).collect();
    let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain(Some(0)).collect();
    let mut info = STARTUPINFOEXW::default();
    info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    info.StartupInfo.lpDesktop = PWSTR(desktop.as_mut_ptr());

    let pipes = if piped {
        let (stdin, child_stdin) = pipe(true)?;
        let (stdout, child_stdout) = pipe(false)?;
        let (stderr, child_stderr) = pipe(false)?;
        Some((
            stdin,
            stdout,
            stderr,
            [child_stdin, child_stdout, child_stderr],
        ))
    } else {
        None
    };
    let inherited: Vec<HANDLE> = pipes
        .as_ref()
        .map(|p| p.3.iter().map(|h| h.0).collect())
        .unwrap_or_default();
    let mut list_storage = Vec::<u8>::new();
    let mut list = LPPROC_THREAD_ATTRIBUTE_LIST::default();
    if let Some((_, _, _, child)) = &pipes {
        info.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
        info.StartupInfo.hStdInput = child[0].0;
        info.StartupInfo.hStdOutput = child[1].0;
        info.StartupInfo.hStdError = child[2].0;
        let mut size = 0usize;
        let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut size) };
        list_storage.resize(size, 0);
        list = LPPROC_THREAD_ATTRIBUTE_LIST(list_storage.as_mut_ptr() as *mut _);
        unsafe { InitializeProcThreadAttributeList(Some(list), 1, None, &mut size) }
            .map_err(|e| format!("could not start it: {e}"))?;
        let updated = unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(inherited.as_ptr() as *const _),
                inherited.len() * std::mem::size_of::<HANDLE>(),
                None,
                None,
            )
        };
        if let Err(e) = updated {
            unsafe { DeleteProcThreadAttributeList(list) };
            return Err(format!("could not start it: {e}"));
        }
        info.lpAttributeList = list;
    }

    let mut flags = CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
    if piped {
        // A command's output goes to the client, not to a console window
        // on the PC.
        flags |= CREATE_NO_WINDOW;
    }
    let mut process = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessAsUserW(
            Some(token.0),
            PCWSTR::null(),
            Some(PWSTR(line.as_mut_ptr())),
            None,
            None,
            piped,
            flags,
            Some(env.as_ptr() as *const _),
            cwd.map_or(PCWSTR::null(), |dir| PCWSTR(dir.as_ptr())),
            &info.StartupInfo as *const STARTUPINFOW,
            &mut process,
        )
    };
    if !list.0.is_null() {
        unsafe { DeleteProcThreadAttributeList(list) };
    }
    created.map_err(|e| {
        let program = command_line.split(' ').next().unwrap_or_default();
        format!("could not start {program}: {e}")
    })?;
    let _ = unsafe { CloseHandle(process.hThread) };
    // The child's ends close with `pipes` here; the child has its own.
    Ok(Child {
        process: Arc::new(Owned(process.hProcess)),
        pid: process.dwProcessId,
        pipes: pipes.map(|(stdin, stdout, stderr, _)| {
            (into_file(stdin), into_file(stdout), into_file(stderr))
        }),
    })
}

/// A command or terminal relay started as the user.
struct UserProcess {
    process: Arc<Owned>,
    stdin: Mutex<Option<File>>,
    /// A terminal relay, which reads [`Frame`]s instead of plain input.
    framed: bool,
}

impl UserProcess {
    fn send(&self, bytes: &[u8]) -> std::io::Result<()> {
        match self.stdin.lock().expect("stdin").as_mut() {
            Some(stdin) => stdin.write_all(bytes).and_then(|()| stdin.flush()),
            None => Ok(()),
        }
    }
}

impl RunningProcess for UserProcess {
    fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        if self.framed {
            self.send(&relay::encode(&Frame::Data(bytes.to_vec())))
        } else {
            self.send(bytes)
        }
    }
    fn eof(&self) {
        if self.framed {
            // A terminal's end of input is the terminal's own ^D.
            let _ = self.write(&[4]);
        } else {
            self.stdin.lock().expect("stdin").take();
        }
    }
    fn resize(&self, size: TerminalSize) {
        if self.framed {
            let size = size.clamped();
            let _ = self.send(&relay::encode(&Frame::Resize(windowcast_pty::Size {
                cols: size.cols,
                rows: size.rows,
            })));
        }
    }
    fn kill(&self) {
        let _ = unsafe { TerminateProcess(self.process.0, 1) };
    }
    fn wait(&self) -> Option<i32> {
        unsafe { WaitForSingleObject(self.process.0, INFINITE) };
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(self.process.0, &mut code) }.ok()?;
        Some(code as i32)
    }
}

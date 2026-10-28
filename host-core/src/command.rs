//! The host's end of the command stream (docs/COMMAND-STREAM.md): shells,
//! commands and application launches that a client opens as channels of
//! its session. Everything a channel does passes [`CommandAuthorizer`]
//! first. The host's own check (by default [`PairedDevices`]; an
//! application replaces it with [`crate::HostControl::set_command_authorizer`])
//! decides alone while account sign-in is off; while it is on, the
//! account policy ([`AccountPolicy`]) is asked first.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use windowcast_accounts::Account;
use windowcast_accounts::Accounts;
use windowcast_identity::PeerId;
use windowcast_protocol::command::{
    data_messages, ChannelId, ChannelKind, CommandMessage, TerminalSize, MAX_CHANNELS, MAX_CHUNK,
};
use windowcast_protocol::ControlMessage;
use windowcast_transport::{Session, TransportError};

/// Who is asking: the device, and the account it acts for when it signed
/// in with one (docs/ACCOUNTS.md); `None` for a device paired by PIN.
#[derive(Debug, Clone)]
pub struct Principal {
    pub peer: PeerId,
    pub account: Option<Account>,
}

/// Why a RemoteApp launch did not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAppRefusal {
    /// Its login needs this Windows user's password, which the client did
    /// not give (or gave wrong).
    PasswordNeeded(String),
    /// Anything else, for the user.
    Refused(String),
}

/// Serves launches as RemoteApps of the host's Remote Desktop, run by the
/// host itself (docs/BACKENDS.md, "RemoteApp"; Windows hosts,
/// `windowcast_rdp::remoteapp_host`): the program's windows join the
/// host's window list. Called off the session's task: it may take a while.
pub trait RemoteApps: Send + Sync + 'static {
    /// Runs `argv` as a RemoteApp for `who`, with the Windows `password`
    /// the client gave; `Ok` once Windows started it. `None` when this
    /// host does not serve it as one now (the launch then runs in the
    /// host's own session).
    fn launch(
        &self,
        who: &Principal,
        argv: &[String],
        password: Option<&str>,
    ) -> Option<Result<(), RemoteAppRefusal>>;
}

/// The one check every channel passes. An `Err` is the reason shown to the
/// user.
pub trait CommandAuthorizer: Send + Sync + 'static {
    fn authorize(&self, who: &Principal, kind: &ChannelKind) -> Result<(), String>;
}

/// The host's own default: a paired device may open any kind of channel,
/// as the owner asked ("authorized by the existing pairing"). A device an
/// account sign-in registered passes it too; the account policy in front
/// of it ([`AccountPolicy`]) is what narrows accounts.
pub struct PairedDevices;

impl CommandAuthorizer for PairedDevices {
    fn authorize(&self, _who: &Principal, _kind: &ChannelKind) -> Result<(), String> {
        Ok(())
    }
}

/// Refuses every channel: a host that offers no commands.
pub struct NoCommands;

impl CommandAuthorizer for NoCommands {
    fn authorize(&self, _who: &Principal, _kind: &ChannelKind) -> Result<(), String> {
        Err("this host does not allow commands".into())
    }
}

/// The check while account sign-in is on: the host's policy for the
/// principal's account (a PIN-paired device is `method:pin` to it) decides
/// when its matching rule says `commands`, `false` refusing and `true`
/// admitting; a rule that does not say leaves it to the host's own check
/// (`then`). Policy is read at every open.
pub struct AccountPolicy {
    accounts: Arc<Accounts>,
    then: Arc<dyn CommandAuthorizer>,
}

impl AccountPolicy {
    pub fn new(accounts: Arc<Accounts>, then: Arc<dyn CommandAuthorizer>) -> Self {
        AccountPolicy { accounts, then }
    }
}

impl CommandAuthorizer for AccountPolicy {
    fn authorize(&self, who: &Principal, kind: &ChannelKind) -> Result<(), String> {
        let decision = self.accounts.decide(who.account.as_ref());
        if !decision.allow {
            return Err("policy does not admit this account".into());
        }
        match decision.commands {
            Some(false) => Err("policy does not allow commands for this account".into()),
            Some(true) => Ok(()),
            None => self.then.authorize(who, kind),
        }
    }
}

/// A channel open now, as a host application shows it.
#[derive(Debug, Clone)]
pub struct CommandStatus {
    pub peer: PeerId,
    pub channel: ChannelId,
    /// "shell", or the command's `argv[0]`.
    pub what: String,
    pub since: Instant,
}

pub(crate) type StatusMap = Arc<Mutex<HashMap<u64, CommandStatus>>>;

/// What a running command offers its channel.
pub trait RunningProcess: Send + Sync {
    fn write(&self, bytes: &[u8]) -> std::io::Result<()>;
    fn eof(&self);
    fn resize(&self, _size: TerminalSize) {}
    fn kill(&self);
    /// Blocks until the command has ended.
    fn wait(&self) -> Option<i32>;
}

struct PtyProcess(windowcast_pty::Pty);

impl RunningProcess for PtyProcess {
    fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.0.write(bytes)
    }
    fn eof(&self) {
        // A terminal's end of input is the terminal's own ^D.
        let _ = self.0.write(&[4]);
    }
    fn resize(&self, size: TerminalSize) {
        let _ = self.0.resize(pty_size(size));
    }
    fn kill(&self) {
        self.0.kill();
    }
    fn wait(&self) -> Option<i32> {
        self.0.wait()
    }
}

fn pty_size(size: TerminalSize) -> windowcast_pty::Size {
    let size = size.clamped();
    windowcast_pty::Size {
        cols: size.cols,
        rows: size.rows,
    }
}

struct PipedProcess {
    child: Mutex<Child>,
    stdin: Mutex<Option<std::process::ChildStdin>>,
}

impl RunningProcess for PipedProcess {
    fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        match self.stdin.lock().expect("stdin").as_mut() {
            Some(stdin) => stdin.write_all(bytes).and_then(|()| stdin.flush()),
            None => Ok(()),
        }
    }
    fn eof(&self) {
        self.stdin.lock().expect("stdin").take();
    }
    fn kill(&self) {
        let _ = self.child.lock().expect("child").kill();
    }
    fn wait(&self) -> Option<i32> {
        loop {
            if let Ok(Some(status)) = self.child.lock().expect("child").try_wait() {
                return status.code();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// One open channel of a session.
struct Channel {
    process: Arc<dyn RunningProcess>,
    /// Input goes in order, off the session's task: a command that does
    /// not read must not hold up the control channel.
    input: std::sync::mpsc::Sender<Option<Vec<u8>>>,
    /// Set when the client closed the channel, so no `Exited` follows.
    closed: Arc<AtomicBool>,
    serial: u64,
}

/// What the host application has turned on beyond commands: shared folders and chat.
pub(crate) struct Services {
    pub(crate) shares: Arc<crate::files::Shares>,
    pub(crate) chat: Option<Arc<crate::chat::ChatHub>>,
}

/// A session's channels. Dropping it ends every command still running.
pub(crate) struct Channels {
    session: Arc<Session>,
    principal: Principal,
    authorizer: Arc<dyn CommandAuthorizer>,
    remote_apps: Option<Arc<dyn RemoteApps>>,
    status: StatusMap,
    serial: Arc<AtomicU64>,
    services: Services,
    open: HashMap<ChannelId, Channel>,
}

impl Channels {
    pub(crate) fn new(
        session: Arc<Session>,
        principal: Principal,
        authorizer: Arc<dyn CommandAuthorizer>,
        remote_apps: Option<Arc<dyn RemoteApps>>,
        status: StatusMap,
        serial: Arc<AtomicU64>,
        services: Services,
    ) -> Self {
        Channels {
            session,
            principal,
            authorizer,
            remote_apps,
            status,
            serial,
            services,
            open: HashMap::new(),
        }
    }

    pub(crate) async fn handle(&mut self, message: CommandMessage) -> Result<(), TransportError> {
        match message {
            CommandMessage::Open { id, kind } => {
                if let Some(answer) = self.remote_app(&kind).await {
                    match answer {
                        Ok(()) => {
                            self.send(CommandMessage::Opened { id, pid: None }).await?;
                            self.send(CommandMessage::Exited { id, code: None }).await?;
                        }
                        Err(RemoteAppRefusal::PasswordNeeded(username)) => {
                            self.send(CommandMessage::PasswordNeeded { id, username })
                                .await?
                        }
                        Err(RemoteAppRefusal::Refused(reason)) => {
                            self.send(CommandMessage::Refused { id, reason }).await?
                        }
                    }
                    return Ok(());
                }
                let launch = matches!(kind, ChannelKind::Launch { .. });
                match self.start(id, kind).await {
                    Ok(pid) => {
                        self.send(CommandMessage::Opened { id, pid }).await?;
                        if launch {
                            // Started and done: the stream has nothing more.
                            self.send(CommandMessage::Exited { id, code: None }).await?;
                        }
                    }
                    Err(reason) => self.send(CommandMessage::Refused { id, reason }).await?,
                }
            }
            CommandMessage::Data { id, bytes } => {
                if let Some(channel) = self.open.get(&id) {
                    let _ = channel.input.send(Some(bytes));
                }
            }
            CommandMessage::Eof { id } => {
                if let Some(channel) = self.open.get(&id) {
                    let _ = channel.input.send(None);
                }
            }
            CommandMessage::Resize { id, size } => {
                if let Some(channel) = self.open.get(&id) {
                    channel.process.resize(size);
                }
            }
            CommandMessage::Close { id } => {
                if let Some(channel) = self.open.remove(&id) {
                    channel.closed.store(true, Ordering::SeqCst);
                    channel.process.kill();
                    self.status
                        .lock()
                        .expect("commands")
                        .remove(&channel.serial);
                }
            }
            // Host-to-client messages arriving from a client are ignored.
            _ => {}
        }
        Ok(())
    }

    /// A launch that asked for a RemoteApp, when this host serves it as
    /// one: the login, or why not. `None` for everything else, which opens
    /// as usual.
    async fn remote_app(&self, kind: &ChannelKind) -> Option<Result<(), RemoteAppRefusal>> {
        let ChannelKind::Launch {
            argv,
            remote_app: true,
            password,
        } = kind
        else {
            return None;
        };
        let provider = Arc::clone(self.remote_apps.as_ref()?);
        if let Err(reason) = self.authorizer.authorize(&self.principal, kind) {
            return Some(Err(RemoteAppRefusal::Refused(reason)));
        }
        let (who, argv, password) = (self.principal.clone(), argv.clone(), password.clone());
        tokio::task::spawn_blocking(move || provider.launch(&who, &argv, password.as_deref()))
            .await
            .unwrap_or_else(|e| Some(Err(RemoteAppRefusal::Refused(e.to_string()))))
    }

    async fn send(&self, message: CommandMessage) -> Result<(), TransportError> {
        self.session
            .send_control(&ControlMessage::Command(message))
            .await
    }

    /// Opens a channel. Returns the process id when the system gives one;
    /// a launch returns without leaving a channel open.
    async fn start(&mut self, id: ChannelId, kind: ChannelKind) -> Result<Option<u32>, String> {
        self.open
            .retain(|_, channel| !channel.closed.load(Ordering::SeqCst));
        if self.open.contains_key(&id) {
            return Err("that channel is already open".into());
        }
        if self.open.len() >= MAX_CHANNELS {
            return Err("too many channels open".into());
        }
        self.authorizer.authorize(&self.principal, &kind)?;
        let what = match &kind {
            ChannelKind::Shell(_) => "shell".to_owned(),
            ChannelKind::Files => "files".to_owned(),
            ChannelKind::Watch { .. } => "watch".to_owned(),
            ChannelKind::Chat => "chat".to_owned(),
            ChannelKind::Exec { argv, .. } | ChannelKind::Launch { argv, .. } => {
                argv.first().cloned().unwrap_or_default()
            }
        };
        let starter = process_starter();
        if let ChannelKind::Launch { argv, .. } = &kind {
            return starter.detached(argv, &launch_env());
        }
        let (process, readers, pid): (Arc<dyn RunningProcess>, Vec<Box<dyn Read + Send>>, _) = if matches!(
            kind,
            ChannelKind::Files | ChannelKind::Watch { .. } | ChannelKind::Chat
        ) {
            if !matches!(kind, ChannelKind::Chat) && self.services.shares.is_empty() {
                return Err("this host shares no folders".into());
            }
            let (session, runtime) = (Arc::clone(&self.session), tokio::runtime::Handle::current());
            let send: Box<dyn Fn(Vec<u8>) -> bool + Send + Sync> =
                Box::new(move |bytes: Vec<u8>| {
                    runtime
                        .block_on(session.send_control(&ControlMessage::Command(
                            CommandMessage::Data { id, bytes },
                        )))
                        .is_ok()
                });
            let process: Arc<dyn RunningProcess> = match &kind {
                ChannelKind::Chat => {
                    let Some(hub) = self.services.chat.as_ref() else {
                        return Err("this host has no chat".into());
                    };
                    Arc::new(hub.open(self.principal.clone(), Arc::from(send)))
                }
                ChannelKind::Watch {
                    share,
                    path,
                    pattern,
                    existing,
                } => Arc::new(crate::watch::WatchProcess::start(
                    &self.services.shares,
                    share,
                    path,
                    pattern.clone(),
                    *existing,
                    send,
                )?),
                _ => Arc::new(crate::files::FilesProcess::new(
                    Arc::clone(&self.services.shares),
                    send,
                )),
            };
            (process, Vec::new(), None)
        } else {
            let Started {
                process,
                output,
                pid,
            } = tokio::task::spawn_blocking(move || start_channel(&*starter, &kind))
                .await
                .map_err(|e| e.to_string())??;
            (process, output, pid)
        };

        let closed = Arc::new(AtomicBool::new(false));
        let (input, input_rx) = std::sync::mpsc::channel::<Option<Vec<u8>>>();
        {
            let process = Arc::clone(&process);
            std::thread::spawn(move || {
                for chunk in input_rx {
                    match chunk {
                        Some(bytes) => {
                            if process.write(&bytes).is_err() {
                                return;
                            }
                        }
                        None => process.eof(),
                    }
                }
            });
        }

        let runtime = tokio::runtime::Handle::current();
        let mut reading = Vec::new();
        for mut reader in readers {
            let session = Arc::clone(&self.session);
            let runtime = runtime.clone();
            reading.push(std::thread::spawn(move || {
                let mut buf = vec![0u8; MAX_CHUNK];
                loop {
                    let n = match reader.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    for message in data_messages(id, &buf[..n]) {
                        let sent = runtime
                            .block_on(session.send_control(&ControlMessage::Command(message)));
                        if sent.is_err() {
                            return;
                        }
                    }
                }
            }));
        }

        let serial = self.serial.fetch_add(1, Ordering::SeqCst);
        self.status.lock().expect("commands").insert(
            serial,
            CommandStatus {
                peer: self.principal.peer,
                channel: id,
                what,
                since: Instant::now(),
            },
        );
        {
            let session = Arc::clone(&self.session);
            let process = Arc::clone(&process);
            let closed = Arc::clone(&closed);
            let status = Arc::clone(&self.status);
            std::thread::spawn(move || {
                let code = process.wait();
                for reader in reading {
                    let _ = reader.join();
                }
                status.lock().expect("commands").remove(&serial);
                if !closed.swap(true, Ordering::SeqCst) {
                    let _ = runtime.block_on(session.send_control(&ControlMessage::Command(
                        CommandMessage::Exited { id, code },
                    )));
                }
            });
        }
        self.open.insert(
            id,
            Channel {
                process,
                input,
                closed,
                serial,
            },
        );
        Ok(pid)
    }
}

impl Drop for Channels {
    fn drop(&mut self) {
        for channel in self.open.values() {
            channel.closed.store(true, Ordering::SeqCst);
            channel.process.kill();
            self.status
                .lock()
                .expect("commands")
                .remove(&channel.serial);
        }
    }
}

/// A command a channel started: what it offers the channel, its output
/// (read until each reader ends), and its process id when there is one.
pub struct Started {
    pub process: Arc<dyn RunningProcess>,
    pub output: Vec<Box<dyn Read + Send>>,
    pub pid: Option<u32>,
}

/// How the host starts what a client asks for: a terminal, a command with
/// piped input and output, or an application left to run. The host's own
/// way ([`HostProcesses`]) starts them as the host's user; a Windows host
/// running as a service replaces it so that they start as the user signed
/// in at the PC, never as SYSTEM (docs/BACKENDS.md, "The secure screen").
pub trait ProcessStarter: Send + Sync + 'static {
    fn terminal(&self, spawn: &windowcast_pty::Spawn) -> Result<Started, String>;
    fn piped(&self, argv: &[String], env: &[(String, String)]) -> Result<Started, String>;
    /// Starts an application with no input or output; its windows are the
    /// window list's business from then on.
    fn detached(&self, argv: &[String], env: &[(String, String)]) -> Result<Option<u32>, String>;
}

/// Starts processes as the host's own user.
pub struct HostProcesses;

static PROCESS_STARTER: std::sync::RwLock<Option<Arc<dyn ProcessStarter>>> =
    std::sync::RwLock::new(None);

/// Replaces how every channel from now on starts its process.
pub fn set_process_starter(starter: Arc<dyn ProcessStarter>) {
    *PROCESS_STARTER.write().expect("process starter") = Some(starter);
}

fn process_starter() -> Arc<dyn ProcessStarter> {
    PROCESS_STARTER
        .read()
        .expect("process starter")
        .clone()
        .unwrap_or_else(|| Arc::new(HostProcesses))
}

/// Variables every shell, command and program a client starts gets on top
/// of the host's environment: where a private session's programs go
/// (docs/BACKENDS.md, "A private session").
static LAUNCH_ENV: std::sync::RwLock<Vec<(String, String)>> = std::sync::RwLock::new(Vec::new());

/// Sets [`LAUNCH_ENV`] (empty: the host's environment as it is).
pub fn set_launch_env(vars: Vec<(String, String)>) {
    *LAUNCH_ENV.write().expect("launch environment") = vars;
}

fn launch_env() -> Vec<(String, String)> {
    LAUNCH_ENV.read().expect("launch environment").clone()
}

/// What `kind` (not a launch) starts, through `starter`.
fn start_channel(starter: &dyn ProcessStarter, kind: &ChannelKind) -> Result<Started, String> {
    match kind {
        ChannelKind::Shell(pty) => starter.terminal(&terminal(None, pty)),
        ChannelKind::Exec {
            argv,
            pty: Some(pty),
        } => {
            if argv.is_empty() {
                return Err("no command".into());
            }
            starter.terminal(&terminal(Some(argv.clone()), pty))
        }
        ChannelKind::Exec { argv, pty: None } => {
            if argv.is_empty() {
                return Err("no command".into());
            }
            starter.piped(argv, &launch_env())
        }
        ChannelKind::Launch { .. } => Err("a launch is not a stream".into()),
        ChannelKind::Files | ChannelKind::Watch { .. } | ChannelKind::Chat => {
            Err("files and chat are not a process".into())
        }
    }
}

fn terminal(
    program: Option<Vec<String>>,
    pty: &windowcast_protocol::command::Pty,
) -> windowcast_pty::Spawn {
    windowcast_pty::Spawn {
        size: pty_size(pty.size),
        term: pty.term.clone(),
        program,
        env: launch_env(),
    }
}

impl ProcessStarter for HostProcesses {
    fn terminal(&self, spawn: &windowcast_pty::Spawn) -> Result<Started, String> {
        let spawned = windowcast_pty::Pty::spawn(spawn)
            .map_err(|e| format!("could not start the shell: {e}"))?;
        let reader = spawned.take_reader().ok_or("no terminal output")?;
        Ok(Started {
            process: Arc::new(PtyProcess(spawned)),
            output: vec![Box::new(reader)],
            pid: None,
        })
    }

    fn piped(&self, argv: &[String], env: &[(String, String)]) -> Result<Started, String> {
        let (program, args) = argv.split_first().ok_or("no command")?;
        let mut child = Command::new(program)
            .args(args)
            .envs(env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not start {program}: {e}"))?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let output: Vec<Box<dyn Read + Send>> = vec![
            Box::new(child.stdout.take().ok_or("no stdout")?),
            Box::new(child.stderr.take().ok_or("no stderr")?),
        ];
        let process = PipedProcess {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
        };
        Ok(Started {
            process: Arc::new(process),
            output,
            pid: Some(pid),
        })
    }

    fn detached(&self, argv: &[String], env: &[(String, String)]) -> Result<Option<u32>, String> {
        let (program, args) = argv.split_first().ok_or("no application")?;
        let mut child = Command::new(program)
            .args(args)
            .envs(env.iter().cloned())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("could not start {program}: {e}"))?;
        let pid = child.id();
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(Some(pid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windowcast_protocol::command::Pty;

    /// Records which way it was asked to start something, and starts
    /// nothing.
    #[derive(Default)]
    struct Recording(Mutex<Vec<String>>);

    impl ProcessStarter for Recording {
        fn terminal(&self, spawn: &windowcast_pty::Spawn) -> Result<Started, String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("terminal {:?}", spawn.program));
            Err("recorded".into())
        }
        fn piped(&self, argv: &[String], _env: &[(String, String)]) -> Result<Started, String> {
            self.0.lock().unwrap().push(format!("piped {argv:?}"));
            Err("recorded".into())
        }
        fn detached(
            &self,
            argv: &[String],
            _env: &[(String, String)],
        ) -> Result<Option<u32>, String> {
            self.0.lock().unwrap().push(format!("detached {argv:?}"));
            Err("recorded".into())
        }
    }

    fn pty() -> Pty {
        Pty {
            size: TerminalSize { cols: 80, rows: 24 },
            term: "xterm".into(),
        }
    }

    #[test]
    fn every_channel_starts_through_the_starter() {
        let starter = Recording::default();
        let argv = vec!["whoami".to_owned()];
        for kind in [
            ChannelKind::Shell(pty()),
            ChannelKind::Exec {
                argv: argv.clone(),
                pty: Some(pty()),
            },
            ChannelKind::Exec {
                argv: argv.clone(),
                pty: None,
            },
        ] {
            assert_eq!(
                start_channel(&starter, &kind).err().as_deref(),
                Some("recorded")
            );
        }
        assert_eq!(
            *starter.0.lock().unwrap(),
            [
                "terminal None",
                "terminal Some([\"whoami\"])",
                "piped [\"whoami\"]",
            ]
        );
    }

    #[test]
    fn an_empty_command_starts_nothing() {
        let starter = Recording::default();
        for pty in [Some(pty()), None] {
            let kind = ChannelKind::Exec { argv: vec![], pty };
            assert_eq!(
                start_channel(&starter, &kind).err().as_deref(),
                Some("no command")
            );
        }
        assert!(starter.0.lock().unwrap().is_empty());
    }
}

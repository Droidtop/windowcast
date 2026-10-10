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
trait Process: Send + Sync {
    fn write(&self, bytes: &[u8]) -> std::io::Result<()>;
    fn eof(&self);
    fn resize(&self, _size: TerminalSize) {}
    fn kill(&self);
    /// Blocks until the command has ended.
    fn wait(&self) -> Option<i32>;
}

struct PtyProcess(windowcast_pty::Pty);

impl Process for PtyProcess {
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

impl Process for PipedProcess {
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
    process: Arc<dyn Process>,
    /// Input goes in order, off the session's task: a command that does
    /// not read must not hold up the control channel.
    input: std::sync::mpsc::Sender<Option<Vec<u8>>>,
    /// Set when the client closed the channel, so no `Exited` follows.
    closed: Arc<AtomicBool>,
    serial: u64,
}

/// A session's channels. Dropping it ends every command still running.
pub(crate) struct Channels {
    session: Arc<Session>,
    principal: Principal,
    authorizer: Arc<dyn CommandAuthorizer>,
    status: StatusMap,
    serial: Arc<AtomicU64>,
    open: HashMap<ChannelId, Channel>,
}

impl Channels {
    pub(crate) fn new(
        session: Arc<Session>,
        principal: Principal,
        authorizer: Arc<dyn CommandAuthorizer>,
        status: StatusMap,
        serial: Arc<AtomicU64>,
    ) -> Self {
        Channels {
            session,
            principal,
            authorizer,
            status,
            serial,
            open: HashMap::new(),
        }
    }

    pub(crate) async fn handle(&mut self, message: CommandMessage) -> Result<(), TransportError> {
        match message {
            CommandMessage::Open { id, kind } => {
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
            ChannelKind::Exec { argv, .. } | ChannelKind::Launch { argv } => {
                argv.first().cloned().unwrap_or_default()
            }
        };
        if let ChannelKind::Launch { argv } = &kind {
            return launch(argv);
        }
        let (process, readers, pid) = tokio::task::spawn_blocking(move || spawn(&kind))
            .await
            .map_err(|e| e.to_string())??;

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

type Spawned = (Arc<dyn Process>, Vec<Box<dyn Read + Send>>, Option<u32>);

fn spawn(kind: &ChannelKind) -> Result<Spawned, String> {
    match kind {
        ChannelKind::Shell(pty) => spawn_pty(None, pty),
        ChannelKind::Exec {
            argv,
            pty: Some(pty),
        } => {
            if argv.is_empty() {
                return Err("no command".into());
            }
            spawn_pty(Some(argv.clone()), pty)
        }
        ChannelKind::Exec { argv, pty: None } => {
            let (program, args) = argv.split_first().ok_or("no command")?;
            let mut child = Command::new(program)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| format!("could not start {program}: {e}"))?;
            let pid = child.id();
            let stdin = child.stdin.take();
            let readers: Vec<Box<dyn Read + Send>> = vec![
                Box::new(child.stdout.take().ok_or("no stdout")?),
                Box::new(child.stderr.take().ok_or("no stderr")?),
            ];
            let process = PipedProcess {
                child: Mutex::new(child),
                stdin: Mutex::new(stdin),
            };
            Ok((Arc::new(process), readers, Some(pid)))
        }
        ChannelKind::Launch { .. } => Err("a launch is not a stream".into()),
    }
}

fn spawn_pty(
    program: Option<Vec<String>>,
    pty: &windowcast_protocol::command::Pty,
) -> Result<Spawned, String> {
    let spawned = windowcast_pty::Pty::spawn(&windowcast_pty::Spawn {
        size: pty_size(pty.size),
        term: pty.term.clone(),
        program,
    })
    .map_err(|e| format!("could not start the shell: {e}"))?;
    let reader = spawned.take_reader().ok_or("no terminal output")?;
    Ok((Arc::new(PtyProcess(spawned)), vec![Box::new(reader)], None))
}

/// Starts an application detached: no input, no output, reaped when it
/// ends. Its windows are the window list's business from then on.
fn launch(argv: &[String]) -> Result<Option<u32>, String> {
    let (program, args) = argv.split_first().ok_or("no application")?;
    let mut child = Command::new(program)
        .args(args)
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

//! The SSH client against an SSH server this test runs itself (russh's
//! server, with a real shell on a PTY behind it): password, key and
//! certificate login (a certificate a windowcast host's CA issues for an
//! account, the server trusting that CA as sshd's `TrustedUserCAKeys`
//! does), host-key pinning, a shell with a resize, a command with its exit
//! code, and an application launch.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use russh::keys::{Algorithm, Certificate, PrivateKey};
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel as SshChannel, ChannelId};
use windowcast_accounts::ssh::{CertificateAuthority, UserKey};
use windowcast_accounts::{Account, Method};
use windowcast_protocol::command::{ChannelKind, Pty, TerminalSize};
use windowcast_pty::Spawn;
use windowcast_terminal::{
    connect, ChannelEvent, HostKeyPolicy, HostKeyStore, SshAuth, SshError, SshTarget, Terminal,
};

const WAIT: Duration = Duration::from_secs(20);

/// The terminal a client asked for: its TERM, columns and rows.
type PtyRequest = (String, u32, u32);

#[derive(Clone)]
struct TestServer {
    user_key: Arc<russh::keys::PublicKey>,
    /// The user CA this server trusts, as `TrustedUserCAKeys` names it.
    user_ca: Arc<russh::keys::ssh_key::Fingerprint>,
    ptys: Arc<Mutex<HashMap<ChannelId, Arc<windowcast_pty::Pty>>>>,
    asked: Arc<Mutex<HashMap<ChannelId, PtyRequest>>>,
}

impl server::Server for TestServer {
    type Handler = TestServer;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> TestServer {
        self.clone()
    }
}

impl TestServer {
    fn start(&self, channel: ChannelId, command: Option<&[u8]>, session: &mut Session) {
        let asked = self.asked.lock().unwrap().remove(&channel);
        if let (None, Some(command)) = (&asked, command) {
            // No terminal asked for, so none is given: as sshd does for a
            // plain exec, the command has no controlling terminal.
            let mut child = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("{} 2>&1", String::from_utf8_lossy(command)))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut out = child.stdout.take().unwrap();
            let handle = session.handle();
            let runtime = tokio::runtime::Handle::current();
            std::thread::spawn(move || {
                use std::io::Read;
                let mut buf = [0u8; 4096];
                while let Ok(n) = out.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let _ = runtime.block_on(handle.data(channel, buf[..n].to_vec()));
                }
                let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(0);
                let _ = runtime.block_on(handle.exit_status_request(channel, code as u32));
                let _ = runtime.block_on(handle.eof(channel));
                let _ = runtime.block_on(handle.close(channel));
            });
            return;
        }
        let (term, cols, rows) = asked.unwrap_or(("dumb".into(), 80, 24));
        let program = match command {
            None => vec!["/bin/sh".to_owned()],
            Some(command) => vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                String::from_utf8_lossy(command).into_owned(),
            ],
        };
        let pty = Arc::new(
            windowcast_pty::Pty::spawn(&Spawn {
                size: windowcast_pty::Size {
                    cols: cols as u16,
                    rows: rows as u16,
                },
                term,
                program: Some(program),
            })
            .unwrap(),
        );
        self.ptys.lock().unwrap().insert(channel, Arc::clone(&pty));
        let handle = session.handle();
        let runtime = tokio::runtime::Handle::current();
        let mut reader = pty.take_reader().unwrap();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let data = buf[..n].to_vec();
                let _ = runtime.block_on(handle.data(channel, data));
            }
            let code = pty.wait().unwrap_or(0);
            let _ = runtime.block_on(handle.exit_status_request(channel, code as u32));
            let _ = runtime.block_on(handle.eof(channel));
            let _ = runtime.block_on(handle.close(channel));
        });
    }
}

impl server::Handler for TestServer {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == "tester" && password == "secret" {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: &russh::keys::PublicKey,
    ) -> Result<Auth, Self::Error> {
        Ok(if user == "tester" && key == &*self.user_key {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    /// As sshd with `TrustedUserCAKeys`: a user certificate signed by the
    /// trusted CA, valid now, naming the user among its principals.
    /// (russh has checked the client holds the certified key.)
    async fn auth_openssh_certificate(
        &mut self,
        user: &str,
        certificate: &Certificate,
    ) -> Result<Auth, Self::Error> {
        let trusted = certificate.validate([&*self.user_ca]).is_ok()
            && certificate.cert_type() == russh::keys::ssh_key::certificate::CertType::User
            && certificate.valid_principals().iter().any(|p| p == user);
        Ok(if user == "tester" && trusted {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        _channel: SshChannel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        cols: u32,
        rows: u32,
        _: u32,
        _: u32,
        _: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.asked
            .lock()
            .unwrap()
            .insert(channel, (term.to_owned(), cols, rows));
        session.channel_success(channel)
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        self.start(channel, None, session);
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        command: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        self.start(channel, Some(command), session);
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(pty) = self.ptys.lock().unwrap().get(&channel) {
            let _ = pty.write(data);
        }
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        cols: u32,
        rows: u32,
        _: u32,
        _: u32,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(pty) = self.ptys.lock().unwrap().get(&channel) {
            let _ = pty.resize(windowcast_pty::Size {
                cols: cols as u16,
                rows: rows as u16,
            });
        }
        Ok(())
    }
}

struct Running {
    port: u16,
    user_key_pem: String,
    host_fingerprint: String,
    /// The user CA the server trusts, and the folder its key is in.
    ca: CertificateAuthority,
    ca_dir: std::path::PathBuf,
}

async fn run_server() -> Running {
    static SERVERS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let ca_dir = std::env::temp_dir().join(format!(
        "windowcast-ssh-test-{}-{}",
        std::process::id(),
        SERVERS.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&ca_dir).unwrap();
    let ca = CertificateAuthority::load_or_generate(&ca_dir.join("user-ca")).unwrap();
    let user_ca = russh::keys::PublicKey::from_openssh(&ca.public_key().unwrap())
        .unwrap()
        .fingerprint(russh::keys::HashAlg::Sha256);
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let host_fingerprint = host_key
        .public_key()
        .fingerprint(russh::keys::HashAlg::Sha256)
        .to_string();
    let user_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let user_key_pem = user_key
        .to_openssh(russh::keys::ssh_key::LineEnding::LF)
        .unwrap()
        .to_string();
    let config = Arc::new(server::Config {
        keys: vec![host_key],
        auth_rejection_time: Duration::from_millis(10),
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut server = TestServer {
        user_key: Arc::new(user_key.public_key().clone()),
        user_ca: Arc::new(user_ca),
        ptys: Default::default(),
        asked: Default::default(),
    };
    tokio::spawn(async move {
        let _ = server.run_on_socket(config, &listener).await;
    });
    Running {
        port,
        user_key_pem,
        host_fingerprint,
        ca,
        ca_dir,
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.ca_dir);
    }
}

fn target(server: &Running) -> SshTarget {
    SshTarget {
        host: "127.0.0.1".into(),
        port: server.port,
        user: "tester".into(),
    }
}

fn password() -> SshAuth {
    SshAuth::Password("secret".into())
}

fn screen_text(terminal: &Terminal) -> String {
    terminal
        .snapshot()
        .lines
        .iter()
        .map(|line| line.iter().map(|r| r.text.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn wait_for_screen(terminal: &Terminal, needle: &str) {
    let deadline = Instant::now() + WAIT;
    let mut seen = 0;
    while !screen_text(terminal).contains(needle) {
        assert!(
            Instant::now() < deadline,
            "{needle:?} never appeared; screen:\n{}",
            screen_text(terminal)
        );
        if terminal.wait_change(seen, Duration::from_millis(200)) {
            seen = terminal.snapshot().version;
        }
    }
}

fn shell() -> ChannelKind {
    ChannelKind::Shell(Pty {
        size: TerminalSize {
            cols: 100,
            rows: 30,
        },
        term: "xterm-256color".into(),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shell_over_a_password_login_with_a_resize() {
    let server = run_server().await;
    let store = HostKeyStore::in_memory();
    let connection = connect(
        &target(&server),
        &password(),
        Arc::clone(&store),
        HostKeyPolicy::TrustOnFirstUse,
    )
    .await
    .unwrap();
    assert_eq!(connection.fingerprint(), server.host_fingerprint);

    let channel = connection.open(&shell()).await.unwrap();
    let terminal = Arc::new(Terminal::new(
        channel,
        TerminalSize {
            cols: 100,
            rows: 30,
        },
    ));
    let t = Arc::clone(&terminal);
    tokio::task::spawn_blocking(move || {
        t.send_text("echo hi-$((20+22)); stty size\r");
        wait_for_screen(&t, "hi-42");
        wait_for_screen(&t, "30 100");
        t.resize(TerminalSize { cols: 61, rows: 17 });
        t.send_text("stty size\r");
        wait_for_screen(&t, "17 61");
        t.send_text("exit 5\r");
        let deadline = Instant::now() + WAIT;
        while t.ended().is_none() {
            assert!(Instant::now() < deadline, "the shell never ended");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(t.ended(), Some(Some(5)));
    })
    .await
    .unwrap();
    connection.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn key_login_works_and_a_wrong_password_does_not() {
    let server = run_server().await;
    let store = HostKeyStore::in_memory();
    let key = SshAuth::Key {
        pem: server.user_key_pem.clone(),
        passphrase: None,
    };
    connect(
        &target(&server),
        &key,
        Arc::clone(&store),
        HostKeyPolicy::TrustOnFirstUse,
    )
    .await
    .unwrap();

    let wrong = connect(
        &target(&server),
        &SshAuth::Password("nope".into()),
        store,
        HostKeyPolicy::TrustOnFirstUse,
    )
    .await;
    assert!(
        matches!(wrong, Err(SshError::AuthFailed)),
        "{:?}",
        wrong.err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_account_certificate_logs_in_where_the_ca_is_trusted() {
    let server = run_server().await;
    let store = HostKeyStore::in_memory();
    let mine = UserKey::load_or_generate(&server.ca_dir.join("client-key")).unwrap();
    let account = |name: &str| Account {
        name: name.into(),
        groups: Vec::new(),
        method: Method::Oidc,
        provider: "corp".into(),
    };
    let login = |ca: &CertificateAuthority, name: &str| SshAuth::Certificate {
        pem: mine.private_key().unwrap(),
        certificate: ca
            .issue(
                &mine.public_key().unwrap(),
                &account(name),
                Duration::from_secs(600),
            )
            .unwrap(),
    };

    // The server's CA certified "tester": a shell's worth of login.
    let connection = connect(
        &target(&server),
        &login(&server.ca, "tester"),
        Arc::clone(&store),
        HostKeyPolicy::TrustOnFirstUse,
    )
    .await
    .unwrap();
    connection.close().await;

    // A certificate for another account, or from a CA the server does not
    // trust, is refused.
    let other_ca = CertificateAuthority::load_or_generate(&server.ca_dir.join("other-ca")).unwrap();
    for auth in [login(&server.ca, "mallory"), login(&other_ca, "tester")] {
        let refused = connect(
            &target(&server),
            &auth,
            Arc::clone(&store),
            HostKeyPolicy::TrustOnFirstUse,
        )
        .await;
        assert!(
            matches!(refused, Err(SshError::AuthFailed)),
            "{:?}",
            refused.err()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn host_keys_are_pinned_and_a_changed_key_is_refused() {
    let server = run_server().await;
    let store = HostKeyStore::in_memory();
    let name = target(&server).server();

    // Not pinned, and the policy pins nothing new.
    let refused = connect(
        &target(&server),
        &password(),
        Arc::clone(&store),
        HostKeyPolicy::Pinned,
    )
    .await;
    assert!(matches!(refused, Err(SshError::HostKeyRefused { .. })));

    // A fingerprint the user typed that is not this server's.
    let refused = connect(
        &target(&server),
        &password(),
        Arc::clone(&store),
        HostKeyPolicy::Fingerprint("SHA256:wrong".into()),
    )
    .await;
    assert!(matches!(refused, Err(SshError::HostKeyRefused { .. })));

    // The right one pins it, and then Pinned is enough.
    connect(
        &target(&server),
        &password(),
        Arc::clone(&store),
        HostKeyPolicy::Fingerprint(server.host_fingerprint.clone()),
    )
    .await
    .unwrap();
    connect(
        &target(&server),
        &password(),
        Arc::clone(&store),
        HostKeyPolicy::Pinned,
    )
    .await
    .unwrap();

    // The same name now answers with another key: refused, whatever the policy.
    store.pin(&name, "SHA256:somebody-else").unwrap();
    let changed = connect(
        &target(&server),
        &password(),
        Arc::clone(&store),
        HostKeyPolicy::TrustOnFirstUse,
    )
    .await;
    match changed {
        Err(SshError::HostKeyChanged {
            pinned, presented, ..
        }) => {
            assert_eq!(pinned, "SHA256:somebody-else");
            assert_eq!(presented, server.host_fingerprint);
        }
        other => panic!("expected a changed key, got {:?}", other.err()),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_reports_its_output_and_exit_code_and_an_application_launches() {
    let server = run_server().await;
    let connection = connect(
        &target(&server),
        &password(),
        HostKeyStore::in_memory(),
        HostKeyPolicy::TrustOnFirstUse,
    )
    .await
    .unwrap();

    let channel = connection
        .open(&ChannelKind::Exec {
            argv: vec!["sh".into(), "-c".into(), "echo out-$((6*7)); exit 3".into()],
            pty: None,
        })
        .await
        .unwrap();
    let (output, code) = tokio::task::spawn_blocking(move || {
        let mut output = Vec::new();
        let mut code = None;
        let deadline = Instant::now() + WAIT;
        loop {
            assert!(Instant::now() < deadline, "the command never ended");
            match channel.next_event(Duration::from_millis(200)) {
                Some(ChannelEvent::Data(bytes)) => output.extend(bytes),
                Some(ChannelEvent::Exited(c)) => code = c,
                Some(ChannelEvent::Closed) => {
                    break (String::from_utf8_lossy(&output).into_owned(), code)
                }
                None => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(output.contains("out-42"), "{output:?}");
    assert_eq!(code, Some(3));

    // A launch starts the application detached and reports its process id.
    let marker = std::env::temp_dir().join(format!("wc-launch-{}", std::process::id()));
    let launched = connection
        .open(&ChannelKind::Launch {
            argv: vec!["touch".into(), marker.to_string_lossy().into_owned()],
            remote_app: false,
        })
        .await
        .unwrap();
    assert!(launched.pid().is_some_and(|pid| pid > 1));
    let deadline = Instant::now() + WAIT;
    while !marker.exists() {
        assert!(Instant::now() < deadline, "the application never ran");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    std::fs::remove_file(marker).unwrap();
}

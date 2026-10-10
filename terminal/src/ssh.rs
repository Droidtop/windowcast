//! An SSH client that opens the command stream's channel kinds on any SSH
//! server: a shell on a pseudo-terminal, one command, an application
//! launch. The server's host key is pinned ([`crate::hostkeys`]); the user
//! authenticates with a password or a private key. The result is the same
//! [`Channel`] the windowcast session gives, so a viewer cannot tell the
//! two apart.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client;
use russh::keys::{decode_secret_key, Certificate, HashAlg, PrivateKeyWithHashAlg};
use russh::{ChannelMsg, Disconnect};
use tokio::sync::mpsc::UnboundedReceiver;
use windowcast_protocol::command::{ChannelKind, Pty};

use crate::channel::{Channel, ChannelEnd, ChannelEvent, Command};
use crate::hostkeys::{HostKeyPolicy, HostKeyStore, Known};

/// How long to wait for a server before giving up on connecting.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
}

impl SshTarget {
    /// The name host keys are pinned under.
    pub fn server(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// How the user proves who they are. The client never keeps a password.
#[derive(Clone)]
pub enum SshAuth {
    Password(String),
    /// A private key in OpenSSH or PKCS#8 PEM form.
    Key {
        pem: String,
        passphrase: Option<String>,
    },
    /// A private key (unencrypted, OpenSSH or PKCS#8 PEM) with an OpenSSH
    /// user certificate for it: what a windowcast host issues to a device
    /// signed in with an account (docs/ACCOUNTS.md), which any sshd that
    /// trusts the host's CA (`TrustedUserCAKeys`) accepts.
    Certificate {
        pem: String,
        certificate: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("could not connect: {0}")]
    Connect(String),
    #[error("{server} presented {presented}, but it is pinned to {pinned}: refusing to connect")]
    HostKeyChanged {
        server: String,
        pinned: String,
        presented: String,
    },
    #[error("{server} is not trusted: its key is {fingerprint}")]
    HostKeyRefused { server: String, fingerprint: String },
    #[error("the server refused the login")]
    AuthFailed,
    #[error("cannot use that key: {0}")]
    Key(String),
    #[error("ssh: {0}")]
    Ssh(#[from] russh::Error),
    #[error("{0}")]
    Command(String),
}

/// Why the host-key check said no, kept for the error the caller sees.
#[derive(Default)]
struct Verdict {
    presented: Option<String>,
    refusal: Option<SshError>,
}

struct Handler {
    server: String,
    store: Arc<HostKeyStore>,
    policy: HostKeyPolicy,
    verdict: Arc<Mutex<Verdict>>,
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            return Ok(false);
        };
        let presented = key.fingerprint(HashAlg::Sha256).to_string();
        let mut verdict = self.verdict.lock().expect("verdict");
        verdict.presented = Some(presented.clone());
        let server = self.server.clone();
        let accepted = match self.store.check(&server, &presented) {
            Known::Match => true,
            Known::Changed { pinned } => {
                verdict.refusal = Some(SshError::HostKeyChanged {
                    server,
                    pinned,
                    presented,
                });
                return Ok(false);
            }
            Known::Unknown => {
                let trusted = match &self.policy {
                    HostKeyPolicy::TrustOnFirstUse => true,
                    HostKeyPolicy::Fingerprint(wanted) => *wanted == presented,
                    HostKeyPolicy::Ask(ask) => ask(&server, &presented),
                    HostKeyPolicy::Pinned => false,
                };
                if trusted {
                    if let Err(e) = self.store.pin(&server, &presented) {
                        tracing::warn!("could not record the host key of {server}: {e}");
                    }
                } else {
                    verdict.refusal = Some(SshError::HostKeyRefused {
                        server,
                        fingerprint: presented,
                    });
                }
                trusted
            }
        };
        Ok(accepted)
    }
}

/// A logged-in connection to an SSH server.
pub struct SshConnection {
    handle: client::Handle<Handler>,
    fingerprint: String,
}

/// Connects to `target`, checks and pins its host key, and logs in.
pub async fn connect(
    target: &SshTarget,
    auth: &SshAuth,
    store: Arc<HostKeyStore>,
    policy: HostKeyPolicy,
) -> Result<SshConnection, SshError> {
    let verdict = Arc::new(Mutex::new(Verdict::default()));
    let handler = Handler {
        server: target.server(),
        store,
        policy,
        verdict: Arc::clone(&verdict),
    };
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    });
    let connecting = client::connect(config, (target.host.as_str(), target.port), handler);
    let mut handle = match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
        Err(_) => return Err(SshError::Connect("timed out".into())),
        Ok(Ok(handle)) => handle,
        Ok(Err(e)) => {
            return Err(verdict
                .lock()
                .expect("verdict")
                .refusal
                .take()
                .unwrap_or_else(|| SshError::Connect(e.to_string())))
        }
    };
    let presented = verdict
        .lock()
        .expect("verdict")
        .presented
        .clone()
        .unwrap_or_default();

    let result = match auth {
        SshAuth::Password(password) => {
            handle
                .authenticate_password(target.user.clone(), password.clone())
                .await?
        }
        SshAuth::Key { pem, passphrase } => {
            let key = decode_secret_key(pem, passphrase.as_deref())
                .map_err(|e| SshError::Key(e.to_string()))?;
            let hash = handle.best_supported_rsa_hash().await?.flatten();
            handle
                .authenticate_publickey(
                    target.user.clone(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                )
                .await?
        }
        SshAuth::Certificate { pem, certificate } => {
            let key = decode_secret_key(pem, None).map_err(|e| SshError::Key(e.to_string()))?;
            let certificate = Certificate::from_openssh(certificate.trim())
                .map_err(|e| SshError::Key(e.to_string()))?;
            handle
                .authenticate_openssh_cert(target.user.clone(), Arc::new(key), certificate)
                .await?
        }
    };
    if !result.success() {
        return Err(SshError::AuthFailed);
    }
    Ok(SshConnection {
        handle,
        fingerprint: presented,
    })
}

/// Single-quotes `word` for a POSIX shell.
fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

fn command_line(argv: &[String]) -> String {
    argv.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ")
}

impl SshConnection {
    /// The server's host key fingerprint, as pinned.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }

    /// Opens a channel of `kind`. A launch needs a POSIX shell on the
    /// server (it runs the application under `nohup` and reports its
    /// process id).
    pub async fn open(&self, kind: &ChannelKind) -> Result<Channel, SshError> {
        let channel = self.handle.channel_open_session().await?;
        let pty = |p: &Pty| {
            (
                p.term.clone(),
                u32::from(p.size.cols),
                u32::from(p.size.rows),
            )
        };
        match kind {
            ChannelKind::Shell(p) => {
                let (term, cols, rows) = pty(p);
                channel
                    .request_pty(true, &term, cols, rows, 0, 0, &[])
                    .await?;
                channel.request_shell(true).await?;
            }
            ChannelKind::Exec { argv, pty: p } => {
                if argv.is_empty() {
                    return Err(SshError::Command("no command".into()));
                }
                if let Some(p) = p {
                    let (term, cols, rows) = pty(p);
                    channel
                        .request_pty(true, &term, cols, rows, 0, 0, &[])
                        .await?;
                }
                channel.exec(true, command_line(argv)).await?;
            }
            ChannelKind::Launch { argv } => {
                if argv.is_empty() {
                    return Err(SshError::Command("no application".into()));
                }
                let line = format!(
                    "nohup {} >/dev/null 2>&1 </dev/null & echo $!",
                    command_line(argv)
                );
                return self.launch(channel, line).await;
            }
        }
        let (client_end, end) = Channel::pair(None);
        tokio::spawn(drive(channel, end));
        Ok(client_end)
    }

    async fn launch(
        &self,
        mut channel: russh::Channel<client::Msg>,
        line: String,
    ) -> Result<Channel, SshError> {
        channel.exec(true, line).await?;
        let mut output = Vec::new();
        let mut code = None;
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } => output.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        let _ = channel.close().await;
        if code != Some(0) {
            return Err(SshError::Command(format!(
                "the server could not start the application ({})",
                String::from_utf8_lossy(&output).trim()
            )));
        }
        let pid = String::from_utf8_lossy(&output).trim().parse().ok();
        let (client_end, end) = Channel::pair(pid);
        let _ = end.events.send(ChannelEvent::Exited(None));
        let _ = end.events.send(ChannelEvent::Closed);
        Ok(client_end)
    }

    pub async fn close(&self) {
        let _ = self
            .handle
            .disconnect(Disconnect::ByApplication, "bye", "en")
            .await;
    }
}

/// Moves one channel's traffic between the SSH channel and the client's
/// half until either side ends.
async fn drive(mut channel: russh::Channel<client::Msg>, end: ChannelEnd) {
    let ChannelEnd {
        mut commands,
        events,
    } = end;
    let mut code = None;
    let mut closing = false;
    loop {
        tokio::select! {
            command = recv(&mut commands), if !closing => match command {
                Some(Command::Data(bytes)) => {
                    if channel.data(&bytes[..]).await.is_err() {
                        break;
                    }
                }
                Some(Command::Resize(size)) => {
                    let _ = channel
                        .window_change(u32::from(size.cols), u32::from(size.rows), 0, 0)
                        .await;
                }
                Some(Command::Eof) => {
                    let _ = channel.eof().await;
                }
                Some(Command::Close) | None => {
                    closing = true;
                    let _ = channel.close().await;
                }
            },
            message = channel.wait() => match message {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    if events.send(ChannelEvent::Data(data.to_vec())).is_err() {
                        break;
                    }
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => code = Some(exit_status as i32),
                Some(ChannelMsg::Close) | None => break,
                _ => {}
            },
        }
    }
    let _ = events.send(ChannelEvent::Exited(code));
    let _ = events.send(ChannelEvent::Closed);
}

async fn recv(commands: &mut UnboundedReceiver<Command>) -> Option<Command> {
    commands.recv().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_quoted_for_a_posix_shell() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(
            command_line(&["echo".into(), "a b".into(), "$HOME".into()]),
            "'echo' 'a b' '$HOME'"
        );
    }
}

//! The client side of the windowcast library, and the one surface every
//! client implements against: a Rust API ([`Client`], [`ClientSession`])
//! and the C interface over it (`ffi`, declared in
//! `include/windowcast.h`). droidtop's Desktop mode (through JNI), the
//! Android library in `android/`, the reference CLI and any other client
//! use exactly this.
//!
//! The model is pull-based so no client has to accept calls from library
//! threads: a client connects, asks for the window list and for streams,
//! then polls two queues, session events ([`ClientSession::next_event`])
//! and each window's frames ([`ClientSession::next_frame`]), from its own
//! threads. Frames come out whole and ready for a hardware decoder; a
//! window the host hands to RDP comes out as RGBA pictures instead
//! ([`ClientSession::next_picture`]), and its input goes over RDP.

mod command;
pub mod ffi;
pub mod ffi_terminal;
mod remote_app;
mod selector;

pub use command::SshSession;
pub use remote_app::REMOTE_APP_WINDOW;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use windowcast_accounts::oidc::{BrowserSignIn, DeviceSignIn, OidcError};
use windowcast_identity::{Identity, PeerId, TrustStore};
use windowcast_protocol::selection::{self, BackendRule};
use windowcast_protocol::{
    AccountCredential, AccountOffer, BackendKind, ControlMessage, HandoffTarget, InputEvent,
    OidcProviderInfo, SignInMethod, StreamBackend, StreamLimits, StreamOptions, StreamQuality,
    StreamTarget, VideoCodec, WindowId, WindowInfo,
};
use windowcast_rdp::client::RdpStream;
pub use windowcast_rdp::client::RgbaPicture;
use windowcast_transport::remote::{
    self, Answer, Directory, RemoteConfig, RemotePeers, Syncthing, CONNECT_WITHIN,
};
use windowcast_transport::{
    connect, AudioPacket, AudioTrack, ClientCredential, RemoteAudio, RemoteTrack, RemoteWindow,
    Session, TransportError, WindowFrame,
};

/// Frames queued per window before the client is considered behind. When
/// it is, frames are dropped up to the next keyframe (and one is asked
/// for), so a slow decoder skips ahead instead of falling further behind.
const FRAME_QUEUE: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{0}")]
    Transport(#[from] TransportError),
    #[error("cannot reach {0}: {1}")]
    Unreachable(String, std::io::Error),
    #[error("identity or trust store: {0}")]
    Identity(#[from] windowcast_identity::IdentityError),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("sound: {0}")]
    Audio(String),
    /// The host cannot be looked for away from the LAN: not paired, or no
    /// session on the LAN told this client its discovery ID yet.
    #[error("{0} cannot be reached away from the LAN yet: connect on the LAN once first")]
    NotReachableAway(String),
    /// Discovery has no address for the host, or none answered.
    #[error("{0} was not found away from the LAN ({1})")]
    NotFound(String, String),
    /// Signing in: the host's identity (hex) is not trusted yet. Show its
    /// fingerprint ([`fingerprint`]) and connect again with it accepted
    /// if the user confirms it.
    #[error("the host's identity {0} is not trusted yet")]
    HostNotTrusted(String),
    #[error("OpenID Connect: {0}")]
    Oidc(#[from] OidcError),
    /// A host or server did not open a command channel; the reason is
    /// for the user.
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Ssh(#[from] windowcast_terminal::SshError),
    /// This client's SSH key, or a certificate for it.
    #[error("{0}")]
    SshKey(#[from] windowcast_accounts::ssh::SshError),
    /// A RemoteApp launch needs the Windows password of this user: ask the
    /// user and launch again with it ([`ClientSession::launch_with_password`]).
    #[error("Remote Desktop needs the Windows password of {0}")]
    PasswordNeeded(String),
}

/// How a client signs in to a host with an account (docs/ACCOUNTS.md).
/// In JSON (the C interface): `{"password":{"username":"..","password":".."}}`,
/// `{"oidc":{"provider":"..","id_token":".."}}` or `"kerberos"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignIn {
    Password {
        username: String,
        password: String,
    },
    /// An ID token from [`Client::oidc_browser`] or [`Client::oidc_device`]
    /// with the provider the host named.
    Oidc {
        provider: String,
        id_token: String,
    },
    /// The user's current Kerberos tickets, for the service the host names.
    Kerberos,
}

impl SignIn {
    fn credential(&self, offer: &AccountOffer) -> Result<AccountCredential, String> {
        Ok(match self {
            SignIn::Password { username, password } => AccountCredential::Password {
                username: username.clone(),
                password: password.clone(),
            },
            SignIn::Oidc { provider, id_token } => AccountCredential::Oidc {
                provider: provider.clone(),
                id_token: id_token.clone(),
            },
            SignIn::Kerberos => {
                let service = offer
                    .kerberos_service
                    .as_deref()
                    .ok_or("the host takes no Kerberos sign-in")?;
                AccountCredential::Kerberos {
                    token: kerberos_token(service)?,
                }
            }
        })
    }
}

#[cfg(feature = "kerberos")]
fn kerberos_token(service: &str) -> Result<Vec<u8>, String> {
    windowcast_accounts::kerberos::initiate(service)
}

#[cfg(not(feature = "kerberos"))]
fn kerberos_token(_service: &str) -> Result<Vec<u8>, String> {
    Err("this client has no Kerberos support".into())
}

/// What a host offers a client that wants to sign in.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SignInOptions {
    /// The host's identity, hex.
    pub host_id: String,
    /// Its fingerprint, for the user to compare with the one the host
    /// shows ([`fingerprint`]).
    pub fingerprint: String,
    /// Whether this client already trusts it; until it does, the
    /// providers below are only the host's claim.
    pub trusted: bool,
    pub methods: Vec<SignInMethod>,
    pub providers: Vec<OidcProviderInfo>,
    pub kerberos_service: Option<String>,
}

/// An identity as people compare it: the first 16 bytes of its key in
/// groups of four hex digits (`1a2b-3c4d-...`), as host and client both
/// show it.
pub fn fingerprint(peer_hex: &str) -> String {
    peer_hex
        .as_bytes()
        .chunks(4)
        .take(8)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

/// 20 ms of interleaved stereo at 48 kHz: one Opus packet's worth.
const MICROPHONE_FRAME: usize = 960 * 2;

/// The microphone while it is on: its track, and the Opus encoder with the
/// samples not yet a whole packet.
struct Microphone {
    track: AudioTrack,
    encoder: opus::Encoder,
    pending: Vec<i16>,
}

/// Where a session keeps the host's discovery ID, and this client's own.
struct Rendezvous {
    peers: Arc<RemotePeers>,
    own: Option<String>,
}

/// One client identity and its trusted hosts; connects to any number of
/// hosts.
pub struct Client {
    runtime: Arc<Runtime>,
    identity: Arc<Identity>,
    trust: Mutex<TrustStore>,
    trust_path: PathBuf,
    /// The hosts' discovery IDs, learned on sessions, for reaching them
    /// away from the LAN.
    remote_hosts: Arc<RemotePeers>,
    /// The SSH servers' pinned host keys.
    host_keys: Arc<windowcast_terminal::HostKeyStore>,
    /// This client's own SSH key, made on first use; hosts certify it for
    /// the account the client signed in with.
    ssh_key_path: PathBuf,
}

impl Client {
    /// Loads (or creates) the client's identity and trusted-host list in
    /// `data_dir`, app-private storage on Android.
    pub fn new(data_dir: &Path) -> Result<Self, ClientError> {
        std::fs::create_dir_all(data_dir)?;
        let identity = Identity::load_or_generate(&data_dir.join("client-identity.key"))?;
        let trust_path = data_dir.join("client-trusted-hosts");
        let trust = TrustStore::load(&trust_path)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        Ok(Client {
            runtime: Arc::new(runtime),
            identity: Arc::new(identity),
            trust: Mutex::new(trust),
            trust_path,
            remote_hosts: Arc::new(RemotePeers::load(&data_dir.join("remote-hosts.json"))),
            host_keys: windowcast_terminal::HostKeyStore::open(
                &data_dir.join("ssh-known-hosts.json"),
            )?,
            ssh_key_path: data_dir.join("ssh-user-key"),
        })
    }

    /// This client's own SSH public key (an OpenSSH line), made on first
    /// use: the key to ask a host for a certificate for
    /// ([`ClientSession::request_ssh_certificate`]).
    pub fn ssh_public_key(&self) -> Result<String, ClientError> {
        let key = windowcast_accounts::ssh::UserKey::load_or_generate(&self.ssh_key_path)?;
        Ok(key.public_key()?)
    }

    /// The SSH login with `certificate`, an OpenSSH user certificate a host
    /// issued for [`Self::ssh_public_key`] (the `ssh_certificate` event):
    /// any SSH server that trusts that host's CA admits it as the account
    /// the certificate names. Fails for a certificate for another key.
    pub fn ssh_certificate_auth(
        &self,
        certificate: &str,
    ) -> Result<windowcast_terminal::SshAuth, ClientError> {
        let key = windowcast_accounts::ssh::UserKey::load_or_generate(&self.ssh_key_path)?;
        Ok(windowcast_terminal::SshAuth::Certificate {
            certificate: key.check_certificate(certificate)?,
            pem: key.private_key()?,
        })
    }

    /// This client's identity, as a host shows it.
    pub fn peer_id(&self) -> String {
        self.identity.peer_id().to_hex()
    }

    /// Connects to a host agent at `address` (`HOST:PORT`). Resumes with
    /// this device's pinned identity when the host trusts it; `pin` is
    /// used only when that is refused, because the host is not pinned here
    /// or no longer trusts this device (so a PIN left in the form does not
    /// get a host that already paired this device to refuse the session
    /// with "pairing is not open"). Blocks until the session is up.
    pub fn connect(&self, address: &str, pin: Option<&str>) -> Result<ClientSession, ClientError> {
        let trusted = self.trust.lock().expect("trust store").clone();
        let mut host_ip = None;
        let resumed = if pin.is_some() {
            // Try the identity first; only a refusal that a PIN could fix falls through.
            match self.connect_once(address, None, &trusted, &mut host_ip) {
                Err(ClientError::Transport(e)) if pairing_could_help(&e) => None,
                other => Some(other),
            }
        } else {
            Some(self.connect_once(address, None, &trusted, &mut host_ip))
        };
        let established = match resumed {
            Some(result) => result?,
            None => self.connect_once(address, pin, &trusted, &mut host_ip)?,
        };

        if established.paired {
            let mut trust = self.trust.lock().expect("trust store");
            trust.pin(established.peer);
            trust.save(&self.trust_path)?;
        }
        Ok(self.started(established, host_ip))
    }

    /// One connection attempt: pairing with `pin`, or resuming when there is none.
    fn connect_once(
        &self,
        address: &str,
        pin: Option<&str>,
        trusted: &TrustStore,
        host_ip: &mut Option<std::net::IpAddr>,
    ) -> Result<windowcast_transport::Established, ClientError> {
        let identity = Arc::clone(&self.identity);
        self.runtime.block_on(async {
            let stream = tokio::net::TcpStream::connect(address)
                .await
                .map_err(|e| ClientError::Unreachable(address.to_owned(), e))?;
            *host_ip = stream.peer_addr().ok().map(|peer| peer.ip());
            // A host on this device is reached over loopback alone.
            let local = stream.peer_addr().is_ok_and(|peer| peer.ip().is_loopback());
            let session = if local {
                Session::local_only().await?
            } else {
                Session::new().await?
            };
            let credential = match pin {
                Some(pin) => ClientCredential::Pin(pin),
                None => ClientCredential::Pinned(trusted),
            };
            Ok::<_, ClientError>(connect(stream, session, &identity, credential).await?)
        })
    }

    /// A session on `established`; `host_ip` is where backends with their
    /// own connection (RDP) are reached, `None` away from the LAN (they
    /// are not offered then).
    fn started(
        &self,
        established: windowcast_transport::Established,
        host_ip: Option<std::net::IpAddr>,
    ) -> ClientSession {
        ClientSession::start(
            Arc::clone(&self.runtime),
            established.session,
            established.peer,
            established.paired,
            Rendezvous {
                peers: Arc::clone(&self.remote_hosts),
                own: windowcast_transport::remote::discovery_id(&self.identity).ok(),
            },
            host_ip,
        )
    }

    /// Asks the host at `address` which account sign-ins it takes, without
    /// signing in.
    pub fn sign_in_options(&self, address: &str) -> Result<SignInOptions, ClientError> {
        let identity = Arc::clone(&self.identity);
        let (host, offer) = self.runtime.block_on(async {
            let stream = tokio::net::TcpStream::connect(address)
                .await
                .map_err(|e| ClientError::Unreachable(address.to_owned(), e))?;
            Ok::<_, ClientError>(windowcast_transport::sign_in_offer(stream, &identity).await?)
        })?;
        let host_id = host.to_hex();
        Ok(SignInOptions {
            fingerprint: fingerprint(&host_id),
            trusted: self.trust.lock().expect("trust store").is_pinned(&host),
            host_id,
            methods: offer.methods,
            providers: offer.providers,
            kerberos_service: offer.kerberos_service,
        })
    }

    /// Signs in to the host at `address` with an account, and connects.
    /// The credential goes only to a host this client trusts, or whose
    /// identity (hex) the user confirmed as `accept_host`; any other fails
    /// with [`ClientError::HostNotTrusted`]. On success the host is
    /// trusted from then on, and later connections resume with
    /// [`Self::connect`] (no PIN) while the host keeps the registration.
    pub fn connect_account(
        &self,
        address: &str,
        sign_in: &SignIn,
        accept_host: Option<&str>,
    ) -> Result<ClientSession, ClientError> {
        let trusted = self.trust.lock().expect("trust store").clone();
        let accept = accept_host.map(PeerId::from_hex).transpose()?;
        let identity = Arc::clone(&self.identity);
        let make = |offer: &AccountOffer| sign_in.credential(offer);
        let mut host_ip = None;
        let established = self.runtime.block_on(async {
            let stream = tokio::net::TcpStream::connect(address)
                .await
                .map_err(|e| ClientError::Unreachable(address.to_owned(), e))?;
            host_ip = stream.peer_addr().ok().map(|peer| peer.ip());
            let local = stream.peer_addr().is_ok_and(|peer| peer.ip().is_loopback());
            let session = if local {
                Session::local_only().await?
            } else {
                Session::new().await?
            };
            let credential = ClientCredential::Account {
                trusted: &trusted,
                accept,
                credential: &make,
            };
            match connect(stream, session, &identity, credential).await {
                Ok(established) => Ok(established),
                Err(TransportError::HostNotTrusted(host)) => {
                    Err(ClientError::HostNotTrusted(host.to_hex()))
                }
                Err(e) => Err(e.into()),
            }
        })?;
        if established.signed_in {
            let mut trust = self.trust.lock().expect("trust store");
            trust.pin(established.peer);
            trust.save(&self.trust_path)?;
        }
        let session = self.started(established, host_ip);
        // A Windows host's RemoteApps log in as this user with this
        // password (docs/BACKENDS.md, "RemoteApp"); kept only in memory.
        if let SignIn::Password { password, .. } = sign_in {
            *session.shared.sign_in_password.lock().expect("password") = Some(password.clone());
        }
        Ok(session)
    }

    /// Starts signing in with an OpenID Connect provider in the user's
    /// browser: open [`BrowserSignIn::url`], then [`BrowserSignIn::finish`]
    /// gives the ID token for [`SignIn::Oidc`]. The token is bound to this
    /// client's identity. Blocks while it fetches the provider's metadata.
    pub fn oidc_browser(&self, provider: &OidcProviderInfo) -> Result<BrowserSignIn, ClientError> {
        Ok(BrowserSignIn::start(provider, &self.identity.peer_id())?)
    }

    /// Starts signing in with an OpenID Connect provider on another device
    /// (no browser here): show the code and page it gives, then
    /// [`DeviceSignIn::finish`] waits for the ID token.
    pub fn oidc_device(&self, provider: &OidcProviderInfo) -> Result<DeviceSignIn, ClientError> {
        Ok(DeviceSignIn::start(provider)?)
    }

    /// Whether `host_id` (hex) can be looked for away from the LAN: it is
    /// paired and told this client its discovery ID.
    pub fn reachable_away(&self, host_id: &str) -> bool {
        windowcast_identity::PeerId::from_hex(host_id).is_ok_and(|peer| {
            self.trust.lock().expect("trust store").is_pinned(&peer)
                && self.remote_hosts.get(&peer).is_some()
        })
    }

    /// Connects to a paired host away from the LAN through Syncthing's
    /// global discovery and the STUN servers `config` names: announces
    /// this client's address, looks the host up, and punches through both
    /// NATs to it (`windowcast_transport::remote`). Blocks, up to a minute
    /// and a half while the host's next lookup finds this client.
    pub fn connect_away(
        &self,
        host_id: &str,
        config: &RemoteConfig,
    ) -> Result<ClientSession, ClientError> {
        let directory = Syncthing::new(&self.identity, &config.servers)
            .map_err(|e| ClientError::NotFound(host_id.to_owned(), e))?;
        self.connect_away_with(host_id, config, &directory, CONNECT_WITHIN)
    }

    /// [`Self::connect_away`] with another directory and time limit.
    pub fn connect_away_with(
        &self,
        host_id: &str,
        config: &RemoteConfig,
        directory: &dyn Directory,
        within: Duration,
    ) -> Result<ClientSession, ClientError> {
        let peer = windowcast_identity::PeerId::from_hex(host_id)?;
        let trusted = self.trust.lock().expect("trust store").clone();
        let host_discovery = self
            .remote_hosts
            .get(&peer)
            .filter(|_| trusted.is_pinned(&peer))
            .ok_or_else(|| ClientError::NotReachableAway(host_id.to_owned()))?;
        let identity = Arc::clone(&self.identity);
        let established = self.runtime.block_on(async {
            let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
            let rendezvous = windowcast_transport::punched::Rendezvous::new(socket, false);
            let config_copy = config.clone();
            let stun = tokio::task::spawn_blocking(move || config_copy.stun_addresses())
                .await
                .unwrap_or_default();
            let mapped = remote::mapped_address(&rendezvous, &stun, Duration::from_secs(3)).await;
            // So the host's lookups find this client and punch towards it.
            // Discovery blocks on https; the runtime's other worker keeps
            // the rendezvous socket read meanwhile.
            let _ = tokio::task::block_in_place(|| directory.announce(&remote::announced(mapped)));
            let endpoints = match tokio::task::block_in_place(|| directory.lookup(&host_discovery))
            {
                Answer::Found(addresses) => remote::endpoints(&addresses),
                _ => Vec::new(),
            };
            if endpoints.is_empty() {
                return Err(ClientError::NotFound(
                    host_id.to_owned(),
                    "discovery has no address for it".into(),
                ));
            }
            let deadline = std::time::Instant::now() + within;
            let stream = loop {
                let mut last = None;
                for endpoint in &endpoints {
                    match rendezvous.connect(*endpoint, Duration::from_secs(4)).await {
                        Ok(stream) => {
                            last = Some(Ok(stream));
                            break;
                        }
                        Err(e) => last = Some(Err(e)),
                    }
                }
                match last {
                    Some(Ok(stream)) => break stream,
                    Some(Err(e)) if std::time::Instant::now() >= deadline => {
                        return Err(ClientError::NotFound(host_id.to_owned(), e.to_string()));
                    }
                    _ => {}
                }
            };
            let session = Session::away(&config.stun_names()).await?;
            Ok::<_, ClientError>(
                connect(
                    stream,
                    session,
                    &identity,
                    ClientCredential::Pinned(&trusted),
                )
                .await?,
            )
        })?;
        Ok(self.started(established, None))
    }

    /// Stops trusting the host whose identity is `host_id` (hex, as
    /// [`ClientSession::host_id`] gives it): it has to be paired again.
    pub fn forget(&self, host_id: &str) -> Result<(), ClientError> {
        let peer = windowcast_identity::PeerId::from_hex(host_id)?;
        let mut trust = self.trust.lock().expect("trust store");
        trust.revoke(&peer);
        trust.save(&self.trust_path)?;
        Ok(())
    }
}

/// Something that happened on a session. Serialized as JSON for the C
/// interface, tagged by `type`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Windows {
        windows: Vec<WindowInfo>,
    },
    StreamStarted {
        window: u64,
        backend: BackendKind,
        /// The video codec, for backends that send video on the session.
        codec: Option<VideoCodec>,
    },
    StreamRefused {
        window: u64,
        reason: String,
    },
    StreamStopped {
        window: u64,
    },
    /// A switch's new carrier is up ([`ClientSession::switch_window`]):
    /// show it beside the one on screen, and swap to it on its first
    /// picture, then call [`ClientSession::carrier_shown`]. Its frames
    /// come through [`ClientSession::next_frame`] (a backend with a codec)
    /// or [`ClientSession::next_picture`] (RDP), as for a start.
    CarrierStarted {
        window: u64,
        generation: u32,
        backend: BackendKind,
        codec: Option<VideoCodec>,
    },
    /// A switch did not happen (the host refused it, it could not
    /// connect, or it showed nothing in time): the window stays on the
    /// carrier it is on.
    CarrierRefused {
        window: u64,
        generation: u32,
        reason: String,
    },
    WindowResized {
        window: u64,
        width: u32,
        height: u32,
    },
    WindowFocused {
        window: u64,
    },
    /// The host's clipboard text changed.
    Clipboard {
        text: String,
    },
    /// The answer to [`ClientSession::request_ssh_certificate`]: an
    /// OpenSSH certificate line, or why there is none.
    SshCertificate {
        certificate: Option<String>,
        error: Option<String>,
    },
    /// What a stream is sent at now and the network the host sees: on
    /// every change and every few seconds.
    StreamQuality {
        window: u64,
        quality: StreamQuality,
    },
    Closed,
}

/// Opus packets queued per window before the oldest are dropped: a second
/// of sound, more than any player buffers.
const AUDIO_QUEUE: usize = 50;

/// The result of waiting for a window's sound.
pub enum AudioPoll {
    /// One Opus packet (48 kHz, stereo), 20 ms of sound.
    Packet(AudioPacket),
    /// Nothing within the timeout: the window is quiet, or it has no
    /// audio (yet, or at all).
    Timeout,
    /// The window's audio ended.
    Ended,
}

/// The result of waiting for an RDP window's picture.
pub enum PicturePoll {
    Picture(RgbaPicture),
    Timeout,
    /// The window is not streamed over RDP (any more).
    Ended,
}

/// The result of waiting for a frame.
pub enum FramePoll {
    Frame(WindowFrame),
    Timeout,
    /// The window's stream ended, or was never started.
    Ended,
}

struct FrameQueue {
    frames: Receiver<WindowFrame>,
    /// A frame a caller could not take (its buffer was too small); handed
    /// out again first.
    held: Option<WindowFrame>,
}

/// One streamed window's frames. Created when the host accepts the stream
/// (before its track delivers anything), so a client can start waiting
/// for frames as soon as it sees [`Event::StreamStarted`].
struct WindowSlot {
    queue: Mutex<FrameQueue>,
    /// Taken by the frame pump when the window's track arrives; dropping it
    /// ends the queue.
    sender: Mutex<Option<SyncSender<WindowFrame>>>,
    /// Set by [`ClientSession::request_keyframe`]; the frame pump asks the
    /// host for one with the next frame it sees.
    keyframe: Arc<AtomicBool>,
}

struct Shared {
    events: Mutex<Receiver<Event>>,
    slots: Mutex<HashMap<WindowId, Arc<WindowSlot>>>,
    /// Each window's sound, from its audio track's arrival to its end.
    audio: Mutex<HashMap<WindowId, Arc<Mutex<Receiver<AudioPacket>>>>>,
    /// When the unanswered ping went out, and the last round trip.
    ping: Mutex<(Option<Instant>, Option<Duration>)>,
    /// The ceilings this client set per window, sent with each start.
    limits: Mutex<HashMap<WindowId, StreamLimits>>,
    /// Where the host is, for backends with their own connection.
    host_ip: Option<std::net::IpAddr>,
    /// Windows streamed over RDP: their connections.
    rdp: Mutex<HashMap<WindowId, Arc<RdpStream>>>,
    /// The window keys and text go to: the last one a pointer event named.
    focus: Mutex<Option<WindowId>>,
    /// A picture a caller could not take (its buffer was too small),
    /// handed out again first.
    held_pictures: Mutex<HashMap<WindowId, RgbaPicture>>,
    /// The command stream's channels (shells, commands, launches).
    channels: Arc<command::Routes>,
    /// Where events go, for those that start on this side (RemoteApps).
    event_tx: Mutex<mpsc::Sender<Event>>,
    /// RemoteApp connections, and the next one's key.
    remote_apps: Mutex<Vec<remote_app::Conn>>,
    remote_keys: AtomicU32,
    /// RemoteApp windows being shown, with the number of the last picture
    /// each was given.
    remote_viewing: Mutex<HashMap<WindowId, u64>>,
    /// The password this client signed in with, for RemoteApps logging in
    /// as the same Windows user. In memory only.
    sign_in_password: Mutex<Option<String>>,
    /// The last carrier generation asked for, per window.
    generations: Mutex<HashMap<WindowId, u32>>,
    /// Each window's carrier on screen and, during a switch, the next.
    carriers: Mutex<HashMap<WindowId, Carriers>>,
}

/// One carrier of a window's stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Carrier {
    generation: u32,
    backend: BackendKind,
}

#[derive(Debug, Default)]
struct Carriers {
    shown: Option<Carrier>,
    next: Option<Carrier>,
}

/// How long a switch's new carrier has to show its first picture.
const SWITCH_WITHIN: Duration = Duration::from_secs(5);

impl Shared {
    /// Whether `generation` is the carrier a switch of `window` waits for.
    fn switching(&self, window: WindowId, generation: u32) -> bool {
        self.carriers
            .lock()
            .expect("carriers")
            .get(&window)
            .and_then(|c| c.next)
            .is_some_and(|next| next.generation == generation)
    }

    /// A window's first carrier is up.
    fn started(&self, window: WindowId, generation: u32, backend: BackendKind) {
        self.carriers
            .lock()
            .expect("carriers")
            .entry(window)
            .or_default()
            .shown = Some(Carrier {
            generation,
            backend,
        });
    }

    /// Ends a switch that did not happen: its carrier stops and the window
    /// stays on the one shown.
    fn abandon_switch(
        &self,
        (runtime, session): (&tokio::runtime::Handle, &Session),
        window: WindowId,
        generation: u32,
        reason: String,
    ) {
        let next = {
            let mut carriers = self.carriers.lock().expect("carriers");
            let Some(entry) = carriers.get_mut(&window) else {
                return;
            };
            if entry.next.is_none_or(|n| n.generation != generation) {
                return;
            }
            entry.next.take()
        };
        if next.is_some_and(|n| n.backend == BackendKind::Rdp) {
            self.rdp.lock().expect("rdp").remove(&window);
        }
        let stop = ControlMessage::CarrierStop { window, generation };
        let _ = runtime.block_on(session.send_control(&stop));
        self.emit(Event::CarrierRefused {
            window: window.0,
            generation,
            reason,
        });
    }

    /// The next carrier generation of `window`'s stream.
    fn next_generation(&self, window: WindowId) -> u32 {
        let mut generations = self.generations.lock().expect("generations");
        let next = generations.entry(window).or_insert(0);
        *next += 1;
        *next
    }

    fn emit(&self, event: Event) {
        let _ = self.event_tx.lock().expect("events").send(event);
    }

    fn slot(&self, window: WindowId) -> Arc<WindowSlot> {
        Arc::clone(
            self.slots
                .lock()
                .expect("slots")
                .entry(window)
                .or_insert_with(new_slot),
        )
    }

    fn existing_slot(&self, window: WindowId) -> Option<Arc<WindowSlot>> {
        self.slots.lock().expect("slots").get(&window).cloned()
    }

    /// The sender for a newly arrived track: the waiting slot's, or a
    /// fresh slot's when that one's sender is already in use.
    fn take_sender(&self, window: WindowId) -> (SyncSender<WindowFrame>, Arc<AtomicBool>) {
        let mut slots = self.slots.lock().expect("slots");
        let slot = slots.entry(window).or_insert_with(new_slot);
        if let Some(sender) = slot.sender.lock().expect("sender").take() {
            return (sender, Arc::clone(&slot.keyframe));
        }
        let fresh = new_slot();
        let sender = fresh
            .sender
            .lock()
            .expect("sender")
            .take()
            .expect("new slot");
        let keyframe = Arc::clone(&fresh.keyframe);
        *slot = fresh;
        (sender, keyframe)
    }
}

/// Whether a failed resume is one a PIN could fix: the host is not pinned
/// on this device, or the host does not know this device (it refuses a
/// resume with the same message whatever the cause).
fn pairing_could_help(error: &TransportError) -> bool {
    matches!(
        error,
        TransportError::UnknownPeer | TransportError::Rejected(_)
    )
}

fn new_slot() -> Arc<WindowSlot> {
    let (tx, rx) = mpsc::sync_channel(FRAME_QUEUE);
    Arc::new(WindowSlot {
        queue: Mutex::new(FrameQueue {
            frames: rx,
            held: None,
        }),
        sender: Mutex::new(Some(tx)),
        keyframe: Arc::new(AtomicBool::new(false)),
    })
}

/// A connected session, seen from the client.
pub struct ClientSession {
    runtime: Arc<Runtime>,
    session: Arc<Session>,
    shared: Arc<Shared>,
    host: String,
    paired: bool,
    /// The user's backend rules, checked before the defaults.
    rules: Mutex<Vec<BackendRule>>,
    /// Whether this client shows RGBA pictures (an RDP window); until it
    /// says so, windows the rules send to RDP are asked for natively.
    pictures: AtomicBool,
    /// The last window list, for choosing backends by content.
    windows: Arc<Mutex<Vec<WindowInfo>>>,
    /// The microphone track and its encoder, while the microphone is on.
    microphone: Mutex<Option<Microphone>>,
    /// The windows this client shows (asked for, or followed), with the
    /// codecs each was asked for in.
    shown: Mutex<HashMap<WindowId, Vec<VideoCodec>>>,
    /// Whether the dialogs, popups and menus a shown window owns are shown
    /// too, as they open ([`Self::set_follow_popups`]).
    follow_popups: AtomicBool,
    /// Whether carriers switch by themselves ([`Self::set_auto_switch`]).
    auto_switch: AtomicBool,
    /// The selector's view of each shown window, and when it last looked.
    selection: Mutex<(HashMap<WindowId, selector::Window>, Option<Instant>)>,
    /// Frames each window's video has handed out, for the selector.
    frames_seen: Mutex<HashMap<WindowId, u64>>,
}

impl ClientSession {
    fn start(
        runtime: Arc<Runtime>,
        session: Session,
        host: windowcast_identity::PeerId,
        paired: bool,
        rendezvous: Rendezvous,
        host_ip: Option<std::net::IpAddr>,
    ) -> Self {
        let session = Arc::new(session);
        if let Some(discovery_id) = rendezvous.own.clone() {
            let session = Arc::clone(&session);
            runtime.spawn(async move {
                let _ = session
                    .send_control(&ControlMessage::Rendezvous { discovery_id })
                    .await;
            });
        }
        let (event_tx, event_rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            events: Mutex::new(event_rx),
            slots: Mutex::new(HashMap::new()),
            audio: Mutex::new(HashMap::new()),
            ping: Mutex::new((None, None)),
            limits: Mutex::default(),
            host_ip,
            rdp: Mutex::default(),
            focus: Mutex::default(),
            held_pictures: Mutex::default(),
            channels: Arc::default(),
            event_tx: Mutex::new(event_tx.clone()),
            remote_apps: Mutex::default(),
            remote_keys: AtomicU32::new(1),
            remote_viewing: Mutex::default(),
            sign_in_password: Mutex::default(),
            generations: Mutex::default(),
            carriers: Mutex::default(),
        });
        let windows = Arc::new(Mutex::new(Vec::new()));

        runtime.spawn(pump_events(
            Arc::clone(&session),
            Arc::clone(&shared),
            event_tx,
            Arc::clone(&windows),
            (host, rendezvous.peers),
        ));
        runtime.spawn(pump_windows(Arc::clone(&session), Arc::clone(&shared)));

        ClientSession {
            runtime,
            session,
            shared,
            host: host.to_hex(),
            paired,
            rules: Mutex::new(Vec::new()),
            pictures: AtomicBool::new(false),
            windows,
            microphone: Mutex::new(None),
            shown: Mutex::default(),
            follow_popups: AtomicBool::new(true),
            auto_switch: AtomicBool::new(true),
            selection: Mutex::default(),
            frames_seen: Mutex::default(),
        }
    }

    /// The host's identity.
    pub fn host_id(&self) -> &str {
        &self.host
    }

    /// Whether this connection paired by PIN (the host is now pinned).
    pub fn paired(&self) -> bool {
        self.paired
    }

    /// Says this client shows windows as RGBA pictures ([`Self::next_picture`]),
    /// so windows its rules send to RDP are asked for over RDP.
    pub fn accept_pictures(&self, on: bool) {
        self.pictures.store(on, Ordering::SeqCst);
    }

    /// Whether the dialogs, popups and menus a shown window owns are shown
    /// too, each as its own window, as they open (on by default). Off, they
    /// are still in the window list for the user to pick.
    pub fn set_follow_popups(&self, on: bool) {
        self.follow_popups.store(on, Ordering::SeqCst);
    }

    /// Starts showing the windows owned by shown windows that are not
    /// shown yet, and forgets shown windows no longer listed.
    fn follow(&self, windows: &[WindowInfo]) {
        let starts: Vec<(WindowId, Vec<VideoCodec>)> = {
            let mut shown = self.shown.lock().expect("shown");
            shown.retain(|id, _| windows.iter().any(|w| w.id == *id));
            if !self.follow_popups.load(Ordering::SeqCst) {
                return;
            }
            windows
                .iter()
                .filter(|w| !shown.contains_key(&w.id))
                .filter_map(|w| Some((w.id, shown.get(&w.owner?)?.clone())))
                .collect()
        };
        for (window, codecs) in starts {
            let _ = self.start_window(window, &codecs);
        }
    }

    /// Replaces the user's backend rules (per-app overrides and the like).
    pub fn set_rules(&self, rules: Vec<BackendRule>) {
        *self.rules.lock().expect("rules") = rules;
    }

    /// Asks for the window list; it arrives as [`Event::Windows`].
    pub fn request_windows(&self) -> Result<(), ClientError> {
        self.send(ControlMessage::ListWindowsRequest)
    }

    /// Asks to stream `window`, decodable in `codecs` (most preferred
    /// first). The backend comes from the user's rules, then the defaults;
    /// the answer arrives as [`Event::StreamStarted`] or
    /// [`Event::StreamRefused`], and frames through [`Self::next_frame`].
    pub fn start_window(&self, window: WindowId, codecs: &[VideoCodec]) -> Result<(), ClientError> {
        self.shown
            .lock()
            .expect("shown")
            .insert(window, codecs.to_vec());
        if remote_app::split(window).is_some() {
            self.start_remote_window(window);
            return Ok(());
        }
        let backend = {
            let windows = self.windows.lock().expect("windows");
            let rules = self.rules.lock().expect("rules");
            windows
                .iter()
                .find(|info| info.id == window)
                .map_or(BackendKind::Native, |info| {
                    selection::choose_backend(info, &rules)
                })
        };
        // RDP is its own TCP connection to the host, which only a host on
        // this network can take, and its windows come as pictures, which
        // the client must show.
        let backend = match backend {
            BackendKind::Rdp
                if self.shared.host_ip.is_none() || !self.pictures.load(Ordering::SeqCst) =>
            {
                BackendKind::Native
            }
            other => other,
        };
        let limits = self
            .shared
            .limits
            .lock()
            .expect("limits")
            .get(&window)
            .copied()
            .unwrap_or_default();
        self.send(ControlMessage::StreamStartRequest {
            generation: self.shared.next_generation(window),
            target: StreamTarget::Window(window),
            options: StreamOptions {
                backend,
                codecs: codecs.to_vec(),
                limits,
            },
        })
    }

    /// Whether a window's carrier switches by itself as what it shows and
    /// the connection change (docs/BACKENDS.md, "When a carrier switches";
    /// on by default). A window whose app has a rule of the user's, or that
    /// the user switched by hand, keeps its carrier.
    pub fn set_auto_switch(&self, on: bool) {
        self.auto_switch.store(on, Ordering::SeqCst);
    }

    /// Looks at every shown window, at most twice a second, and starts the
    /// switches the selector chooses.
    fn select(&self) {
        let now = Instant::now();
        {
            let mut selection = self.selection.lock().expect("selection");
            if selection
                .1
                .is_some_and(|at| now.duration_since(at) < Duration::from_millis(500))
            {
                return;
            }
            selection.1 = Some(now);
        }
        if !self.auto_switch.load(Ordering::SeqCst) {
            return;
        }
        let shown: Vec<(WindowId, Carrier)> = self
            .shared
            .carriers
            .lock()
            .expect("carriers")
            .iter()
            .filter(|(_, c)| c.next.is_none())
            .filter_map(|(w, c)| Some((*w, c.shown?)))
            .collect();
        let rdp_possible = self.shared.host_ip.is_some() && self.pictures.load(Ordering::SeqCst);
        for (window, carrier) in shown {
            let Some(info) = self
                .windows
                .lock()
                .expect("windows")
                .iter()
                .find(|w| w.id == window)
                .cloned()
            else {
                continue;
            };
            let pinned = self
                .rules
                .lock()
                .expect("rules")
                .iter()
                .any(|rule| rule.when.matches(&info));
            let count = if carrier.backend == BackendKind::Rdp {
                self.shared
                    .rdp
                    .lock()
                    .expect("rdp")
                    .get(&window)
                    .map_or(0, |rdp| rdp.pictures())
            } else {
                self.frames_seen
                    .lock()
                    .expect("frames seen")
                    .get(&window)
                    .copied()
                    .unwrap_or(0)
            };
            let choice = {
                let mut selection = self.selection.lock().expect("selection");
                let state = selection
                    .0
                    .entry(window)
                    .or_insert_with(|| selector::Window::new(now));
                state.pinned |= pinned;
                state.arrived(count, now);
                if state.pending.is_some() {
                    None
                } else {
                    state.choose(carrier.backend, info.content, rdp_possible, now)
                }
            };
            let Some(kind) = choice else {
                continue;
            };
            let codecs = self
                .shown
                .lock()
                .expect("shown")
                .get(&window)
                .cloned()
                .unwrap_or_default();
            let started = self.start_switch(window, kind, &codecs);
            let mut selection = self.selection.lock().expect("selection");
            if let Some(state) = selection.0.get_mut(&window) {
                match started {
                    Ok(generation) => {
                        state.pending = Some((generation, kind));
                        state.switched(kind, now);
                    }
                    Err(_) => state.failed(kind, now),
                }
            }
        }
    }

    /// The selector's bookkeeping for an event about a window.
    fn selection_event(&self, event: &Event) {
        let mut selection = self.selection.lock().expect("selection");
        match event {
            Event::CarrierStarted { .. } => {}
            Event::CarrierRefused {
                window, generation, ..
            } => {
                if let Some(state) = selection.0.get_mut(&WindowId(*window)) {
                    if let Some((pending, kind)) = state.pending {
                        if pending == *generation {
                            state.pending = None;
                            state.failed(kind, Instant::now());
                        }
                    }
                }
            }
            Event::StreamStopped { window } => {
                selection.0.remove(&WindowId(*window));
                self.frames_seen
                    .lock()
                    .expect("frames seen")
                    .remove(&WindowId(*window));
            }
            _ => {}
        }
    }

    /// Switches a streamed window to `backend` without a break
    /// (docs/BACKENDS.md, "One window, any carrier"): the new carrier
    /// starts beside the one on screen, arrives as
    /// [`Event::CarrierStarted`], and replaces it once the app has shown its
    /// first picture ([`Self::carrier_shown`]); [`Event::CarrierRefused`]
    /// says it did not happen. Returns the new carrier's generation.
    ///
    /// A switch by hand pins the window: the selector leaves its carrier
    /// alone from then on.
    pub fn switch_window(
        &self,
        window: WindowId,
        backend: BackendKind,
        codecs: &[VideoCodec],
    ) -> Result<u32, ClientError> {
        self.selection
            .lock()
            .expect("selection")
            .0
            .entry(window)
            .or_insert_with(|| selector::Window::new(Instant::now()))
            .pinned = true;
        self.start_switch(window, backend, codecs)
    }

    fn start_switch(
        &self,
        window: WindowId,
        backend: BackendKind,
        codecs: &[VideoCodec],
    ) -> Result<u32, ClientError> {
        if remote_app::split(window).is_some() {
            return Err(ClientError::Refused(
                "a RemoteApp window has only its RDP connection".into(),
            ));
        }
        if backend == BackendKind::Rdp
            && (self.shared.host_ip.is_none() || !self.pictures.load(Ordering::SeqCst))
        {
            return Err(ClientError::Refused(
                "RDP needs the host on this network and a client that shows pictures".into(),
            ));
        }
        let generation = {
            let mut carriers = self.shared.carriers.lock().expect("carriers");
            let entry = carriers.entry(window).or_default();
            let Some(shown) = entry.shown else {
                return Err(ClientError::Refused("that window is not streaming".into()));
            };
            if entry.next.is_some() {
                return Err(ClientError::Refused(
                    "that window is already switching".into(),
                ));
            }
            if shown.backend == backend {
                return Err(ClientError::Refused(format!(
                    "that window is already on {backend:?}"
                )));
            }
            let generation = self.shared.next_generation(window);
            entry.next = Some(Carrier {
                generation,
                backend,
            });
            generation
        };
        let limits = self
            .shared
            .limits
            .lock()
            .expect("limits")
            .get(&window)
            .copied()
            .unwrap_or_default();
        self.send(ControlMessage::StreamStartRequest {
            target: StreamTarget::Window(window),
            options: StreamOptions {
                backend,
                codecs: codecs.to_vec(),
                limits,
            },
            generation,
        })?;
        // A carrier that shows nothing in time is given up.
        let shared = Arc::downgrade(&self.shared);
        let session = Arc::clone(&self.session);
        let runtime = self.runtime.handle().clone();
        std::thread::spawn(move || {
            std::thread::sleep(SWITCH_WITHIN);
            if let Some(shared) = shared.upgrade() {
                shared.abandon_switch(
                    (&runtime, &session),
                    window,
                    generation,
                    format!("no picture within {} s", SWITCH_WITHIN.as_secs()),
                );
            }
        });
        Ok(generation)
    }

    /// The app showed the first picture of the carrier a switch started
    /// ([`Event::CarrierStarted`]): it replaces the old one, which stops.
    pub fn carrier_shown(&self, window: WindowId, generation: u32) -> Result<(), ClientError> {
        // An automatic switch is done: the selector looks again.
        if let Some(state) = self.selection.lock().expect("selection").0.get_mut(&window) {
            if state.pending.is_some_and(|(g, _)| g == generation) {
                state.pending = None;
            }
        }
        let old = {
            let mut carriers = self.shared.carriers.lock().expect("carriers");
            let Some(entry) = carriers.get_mut(&window) else {
                return Ok(());
            };
            match entry.next {
                Some(next) if next.generation == generation => {
                    entry.next = None;
                    entry.shown.replace(next)
                }
                _ => return Ok(()),
            }
        };
        if let Some(old) = old {
            if old.backend == BackendKind::Rdp {
                self.shared.rdp.lock().expect("rdp").remove(&window);
            }
            self.send(ControlMessage::CarrierStop {
                window,
                generation: old.generation,
            })?;
        }
        Ok(())
    }

    /// Sets this client's ceilings for a window's stream: kept for its
    /// next start, and sent at once to a stream already running (the host
    /// ignores it for a window it is not streaming). The host adapts below
    /// them to what the network carries.
    pub fn set_stream_limits(
        &self,
        window: WindowId,
        limits: StreamLimits,
    ) -> Result<(), ClientError> {
        self.shared
            .limits
            .lock()
            .expect("limits")
            .insert(window, limits);
        self.send(ControlMessage::StreamLimits { window, limits })
    }

    /// Sends input to the host. Pointer and touch events go to the window
    /// they name (one this session streams); keys, text and gamepads to the
    /// last such window.
    pub fn send_input(&self, event: InputEvent) -> Result<(), ClientError> {
        let named = input_window(&event);
        let window = {
            let mut focus = self.shared.focus.lock().expect("focus");
            if named.is_some() {
                *focus = named;
            }
            *focus
        };
        // Input goes over the session whatever carries the window's
        // picture, under the session's rules (docs/BACKENDS.md, "One window,
        // any carrier"). Only a RemoteApp window of this client's own
        // (opt-in Windows RemoteApp) is driven over its RDP connection;
        // gamepads always go over the session.
        let pointer_or_keys = !matches!(
            event,
            InputEvent::Gamepad { .. } | InputEvent::GamepadGone { .. }
        );
        if pointer_or_keys {
            if let Some(w) = window.filter(|w| remote_app::split(*w).is_some()) {
                self.remote_input(w, event);
                return Ok(());
            }
        }
        self.send(ControlMessage::Input(event))
    }

    /// The newest picture of a window streamed over RDP, whether or not
    /// [`Self::next_picture`] handed it out: where a view that opens late
    /// starts, since a window that does not change sends nothing.
    pub fn latest_picture(&self, window: WindowId) -> Option<RgbaPicture> {
        if remote_app::split(window).is_some() {
            return self.remote_latest_picture(window);
        }
        let rdp = self.shared.rdp.lock().expect("rdp").get(&window).cloned()?;
        rdp.latest_picture()
    }

    /// The next picture of a window streamed over RDP (whole, RGBA),
    /// waiting up to `timeout`. A picture comes whenever the window
    /// changes; a client behind gets the newest.
    pub fn next_picture(&self, window: WindowId, timeout: Duration) -> PicturePoll {
        if let Some(picture) = self
            .shared
            .held_pictures
            .lock()
            .expect("held pictures")
            .remove(&window)
        {
            return PicturePoll::Picture(picture);
        }
        if remote_app::split(window).is_some() {
            return self.remote_next_picture(window, timeout);
        }
        let Some(rdp) = self.shared.rdp.lock().expect("rdp").get(&window).cloned() else {
            return PicturePoll::Ended;
        };
        match rdp.next_picture(timeout) {
            Ok(picture) => PicturePoll::Picture(picture),
            Err(RecvTimeoutError::Timeout) => PicturePoll::Timeout,
            Err(RecvTimeoutError::Disconnected) => PicturePoll::Ended,
        }
    }

    /// Gives the host this client's clipboard text.
    /// Asks the host to sign an SSH user certificate for `public_key` (an
    /// OpenSSH public key line), for the account this client signed in
    /// with; it arrives as an [`Event::SshCertificate`].
    pub fn request_ssh_certificate(&self, public_key: &str) -> Result<(), ClientError> {
        self.send(ControlMessage::SshCertificateRequest {
            public_key: public_key.to_owned(),
        })
    }

    pub fn set_clipboard(&self, text: &str) -> Result<(), ClientError> {
        self.send(ControlMessage::Clipboard(text.to_owned()))
    }

    pub fn stop_window(&self, window: WindowId) -> Result<(), ClientError> {
        // The windows it owns go with it.
        let owned: Vec<WindowId> = {
            let mut shown = self.shown.lock().expect("shown");
            shown.remove(&window);
            let listed = self.windows.lock().expect("windows");
            listed
                .iter()
                .filter(|w| w.owner == Some(window) && shown.contains_key(&w.id))
                .map(|w| w.id)
                .collect()
        };
        for popup in owned {
            let _ = self.stop_window(popup);
        }
        if remote_app::split(window).is_some() {
            self.stop_remote_window(window);
            return Ok(());
        }
        self.shared.rdp.lock().expect("rdp").remove(&window);
        self.send(ControlMessage::StreamStopRequest(StreamTarget::Window(
            window,
        )))
    }

    /// The next session event, waiting up to `timeout`. `None` on timeout.
    /// After [`Event::Closed`] every call returns `Closed`.
    pub fn next_event(&self, timeout: Duration) -> Option<Event> {
        self.select();
        match self
            .shared
            .events
            .lock()
            .expect("events")
            .recv_timeout(timeout)
        {
            Ok(event) => {
                self.selection_event(&event);
                match &event {
                    Event::Windows { windows } => self.follow(windows),
                    // A refused window stays counted, so it is not followed
                    // again at every list.
                    Event::StreamStopped { window } => {
                        self.shown.lock().expect("shown").remove(&WindowId(*window));
                    }
                    _ => {}
                }
                Some(event)
            }
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(Event::Closed),
        }
    }

    /// The next frame of `window`, waiting up to `timeout`.
    pub fn next_frame(&self, window: WindowId, timeout: Duration) -> FramePoll {
        let Some(slot) = self.shared.existing_slot(window) else {
            return FramePoll::Ended;
        };
        let mut queue = slot.queue.lock().expect("frame queue");
        if let Some(frame) = queue.held.take() {
            return FramePoll::Frame(frame);
        }
        match queue.frames.recv_timeout(timeout) {
            Ok(frame) => {
                *self
                    .frames_seen
                    .lock()
                    .expect("frames seen")
                    .entry(window)
                    .or_insert(0) += 1;
                FramePoll::Frame(frame)
            }
            Err(RecvTimeoutError::Timeout) => FramePoll::Timeout,
            Err(RecvTimeoutError::Disconnected) => {
                drop(queue);
                let mut slots = self.shared.slots.lock().expect("slots");
                if slots
                    .get(&window)
                    .is_some_and(|current| Arc::ptr_eq(current, &slot))
                {
                    slots.remove(&window);
                }
                FramePoll::Ended
            }
        }
    }

    /// The next Opus packet of `window`'s sound, waiting up to `timeout`.
    /// A window's audio track arrives a little after its stream starts, and
    /// only if the host captures its sound; until then this times out.
    pub fn next_audio(&self, window: WindowId, timeout: Duration) -> AudioPoll {
        let queue = self
            .shared
            .audio
            .lock()
            .expect("audio")
            .get(&window)
            .cloned();
        let Some(queue) = queue else {
            std::thread::sleep(timeout.min(Duration::from_millis(50)));
            return AudioPoll::Timeout;
        };
        let received = queue.lock().expect("audio queue").recv_timeout(timeout);
        match received {
            Ok(packet) => AudioPoll::Packet(packet),
            Err(RecvTimeoutError::Timeout) => AudioPoll::Timeout,
            Err(RecvTimeoutError::Disconnected) => {
                let mut audio = self.shared.audio.lock().expect("audio");
                if audio
                    .get(&window)
                    .is_some_and(|current| Arc::ptr_eq(current, &queue))
                {
                    audio.remove(&window);
                }
                AudioPoll::Ended
            }
        }
    }

    /// Puts a frame back to be returned by the next [`Self::next_frame`].
    pub(crate) fn hold_picture(&self, window: WindowId, picture: RgbaPicture) {
        self.shared
            .held_pictures
            .lock()
            .expect("held pictures")
            .insert(window, picture);
    }

    pub(crate) fn hold_frame(&self, window: WindowId, frame: WindowFrame) {
        if let Some(slot) = self.shared.existing_slot(window) {
            slot.queue.lock().expect("frame queue").held = Some(frame);
        }
    }

    /// Asks the host for a keyframe of `window`, e.g. after the client
    /// reset its decoder; it comes within about one frame.
    pub fn request_keyframe(&self, window: WindowId) {
        if let Some(slot) = self.shared.existing_slot(window) {
            slot.keyframe.store(true, Ordering::SeqCst);
        }
    }

    /// Starts sending this client's microphone to the host: sound given to
    /// [`Self::send_microphone`] reaches the host's virtual microphone
    /// (where the host has one and allows it).
    pub fn start_microphone(&self) -> Result<(), ClientError> {
        let track = self.runtime.block_on(self.session.attach_microphone())?;
        let encoder = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Voip)
            .map_err(|e| ClientError::Audio(e.to_string()))?;
        *self.microphone.lock().expect("microphone") = Some(Microphone {
            track,
            encoder,
            pending: Vec::new(),
        });
        Ok(())
    }

    /// Sends microphone sound: interleaved stereo 16-bit samples at 48 kHz,
    /// any amount at a time. It goes out as 20 ms Opus packets (libopus,
    /// the same on every client) as soon as each is whole.
    pub fn send_microphone(&self, samples: &[i16]) -> Result<(), ClientError> {
        let packets = {
            let mut microphone = self.microphone.lock().expect("microphone");
            let Some(microphone) = microphone.as_mut() else {
                return Err(ClientError::Transport(TransportError::Closed));
            };
            microphone.pending.extend_from_slice(samples);
            let mut packets = Vec::new();
            let mut out = vec![0u8; 4000];
            while microphone.pending.len() >= MICROPHONE_FRAME {
                let len = microphone
                    .encoder
                    .encode(&microphone.pending[..MICROPHONE_FRAME], &mut out)
                    .map_err(|e| ClientError::Audio(e.to_string()))?;
                microphone.pending.drain(..MICROPHONE_FRAME);
                packets.push(bytes::Bytes::copy_from_slice(&out[..len]));
            }
            (microphone.track.clone(), packets)
        };
        let (track, packets) = packets;
        for packet in packets {
            self.runtime
                .block_on(track.write_packet(packet, Duration::from_millis(20)))?;
        }
        Ok(())
    }

    /// Stops sending the microphone.
    pub fn stop_microphone(&self) -> Result<(), ClientError> {
        self.microphone.lock().expect("microphone").take();
        self.runtime.block_on(self.session.detach_microphone())?;
        Ok(())
    }

    /// Measures the round trip to the host over the control channel; the
    /// result is [`Self::round_trip`] once the host answers.
    pub fn ping(&self) -> Result<(), ClientError> {
        self.shared.ping.lock().expect("ping").0 = Some(Instant::now());
        self.send(ControlMessage::Ping)
    }

    /// The last round trip [`Self::ping`] measured.
    pub fn round_trip(&self) -> Option<Duration> {
        self.shared.ping.lock().expect("ping").1
    }

    /// Ends the session; the host is told at once.
    pub fn close(&self) {
        let _ = self.runtime.block_on(self.session.close());
    }

    fn send(&self, message: ControlMessage) -> Result<(), ClientError> {
        Ok(self.runtime.block_on(self.session.send_control(&message))?)
    }
}

impl Drop for ClientSession {
    fn drop(&mut self) {
        self.close();
    }
}

/// The window an input event names, if it names one.
fn input_window(event: &InputEvent) -> Option<WindowId> {
    match event {
        InputEvent::PointerMove { window, .. }
        | InputEvent::PointerButton { window, .. }
        | InputEvent::PointerScroll { window, .. }
        | InputEvent::Touch { window, .. } => Some(*window),
        _ => None,
    }
}

/// Logs in to the RDP server the host started for `window`, on a thread of
/// its own, then announces the stream (or its refusal, telling the host to
/// stop serving it).
fn connect_rdp(
    session: Arc<Session>,
    shared: Arc<Shared>,
    events: mpsc::Sender<Event>,
    (window, generation): (WindowId, u32),
    target: HandoffTarget,
    size: (u16, u16),
) {
    let runtime = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let address = match (target.address.parse::<std::net::IpAddr>(), shared.host_ip) {
            (Ok(ip), _) => Some(ip),
            (Err(_), Some(ip)) if target.address.is_empty() => Some(ip),
            _ => None,
        };
        let result = match address {
            Some(ip) => windowcast_rdp::client::connect(&windowcast_rdp::client::ClientConfig {
                address: std::net::SocketAddr::new(ip, target.port),
                server_name: ip.to_string(),
                username: target.username,
                password: target.password,
                domain: None,
                size,
                pinned: target.certificate_sha256,
                remote_app: None,
            })
            .map_err(|e| e.to_string()),
            None => Err("the host's RDP address is not reachable from here".to_owned()),
        };
        let switching = shared.switching(window, generation);
        let event = match result {
            Ok(stream) => {
                shared
                    .rdp
                    .lock()
                    .expect("rdp")
                    .insert(window, Arc::new(stream));
                if switching {
                    Event::CarrierStarted {
                        window: window.0,
                        generation,
                        backend: BackendKind::Rdp,
                        codec: None,
                    }
                } else {
                    shared.started(window, generation, BackendKind::Rdp);
                    Event::StreamStarted {
                        window: window.0,
                        backend: BackendKind::Rdp,
                        codec: None,
                    }
                }
            }
            Err(reason) if switching => {
                shared.abandon_switch(
                    (&runtime, &session),
                    window,
                    generation,
                    format!("RDP: {reason}"),
                );
                return;
            }
            Err(reason) => {
                runtime.spawn(async move {
                    let _ = session
                        .send_control(&ControlMessage::StreamStopRequest(StreamTarget::Window(
                            window,
                        )))
                        .await;
                });
                Event::StreamRefused {
                    window: window.0,
                    reason: format!("RDP: {reason}"),
                }
            }
        };
        let _ = events.send(event);
    });
}

async fn pump_events(
    session: Arc<Session>,
    shared: Arc<Shared>,
    events: mpsc::Sender<Event>,
    windows: Arc<Mutex<Vec<WindowInfo>>>,
    (host, remote_hosts): (windowcast_identity::PeerId, Arc<RemotePeers>),
) {
    loop {
        let message = match session.recv_control().await {
            Ok(message) => message,
            Err(_) => {
                shared.channels.close_all();
                let _ = events.send(Event::Closed);
                return;
            }
        };
        let event = match message {
            ControlMessage::Command(message) => {
                shared.channels.handle(message);
                continue;
            }
            ControlMessage::ListWindowsResponse(list) => {
                *windows.lock().expect("windows") = list.clone();
                let mut list = list;
                list.extend(remote_app::infos(&shared));
                Event::Windows { windows: list }
            }
            ControlMessage::StreamStartResponse {
                target: StreamTarget::Window(window),
                generation,
                accepted: true,
                backend: StreamBackend::Rdp,
                handoff: Some(target),
                ..
            } => {
                // Logging in takes a moment; the stream starts (or is
                // refused) when it is done.
                let size = windows
                    .lock()
                    .expect("windows")
                    .iter()
                    .find(|w| w.id == window)
                    .map_or((1280, 720), |w| (w.width as u16, w.height as u16));
                connect_rdp(
                    Arc::clone(&session),
                    Arc::clone(&shared),
                    events.clone(),
                    (window, generation),
                    target,
                    size,
                );
                continue;
            }
            ControlMessage::StreamStartResponse {
                target: StreamTarget::Window(window),
                generation,
                accepted,
                backend,
                reason,
                ..
            } => {
                let switching = shared.switching(window, generation);
                if switching && !accepted {
                    if let Some(entry) = shared.carriers.lock().expect("carriers").get_mut(&window)
                    {
                        entry.next = None;
                    }
                    Event::CarrierRefused {
                        window: window.0,
                        generation,
                        reason: reason.unwrap_or_default(),
                    }
                } else if switching {
                    if backend.session_codec().is_some() {
                        shared.slot(window);
                    }
                    Event::CarrierStarted {
                        window: window.0,
                        generation,
                        backend: backend.kind(),
                        codec: backend.session_codec(),
                    }
                } else if accepted {
                    if backend.session_codec().is_some() {
                        shared.slot(window);
                    }
                    shared.started(window, generation, backend.kind());
                    Event::StreamStarted {
                        window: window.0,
                        backend: backend.kind(),
                        codec: backend.session_codec(),
                    }
                } else {
                    Event::StreamRefused {
                        window: window.0,
                        reason: reason.unwrap_or_default(),
                    }
                }
            }
            ControlMessage::TrackEnded(window) => {
                // The window's video track ended; its stream may go on on
                // another carrier, so only the frame queue ends.
                let slot = shared.slots.lock().expect("slots").remove(&window);
                if let Some(slot) = slot {
                    slot.sender.lock().expect("sender").take();
                }
                continue;
            }
            ControlMessage::StreamStopped(StreamTarget::Window(window)) => {
                // A stream stopped before its track delivered anything still
                // holds its sender; dropping it ends the queue. The slot
                // leaves the map, so a new stream of the same window gets a
                // fresh queue instead of this ended one.
                let slot = shared.slots.lock().expect("slots").remove(&window);
                if let Some(slot) = slot {
                    slot.sender.lock().expect("sender").take();
                }
                shared.rdp.lock().expect("rdp").remove(&window);
                shared.carriers.lock().expect("carriers").remove(&window);
                Event::StreamStopped { window: window.0 }
            }
            ControlMessage::WindowResized {
                window,
                width,
                height,
            } => Event::WindowResized {
                window: window.0,
                width,
                height,
            },
            ControlMessage::WindowFocused(window) => Event::WindowFocused { window: window.0 },
            ControlMessage::Clipboard(text) => Event::Clipboard { text },
            ControlMessage::SshCertificateResponse(answer) => match answer {
                Ok(certificate) => Event::SshCertificate {
                    certificate: Some(certificate),
                    error: None,
                },
                Err(error) => Event::SshCertificate {
                    certificate: None,
                    error: Some(error),
                },
            },
            ControlMessage::Pong => {
                let mut ping = shared.ping.lock().expect("ping");
                if let Some(sent) = ping.0.take() {
                    ping.1 = Some(sent.elapsed());
                }
                continue;
            }
            // Kept for reaching this host away from the LAN later.
            ControlMessage::Rendezvous { discovery_id } => {
                let _ = remote_hosts.set(&host, &discovery_id);
                continue;
            }
            // The host measures the round trip for adaptive quality.
            ControlMessage::Ping => {
                if session.send_control(&ControlMessage::Pong).await.is_err() {
                    let _ = events.send(Event::Closed);
                    return;
                }
                continue;
            }
            ControlMessage::StreamQuality { window, quality } => Event::StreamQuality {
                window: window.0,
                quality,
            },
            _ => continue,
        };
        if events.send(event).is_err() {
            return;
        }
    }
}

/// Takes each window track as it arrives and pumps its frames into that
/// window's queue.
async fn pump_windows(session: Arc<Session>, shared: Arc<Shared>) {
    while let Ok(remote) = session.next_remote_track().await {
        match remote {
            RemoteTrack::Window(remote) => {
                let (sender, keyframe) = shared.take_sender(remote.window());
                tokio::spawn(pump_frames(remote, sender, keyframe));
            }
            RemoteTrack::Audio(remote) => {
                let (sender, receiver) = mpsc::sync_channel(AUDIO_QUEUE);
                shared
                    .audio
                    .lock()
                    .expect("audio")
                    .insert(remote.window(), Arc::new(Mutex::new(receiver)));
                tokio::spawn(pump_audio(remote, sender));
            }
            // Only hosts receive a microphone.
            RemoteTrack::Microphone(_) => {}
        }
    }
}

/// A window's Opus packets into its queue; a full queue (nobody playing)
/// drops the new packet rather than block the session.
async fn pump_audio(mut remote: RemoteAudio, queue: SyncSender<AudioPacket>) {
    while let Ok(packet) = remote.next_packet().await {
        if let Err(TrySendError::Disconnected(_)) = queue.try_send(packet) {
            return;
        }
    }
}

async fn pump_frames(
    mut remote: RemoteWindow,
    queue: SyncSender<WindowFrame>,
    keyframe: Arc<AtomicBool>,
) {
    let mut skipping = false;
    while let Ok(frame) = remote.next_frame().await {
        if keyframe.swap(false, Ordering::SeqCst) {
            remote.request_keyframe().await;
        }
        if skipping && !frame.keyframe {
            continue;
        }
        match queue.try_send(frame) {
            Ok(()) => skipping = false,
            Err(TrySendError::Full(_)) => {
                skipping = true;
                remote.request_keyframe().await;
            }
            Err(TrySendError::Disconnected(_)) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_refused_resume_falls_back_to_the_pin() {
        assert!(pairing_could_help(&TransportError::UnknownPeer));
        assert!(pairing_could_help(&TransportError::Rejected(
            "authentication failed".into()
        )));
        assert!(!pairing_could_help(&TransportError::Timeout));
        assert!(!pairing_could_help(&TransportError::Closed));
    }
}

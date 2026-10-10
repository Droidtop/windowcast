//! Bringing a session up: one offer and one answer exchanged over an
//! untrusted byte stream (a LAN TCP socket, or away from the LAN a stream
//! over a punched UDP socket, `punched`),
//! authenticated end to end so whoever carries the stream cannot substitute
//! a description.
//!
//! The exchange, client first:
//!
//! 1. `Hello` both ways: protocol version, persistent identity (Ed25519
//!    public key), a fresh 32-byte nonce, and the client's mode (`Pair`,
//!    `Resume` or `Account`).
//! 2. `Pair` only: one SPAKE2 message each way, seeded with the PIN the
//!    host shows. Both sides derive the same key only if both used the same
//!    PIN; nothing on the wire lets an observer test PIN guesses offline.
//!    `Account` only (docs/ACCOUNTS.md): the host's `AccountOffer`, signed
//!    with its identity, carrying a fresh HPKE key; the client, trusting
//!    that identity already, answers with its credential sealed to the key
//!    (`AccountProof`), and the host answers `AccountAccepted` or rejects.
//!    A hash of offer and proof goes into the transcript below.
//! 3. The client sends its offer, the host its answer
//!    (`windowcast_pairing::exchange`, the exchange droidtop-agent pairs with
//!    too). Each is signed with
//!    the sender's identity over a transcript binding the mode, both
//!    nonces, both identities and the complete SDP (which carries the DTLS
//!    fingerprint and the ICE credentials). While pairing, each is also
//!    HMAC-tagged with the PIN-derived key over the same transcript, which
//!    is what ties the two identities to the PIN; on `Resume` the signer
//!    must already be pinned.
//! 4. webrtc's DTLS handshake then refuses any certificate whose
//!    fingerprint differs from the authenticated SDP's.
//!
//! The host rejects a failed check with one generic "authentication failed"
//! and closes, so a wrong PIN costs the guesser a whole connection. ICE
//! candidates travel inside the descriptions (gathering completes before
//! each is sent); on a LAN that takes milliseconds and saves trickle
//! messages.

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use windowcast_accounts::seal::HostKey;
use windowcast_accounts::Account;
use windowcast_identity::{Identity, PeerId, TrustStore};
use windowcast_pairing::{exchange, SessionKey};
use windowcast_protocol::{
    AccountCredential, AccountOffer, ConnectMode, OidcProviderInfo, SdpKind, SignInMethod,
    SignalMessage, PROTOCOL_VERSION,
};

use crate::{Session, TransportError};

/// Signaling frames are a 4-byte big-endian length plus a bincode
/// `SignalMessage`. An SDP with every candidate is a few kilobytes.
const MAX_FRAME_BYTES: usize = 64 * 1024;

const TRANSCRIPT_LABEL: &[u8] = b"windowcast-signaling-v2\0";

/// What the host's account offer is signed over and the credential sealed
/// with, before the offer's own bytes.
const ACCOUNT_LABEL: &[u8] = b"windowcast-account-v1\0";

/// Message sent with every rejection, whatever the cause.
const AUTHENTICATION_FAILED: &str = "authentication failed";

/// Makes the client's account credential once it has the host's offer
/// (the Kerberos service to ask a ticket for, the providers). An error
/// stops the sign-in with [`TransportError::Credential`].
pub type MakeCredential<'a> =
    &'a (dyn Fn(&AccountOffer) -> Result<AccountCredential, String> + Send + Sync);

/// What the client proves itself with.
pub enum ClientCredential<'a> {
    /// First connection: the PIN the host is showing.
    Pin(&'a str),
    /// Later connections: the host must be pinned in this store.
    Pinned(&'a TrustStore),
    /// Signing in with an account. The credential goes only to a host
    /// pinned in `trusted` or the one `accept` names (a key the user
    /// confirmed); any other stops with [`TransportError::HostNotTrusted`].
    Account {
        trusted: &'a TrustStore,
        accept: Option<PeerId>,
        credential: MakeCredential<'a>,
    },
}

/// The host's account checking, for clients that sign in.
#[async_trait::async_trait]
pub trait AccountGate: Send + Sync {
    /// The sign-in methods, OIDC providers and Kerberos service offered.
    fn offer(&self) -> (Vec<SignInMethod>, Vec<OidcProviderInfo>, Option<String>);
    /// Checks the credential the device `client` presented: the account
    /// it signs in, or `None` to refuse (the gate logs why; the client
    /// hears only "authentication failed").
    async fn check(&self, client: PeerId, credential: AccountCredential) -> Option<Account>;
}

/// What the host accepts.
pub struct HostCredential<'a> {
    /// The PIN on screen while the host is open to a new pairing; `None`
    /// refuses every pairing attempt.
    pub pin: Option<&'a str>,
    /// Clients paired or registered earlier, accepted on `Resume`.
    pub trusted: &'a TrustStore,
    /// Account sign-in; `None` refuses every sign-in.
    pub accounts: Option<&'a dyn AccountGate>,
}

/// A connected, authenticated session.
pub struct Established {
    pub session: Session,
    /// The other side's persistent identity.
    pub peer: PeerId,
    /// True when this connection paired by PIN: the caller should pin
    /// `peer` in its trust store and save it.
    pub paired: bool,
    /// Host side: the account the client signed in with on this
    /// connection, to register its key to.
    pub account: Option<Account>,
    /// Client side: true when this connection signed in with an account
    /// (the host checked it): the caller should pin the host now.
    pub signed_in: bool,
}

struct Hello {
    peer: PeerId,
    nonce: [u8; 32],
    mode: ConnectMode,
}

impl Hello {
    /// The hello as the shared exchange signs it (`windowcast_pairing::exchange`).
    fn shared(&self) -> exchange::Hello {
        exchange::Hello {
            peer: self.peer,
            nonce: self.nonce,
            mode: match self.mode {
                ConnectMode::Pair => 0,
                ConnectMode::Resume => 1,
                ConnectMode::Account => 2,
            },
        }
    }
}

/// Client side. `session` is a fresh [`Session`] (with or without a relay).
pub async fn connect<S>(
    mut stream: S,
    session: Session,
    identity: &Identity,
    credential: ClientCredential<'_>,
) -> Result<Established, TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        connect_inner(&mut stream, &session, identity, credential),
    )
    .await
    .map_err(|_| TransportError::Timeout)?
    .map(|(peer, paired, signed_in)| Established {
        session,
        peer,
        paired,
        account: None,
        signed_in,
    })
}

/// Asks the host at the other end of `stream` what account sign-ins it
/// takes, and stops there: its identity and its offer, whose signature
/// is checked. Trust the providers it names only once that identity is
/// trusted.
pub async fn sign_in_offer<S>(
    mut stream: S,
    identity: &Identity,
) -> Result<(PeerId, AccountOffer), TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let client = Hello {
            peer: identity.peer_id(),
            nonce: exchange::fresh_nonce(),
            mode: ConnectMode::Account,
        };
        write_hello(&mut stream, &client).await?;
        let host = read_hello(&mut stream).await?;
        let (offer, _) = receive_offer(&mut stream, &client, &host).await?;
        Ok((host.peer, offer))
    })
    .await
    .map_err(|_| TransportError::Timeout)?
}

/// Host side, for one incoming signaling stream.
pub async fn accept<S>(
    mut stream: S,
    session: Session,
    identity: &Identity,
    credential: HostCredential<'_>,
) -> Result<Established, TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        accept_inner(&mut stream, &session, identity, credential),
    )
    .await
    .map_err(|_| TransportError::Timeout)?
    .map(|(peer, paired, account)| Established {
        session,
        peer,
        paired,
        account,
        signed_in: false,
    })
}

/// The whole exchange, including PIN key exchange, ICE gathering and the
/// control channel opening.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn connect_inner<S>(
    stream: &mut S,
    session: &Session,
    identity: &Identity,
    credential: ClientCredential<'_>,
) -> Result<(PeerId, bool, bool), TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mode = match credential {
        ClientCredential::Pin(_) => ConnectMode::Pair,
        ClientCredential::Pinned(_) => ConnectMode::Resume,
        ClientCredential::Account { .. } => ConnectMode::Account,
    };
    let client = Hello {
        peer: identity.peer_id(),
        nonce: exchange::fresh_nonce(),
        mode,
    };
    write_hello(stream, &client).await?;
    let host = read_hello(stream).await?;
    if let ClientCredential::Pinned(trusted) = credential {
        if !trusted.is_pinned(&host.peer) {
            return Err(TransportError::UnknownPeer);
        }
    }

    let mut binding = [0u8; 32];
    if let ClientCredential::Account {
        trusted,
        accept,
        credential: make,
    } = credential
    {
        // A credential goes only to a host this client already trusts.
        if !trusted.is_pinned(&host.peer) && accept != Some(host.peer) {
            return Err(TransportError::HostNotTrusted(host.peer));
        }
        let (offer, (context, signature)) = receive_offer(stream, &client, &host).await?;
        let credential = make(&offer).map_err(TransportError::Credential)?;
        let plaintext = windowcast_protocol::encode_credential(&credential)?;
        let (encapsulated, sealed) =
            windowcast_accounts::seal::seal(&offer.seal_key, &Sha256::digest(&context), &plaintext)
                .map_err(|e| TransportError::Credential(e.to_string()))?;
        binding = account_binding(&context, &signature, &encapsulated, &sealed);
        write_message(
            stream,
            &SignalMessage::AccountProof {
                encapsulated,
                sealed,
            },
        )
        .await?;
        match read_message(stream).await? {
            SignalMessage::AccountAccepted => {}
            other => return Err(unexpected(other, "the sign-in's verdict")),
        }
    }

    let key = match credential {
        ClientCredential::Pin(pin) => {
            let start = windowcast_pairing::start_client(pin);
            write_message(stream, &SignalMessage::Pake(start.outbound_message.clone())).await?;
            let peer_message = match read_message(stream).await? {
                SignalMessage::Pake(message) => message,
                other => return Err(unexpected(other, "a PAKE message")),
            };
            Some(windowcast_pairing::finish(start, &peer_message)?)
        }
        ClientCredential::Pinned(_) | ClientCredential::Account { .. } => None,
    };

    let offer = session.gathered_local_description(SdpKind::Offer).await?;
    send_description(
        stream,
        identity,
        key.as_ref(),
        SdpKind::Offer,
        offer,
        &client,
        &host,
        &binding,
    )
    .await?;

    let answer = receive_description(
        stream,
        key.as_ref(),
        SdpKind::Answer,
        &client,
        &host,
        &binding,
    )
    .await?;
    session
        .set_remote_description(SdpKind::Answer, answer)
        .await?;
    session.wait_control_open().await?;
    Ok((host.peer, key.is_some(), mode == ConnectMode::Account))
}

async fn accept_inner<S>(
    stream: &mut S,
    session: &Session,
    identity: &Identity,
    credential: HostCredential<'_>,
) -> Result<(PeerId, bool, Option<Account>), TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let client = read_hello(stream).await?;
    match client.mode {
        ConnectMode::Account if credential.accounts.is_none() => {
            reject(stream, "sign-in is not open on this host").await;
            return Err(TransportError::SignInNotOpen);
        }
        ConnectMode::Pair if credential.pin.is_none() => {
            reject(stream, "pairing is not open on this host").await;
            return Err(TransportError::PairingNotOpen);
        }
        ConnectMode::Resume if !credential.trusted.is_pinned(&client.peer) => {
            reject(stream, AUTHENTICATION_FAILED).await;
            return Err(TransportError::UnknownPeer);
        }
        _ => {}
    }
    let host = Hello {
        peer: identity.peer_id(),
        nonce: exchange::fresh_nonce(),
        mode: client.mode,
    };
    write_hello(stream, &host).await?;

    let mut binding = [0u8; 32];
    let mut account = None;
    if let (ConnectMode::Account, Some(gate)) = (client.mode, credential.accounts) {
        let seal_key = HostKey::generate();
        let (methods, providers, kerberos_service) = gate.offer();
        let offer = AccountOffer {
            methods,
            providers,
            kerberos_service,
            seal_key: seal_key.public(),
        };
        let context = account_context(&client, &host, &offer)?;
        let signature = identity.sign(&context).to_bytes().to_vec();
        write_message(
            stream,
            &SignalMessage::AccountOffer {
                offer,
                signature: signature.clone(),
            },
        )
        .await?;
        let (encapsulated, sealed) = match read_message(stream).await {
            Ok(SignalMessage::AccountProof {
                encapsulated,
                sealed,
            }) => (encapsulated, sealed),
            Ok(other) => return Err(unexpected(other, "an account proof")),
            // A client that only asked what this host offers.
            Err(TransportError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(TransportError::Closed);
            }
            Err(e) => return Err(e),
        };
        let presented = seal_key
            .open(&encapsulated, &sealed, &Sha256::digest(&context))
            .ok()
            .and_then(|plaintext| windowcast_protocol::decode_credential(&plaintext).ok());
        account = match presented {
            Some(presented) => gate.check(client.peer, presented).await,
            None => None,
        };
        if account.is_none() {
            reject(stream, AUTHENTICATION_FAILED).await;
            return Err(TransportError::SignInFailed);
        }
        write_message(stream, &SignalMessage::AccountAccepted).await?;
        binding = account_binding(&context, &signature, &encapsulated, &sealed);
    }

    let key = match (client.mode, credential.pin) {
        (ConnectMode::Pair, Some(pin)) => {
            let peer_message = match read_message(stream).await? {
                SignalMessage::Pake(message) => message,
                other => return Err(unexpected(other, "a PAKE message")),
            };
            let start = windowcast_pairing::start_host(pin);
            write_message(stream, &SignalMessage::Pake(start.outbound_message.clone())).await?;
            Some(windowcast_pairing::finish(start, &peer_message)?)
        }
        _ => None,
    };

    let offer = match receive_description(
        stream,
        key.as_ref(),
        SdpKind::Offer,
        &client,
        &host,
        &binding,
    )
    .await
    {
        Ok(offer) => offer,
        Err(TransportError::AuthenticationFailed) => {
            reject(stream, AUTHENTICATION_FAILED).await;
            return Err(TransportError::AuthenticationFailed);
        }
        Err(e) => return Err(e),
    };
    session
        .set_remote_description(SdpKind::Offer, offer)
        .await?;
    let answer = session.gathered_local_description(SdpKind::Answer).await?;
    send_description(
        stream,
        identity,
        key.as_ref(),
        SdpKind::Answer,
        answer,
        &client,
        &host,
        &binding,
    )
    .await?;

    session.wait_control_open().await?;
    Ok((client.peer, key.is_some(), account))
}

/// What the host's account offer is signed over: a label, both nonces and
/// identities, and the offer.
fn account_context(
    client: &Hello,
    host: &Hello,
    offer: &AccountOffer,
) -> Result<Vec<u8>, TransportError> {
    let mut context = ACCOUNT_LABEL.to_vec();
    context.extend_from_slice(&client.nonce);
    context.extend_from_slice(&host.nonce);
    context.extend_from_slice(&client.peer.0);
    context.extend_from_slice(&host.peer.0);
    context.extend_from_slice(&windowcast_protocol::encode_offer(offer)?);
    Ok(context)
}

/// The sign-in as the descriptions' transcript carries it: the offer, its
/// signature and the sealed proof, hashed.
fn account_binding(
    context: &[u8],
    signature: &[u8],
    encapsulated: &[u8],
    sealed: &[u8],
) -> [u8; 32] {
    Sha256::new()
        .chain_update(context)
        .chain_update(signature)
        .chain_update(encapsulated)
        .chain_update(sealed)
        .finalize()
        .into()
}

/// The offer and, for the proof, what it was signed over and the
/// signature.
type ReceivedOffer = (AccountOffer, (Vec<u8>, Vec<u8>));

/// Reads the host's account offer and checks its signature.
async fn receive_offer<S: AsyncRead + Unpin>(
    stream: &mut S,
    client: &Hello,
    host: &Hello,
) -> Result<ReceivedOffer, TransportError> {
    let (offer, signature) = match read_message(stream).await? {
        SignalMessage::AccountOffer { offer, signature } => (offer, signature),
        other => return Err(unexpected(other, "an account offer")),
    };
    let context = account_context(client, host, &offer)?;
    if !windowcast_identity::verify_bytes(&host.peer, &context, &signature) {
        return Err(TransportError::AuthenticationFailed);
    }
    Ok((offer, (context, signature)))
}

fn transcript(
    kind: SdpKind,
    client: &Hello,
    host: &Hello,
    binding: &[u8; 32],
    sdp: &str,
) -> Vec<u8> {
    let kind = match kind {
        SdpKind::Offer => 0,
        SdpKind::Answer => 1,
    };
    exchange::transcript(
        TRANSCRIPT_LABEL,
        kind,
        &client.shared(),
        &host.shared(),
        binding,
        sdp.as_bytes(),
    )
}

#[allow(clippy::too_many_arguments)]
async fn send_description<S>(
    stream: &mut S,
    identity: &Identity,
    key: Option<&SessionKey>,
    kind: SdpKind,
    sdp: String,
    client: &Hello,
    host: &Hello,
    binding: &[u8; 32],
) -> Result<(), TransportError>
where
    S: AsyncWrite + Unpin,
{
    let t = transcript(kind, client, host, binding, &sdp);
    let proof = exchange::prove(identity, key, &t);
    let message = SignalMessage::Description {
        kind,
        signature: proof.signature,
        pin_tag: proof.pin_tag,
        sdp,
    };
    write_message(stream, &message).await
}

/// Reads the peer's description and checks it. The signer is whoever
/// introduced themselves in their `Hello`: the client for an offer, the
/// host for an answer.
async fn receive_description<S>(
    stream: &mut S,
    key: Option<&SessionKey>,
    kind: SdpKind,
    client: &Hello,
    host: &Hello,
    binding: &[u8; 32],
) -> Result<String, TransportError>
where
    S: AsyncRead + Unpin,
{
    let (received_kind, sdp, signature, pin_tag) = match read_message(stream).await? {
        SignalMessage::Description {
            kind,
            sdp,
            signature,
            pin_tag,
        } => (kind, sdp, signature, pin_tag),
        other => return Err(unexpected(other, "a session description")),
    };
    if received_kind != kind {
        return Err(TransportError::Unexpected("the other kind of description"));
    }
    let signer = match kind {
        SdpKind::Offer => &client.peer,
        SdpKind::Answer => &host.peer,
    };
    let t = transcript(kind, client, host, binding, &sdp);
    exchange::check(signer, key, &t, &exchange::Proof { signature, pin_tag })
        .map_err(|_| TransportError::AuthenticationFailed)?;
    Ok(sdp)
}

async fn write_hello<S: AsyncWrite + Unpin>(
    stream: &mut S,
    hello: &Hello,
) -> Result<(), TransportError> {
    write_message(
        stream,
        &SignalMessage::Hello {
            version: PROTOCOL_VERSION,
            peer_id: hello.peer.0,
            nonce: hello.nonce,
            mode: hello.mode,
        },
    )
    .await
}

async fn read_hello<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Hello, TransportError> {
    match read_message(stream).await? {
        SignalMessage::Hello {
            peer_id,
            nonce,
            mode,
            ..
        } => Ok(Hello {
            peer: PeerId(peer_id),
            nonce,
            mode,
        }),
        other => Err(unexpected(other, "Hello")),
    }
}

fn unexpected(message: SignalMessage, expected: &'static str) -> TransportError {
    match message {
        SignalMessage::Reject(reason) => TransportError::Rejected(reason),
        _ => TransportError::Unexpected(expected),
    }
}

/// Best effort: the peer may already be gone.
async fn reject<S: AsyncWrite + Unpin>(stream: &mut S, reason: &str) {
    let _ = write_message(stream, &SignalMessage::Reject(reason.to_owned())).await;
}

/// Writes one length-prefixed signaling frame. Public so a relay or a test
/// harness can speak the framing; the content is authenticated end to end
/// regardless of who forwards it.
pub async fn write_message<S: AsyncWrite + Unpin>(
    stream: &mut S,
    message: &SignalMessage,
) -> Result<(), TransportError> {
    let bytes = windowcast_protocol::encode_signal(message)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge(bytes.len()));
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

/// Reads one length-prefixed signaling frame.
pub async fn read_message<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> Result<SignalMessage, TransportError> {
    let len = stream.read_u32().await? as usize;
    if len > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge(len));
    }
    let mut bytes = vec![0u8; len];
    stream.read_exact(&mut bytes).await?;
    Ok(windowcast_protocol::decode_signal(&bytes)?)
}

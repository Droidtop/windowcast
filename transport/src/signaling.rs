//! Bringing a session up: one offer and one answer exchanged over an
//! untrusted byte stream (a LAN TCP socket today; a directory relay later),
//! authenticated end to end so whoever carries the stream cannot substitute
//! a description.
//!
//! The exchange, client first:
//!
//! 1. `Hello` both ways: protocol version, persistent identity (Ed25519
//!    public key), a fresh 32-byte nonce, and the client's mode (`Pair` or
//!    `Resume`).
//! 2. `Pair` only: one SPAKE2 message each way, seeded with the PIN the
//!    host shows. Both sides derive the same key only if both used the same
//!    PIN; nothing on the wire lets an observer test PIN guesses offline.
//! 3. The client sends its offer, the host its answer. Each is signed with
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

use rand_core::{OsRng, RngCore};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use windowcast_identity::{Identity, PeerId, TrustStore};
use windowcast_pairing::SessionKey;
use windowcast_protocol::{ConnectMode, SdpKind, SignalMessage, PROTOCOL_VERSION};

use crate::{Session, TransportError};

/// Signaling frames are a 4-byte big-endian length plus a bincode
/// `SignalMessage`. An SDP with every candidate is a few kilobytes.
const MAX_FRAME_BYTES: usize = 64 * 1024;

const TRANSCRIPT_LABEL: &[u8] = b"windowcast-signaling-v1\0";

/// Message sent with every rejection, whatever the cause.
const AUTHENTICATION_FAILED: &str = "authentication failed";

/// What the client proves itself with.
pub enum ClientCredential<'a> {
    /// First connection: the PIN the host is showing.
    Pin(&'a str),
    /// Later connections: the host must be pinned in this store.
    Pinned(&'a TrustStore),
}

/// What the host accepts.
pub struct HostCredential<'a> {
    /// The PIN on screen while the host is open to a new pairing; `None`
    /// refuses every pairing attempt.
    pub pin: Option<&'a str>,
    /// Clients paired earlier, accepted on `Resume`.
    pub trusted: &'a TrustStore,
}

/// A connected, authenticated session.
pub struct Established {
    pub session: Session,
    /// The other side's persistent identity.
    pub peer: PeerId,
    /// True when this connection paired by PIN: the caller should pin
    /// `peer` in its trust store and save it.
    pub paired: bool,
}

struct Hello {
    peer: PeerId,
    nonce: [u8; 32],
    mode: ConnectMode,
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
    .map(|(peer, paired)| Established {
        session,
        peer,
        paired,
    })
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
    .map(|(peer, paired)| Established {
        session,
        peer,
        paired,
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
) -> Result<(PeerId, bool), TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mode = match credential {
        ClientCredential::Pin(_) => ConnectMode::Pair,
        ClientCredential::Pinned(_) => ConnectMode::Resume,
    };
    let client = Hello {
        peer: identity.peer_id(),
        nonce: fresh_nonce(),
        mode,
    };
    write_hello(stream, &client).await?;
    let host = read_hello(stream).await?;
    if let ClientCredential::Pinned(trusted) = credential {
        if !trusted.is_pinned(&host.peer) {
            return Err(TransportError::UnknownPeer);
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
        ClientCredential::Pinned(_) => None,
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
    )
    .await?;

    let answer = receive_description(stream, key.as_ref(), SdpKind::Answer, &client, &host).await?;
    session
        .set_remote_description(SdpKind::Answer, answer)
        .await?;
    session.wait_control_open().await?;
    Ok((host.peer, key.is_some()))
}

async fn accept_inner<S>(
    stream: &mut S,
    session: &Session,
    identity: &Identity,
    credential: HostCredential<'_>,
) -> Result<(PeerId, bool), TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let client = read_hello(stream).await?;
    match client.mode {
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
        nonce: fresh_nonce(),
        mode: client.mode,
    };
    write_hello(stream, &host).await?;

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

    let offer =
        match receive_description(stream, key.as_ref(), SdpKind::Offer, &client, &host).await {
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
    )
    .await?;

    session.wait_control_open().await?;
    Ok((client.peer, key.is_some()))
}

fn transcript(kind: SdpKind, client: &Hello, host: &Hello, sdp: &str) -> Vec<u8> {
    let mut t = Vec::with_capacity(TRANSCRIPT_LABEL.len() + 2 + 4 * 32 + sdp.len());
    t.extend_from_slice(TRANSCRIPT_LABEL);
    t.push(match kind {
        SdpKind::Offer => 0,
        SdpKind::Answer => 1,
    });
    t.push(match client.mode {
        ConnectMode::Pair => 0,
        ConnectMode::Resume => 1,
    });
    t.extend_from_slice(&client.nonce);
    t.extend_from_slice(&host.nonce);
    t.extend_from_slice(&client.peer.0);
    t.extend_from_slice(&host.peer.0);
    t.extend_from_slice(sdp.as_bytes());
    t
}

async fn send_description<S>(
    stream: &mut S,
    identity: &Identity,
    key: Option<&SessionKey>,
    kind: SdpKind,
    sdp: String,
    client: &Hello,
    host: &Hello,
) -> Result<(), TransportError>
where
    S: AsyncWrite + Unpin,
{
    let t = transcript(kind, client, host, &sdp);
    let message = SignalMessage::Description {
        kind,
        signature: identity.sign(&t).to_bytes().to_vec(),
        pin_tag: key.map(|key| windowcast_pairing::authenticate_fingerprint(key, &t)),
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
    let t = transcript(kind, client, host, &sdp);
    if let Some(key) = key {
        let tag = pin_tag.ok_or(TransportError::AuthenticationFailed)?;
        windowcast_pairing::verify_fingerprint(key, &t, &tag)
            .map_err(|_| TransportError::AuthenticationFailed)?;
    }
    if !windowcast_identity::verify_bytes(signer, &t, &signature) {
        return Err(TransportError::AuthenticationFailed);
    }
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

fn fresh_nonce() -> [u8; 32] {
    let mut nonce = [0u8; 32];
    OsRng.fill_bytes(&mut nonce);
    nonce
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

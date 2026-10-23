//! The handshake: Noise `IK` between two pinned windowcast identities, as
//! plain functions on byte strings (no sockets, no timers), so the layer that
//! owns the socket decides how to retransmit.
//!
//! ```text
//!   initiator                                     responder
//!   msg1: e, es, s, ss, {initiator PeerId}   -->
//!                                            <--  msg2: e, ee, se
//! ```
//!
//! The initiator already knows the responder's identity (it is pinned), which
//! is what `IK` needs: the responder's static key is the key its `PeerId`
//! converts to. A windowcast identity is an Ed25519 key; Noise wants X25519,
//! so each side uses the birational map between the two curves (the same one
//! Signal and libsodium use), and a test here proves both sides land on the
//! same keys.
//!
//! The responder cannot turn the initiator's X25519 static key back into one
//! `PeerId` (the map loses a sign bit), so the initiator sends its `PeerId`
//! as the first message's payload and the responder checks it converts to the
//! key that actually authenticated. Only then does it ask whether that peer is
//! pinned, and an unpinned or mismatched peer gets no reply at all.
//!
//! Each handshake mixes in fresh ephemeral keys, so two handshakes between the
//! same peers share no keys, and recorded traffic stays unreadable if a static
//! key leaks later. Both sides' keys come out of the Noise split as one key
//! per direction.

use sha2::{Digest, Sha512};
use snow::{Builder, HandshakeState};
use windowcast_identity::{Identity, IdentityError, PeerId};

use crate::packet::{Opener, Sealer};

const PATTERN: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
/// Binds a handshake to this protocol and version: a Noise session built for
/// anything else cannot be mistaken for one of ours.
const PROLOGUE: &[u8] = b"windowcast-bud/1";
const MAX_MESSAGE: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("noise: {0}")]
    Noise(#[from] snow::Error),
    #[error("identity: {0}")]
    Identity(#[from] IdentityError),
    #[error("the first message does not carry a peer id")]
    BadPayload,
    #[error("the peer id in the first message is not the key that authenticated it")]
    IdentityMismatch,
    #[error("that peer is not pinned")]
    NotPinned,
    #[error("the handshake did not finish")]
    Unfinished,
}

/// What a finished handshake gives the layer above.
pub struct Session {
    /// The peer on the other end, as pinned.
    pub peer: PeerId,
    pub sealer: Sealer,
    pub opener: Opener,
    /// Both sides compute the same value; comparing it out of band confirms
    /// there is no one in the middle.
    pub handshake_hash: [u8; 32],
}

/// The X25519 secret that goes with an identity's Ed25519 key: the clamped
/// first half of the SHA-512 of its seed, which is exactly the scalar Ed25519
/// signs with.
fn static_secret(identity: &Identity) -> [u8; 32] {
    let pair = identity.to_keypair_bytes();
    let digest = Sha512::digest(&pair[..32]);
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&digest[..32]);
    secret[0] &= 248;
    secret[31] &= 127;
    secret[31] |= 64;
    secret
}

/// The X25519 public key a peer id converts to.
fn static_public(peer: &PeerId) -> Result<[u8; 32], HandshakeError> {
    Ok(peer.to_verifying_key()?.to_montgomery().to_bytes())
}

fn finish(
    mut state: HandshakeState,
    peer: PeerId,
    initiator: bool,
) -> Result<Session, HandshakeError> {
    if !state.is_handshake_finished() {
        return Err(HandshakeError::Unfinished);
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(state.get_handshake_hash());
    // The first key is the initiator's sending key, the second the
    // responder's.
    let (mut first, mut second) = state.dangerously_get_raw_split();
    let (send, receive) = if initiator {
        (&first, &second)
    } else {
        (&second, &first)
    };
    let session = Session {
        peer,
        sealer: Sealer::new(send),
        opener: Opener::new(receive),
        handshake_hash: hash,
    };
    first.fill(0);
    second.fill(0);
    Ok(session)
}

/// The initiator between its first message and the reply.
pub struct Initiator {
    state: HandshakeState,
    remote: PeerId,
}

impl Initiator {
    /// Starts a handshake with `remote`, a peer this side has pinned. Returns
    /// the state to finish it with and the first message to send.
    pub fn start(
        local: &Identity,
        remote: &PeerId,
    ) -> Result<(Initiator, Vec<u8>), HandshakeError> {
        Self::start_claiming(local, remote, &local.peer_id())
    }

    /// `start`, with the peer id the first message claims given separately,
    /// so tests can send a claim that does not match the key.
    pub(crate) fn start_claiming(
        local: &Identity,
        remote: &PeerId,
        claimed: &PeerId,
    ) -> Result<(Initiator, Vec<u8>), HandshakeError> {
        let mut secret = static_secret(local);
        let remote_key = static_public(remote)?;
        let built = Builder::new(PATTERN.parse()?)
            .local_private_key(&secret)
            .remote_public_key(&remote_key)
            .prologue(PROLOGUE)
            .build_initiator();
        secret.fill(0);
        let mut state = built?;
        let mut message = [0u8; MAX_MESSAGE];
        let len = state.write_message(&claimed.0, &mut message)?;
        Ok((
            Initiator {
                state,
                remote: *remote,
            },
            message[..len].to_vec(),
        ))
    }

    /// Reads the responder's reply and, if it is genuine, gives the session.
    pub fn finish(mut self, reply: &[u8]) -> Result<Session, HandshakeError> {
        let mut payload = [0u8; MAX_MESSAGE];
        self.state.read_message(reply, &mut payload)?;
        finish(self.state, self.remote, true)
    }
}

/// What the responder gets from a first message it accepts.
pub struct Accepted {
    /// Send this back to the initiator.
    pub reply: Vec<u8>,
    pub session: Session,
}

/// Answers a first message. `is_pinned` says whether a peer id is one this
/// side trusts; a peer that is not, or whose claimed id is not the key that
/// authenticated, gets an error here and nothing is sent back.
pub fn accept(
    local: &Identity,
    first_message: &[u8],
    is_pinned: impl Fn(&PeerId) -> bool,
) -> Result<Accepted, HandshakeError> {
    let mut secret = static_secret(local);
    let built = Builder::new(PATTERN.parse()?)
        .local_private_key(&secret)
        .prologue(PROLOGUE)
        .build_responder();
    secret.fill(0);
    let mut state = built?;
    let mut payload = [0u8; MAX_MESSAGE];
    let len = state.read_message(first_message, &mut payload)?;
    if len != 32 {
        return Err(HandshakeError::BadPayload);
    }
    let mut id = [0u8; 32];
    id.copy_from_slice(&payload[..32]);
    let peer = PeerId(id);
    let authenticated = state
        .get_remote_static()
        .ok_or(HandshakeError::Unfinished)?;
    // An id that is not a valid key, or that converts to a different key than
    // the one the handshake just proved the initiator holds, is a lie.
    if static_public(&peer).map_err(|_| HandshakeError::IdentityMismatch)?[..] != *authenticated {
        return Err(HandshakeError::IdentityMismatch);
    }
    if !is_pinned(&peer) {
        return Err(HandshakeError::NotPinned);
    }
    let mut reply = [0u8; MAX_MESSAGE];
    let reply_len = state.write_message(&[], &mut reply)?;
    let session = finish(state, peer, false)?;
    Ok(Accepted {
        reply: reply[..reply_len].to_vec(),
        session,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::Header;
    use crate::packet::OpenError;

    fn header() -> Header {
        Header {
            flags: 0,
            stream: 1,
            frame: 0,
            fragment: 0,
            fragments: 1,
            packet: 0,
            sent_us: 0,
        }
    }

    /// A full handshake between two fresh identities that pin each other.
    fn connected() -> (Session, Session, Identity, Identity) {
        let a = Identity::generate();
        let b = Identity::generate();
        let (initiator, first) = Initiator::start(&a, &b.peer_id()).unwrap();
        let a_id = a.peer_id();
        let accepted = accept(&b, &first, |p| *p == a_id).unwrap();
        let a_session = initiator.finish(&accepted.reply).unwrap();
        (a_session, accepted.session, a, b)
    }

    #[test]
    fn both_sides_find_each_other_and_share_a_hash() {
        let (a_session, b_session, a, b) = connected();
        assert_eq!(a_session.peer, b.peer_id());
        assert_eq!(b_session.peer, a.peer_id());
        assert_eq!(a_session.handshake_hash, b_session.handshake_hash);
    }

    #[test]
    fn traffic_flows_both_ways_under_separate_keys() {
        let (mut a, mut b, _, _) = connected();
        let d = a.sealer.seal(header(), b"from a").unwrap();
        assert_eq!(b.opener.open(&d).unwrap().1, b"from a");
        let d = b.sealer.seal(header(), b"from b").unwrap();
        assert_eq!(a.opener.open(&d).unwrap().1, b"from b");
        // A packet cannot be reflected back at its sender.
        let d = a.sealer.seal(header(), b"mine").unwrap();
        assert_eq!(a.opener.open(&d), Err(OpenError::Authentication));
    }

    #[test]
    fn two_handshakes_between_the_same_peers_share_no_keys() {
        let a = Identity::generate();
        let b = Identity::generate();
        let a_id = a.peer_id();
        let run = || {
            let (i, first) = Initiator::start(&a, &b.peer_id()).unwrap();
            let acc = accept(&b, &first, |p| *p == a_id).unwrap();
            (i.finish(&acc.reply).unwrap(), acc.session)
        };
        let (mut one, _) = run();
        let (_, mut two) = run();
        let d = one.sealer.seal(header(), b"x").unwrap();
        assert!(two.opener.open(&d).is_err());
    }

    #[test]
    fn a_peer_that_is_not_pinned_is_refused() {
        let a = Identity::generate();
        let b = Identity::generate();
        let (_, first) = Initiator::start(&a, &b.peer_id()).unwrap();
        assert!(matches!(
            accept(&b, &first, |_| false),
            Err(HandshakeError::NotPinned)
        ));
    }

    #[test]
    fn a_first_message_for_someone_else_does_not_open() {
        let a = Identity::generate();
        let b = Identity::generate();
        let other = Identity::generate();
        let (_, first) = Initiator::start(&a, &b.peer_id()).unwrap();
        // `other` receives a message encrypted to b.
        assert!(matches!(
            accept(&other, &first, |_| true),
            Err(HandshakeError::Noise(_))
        ));
    }

    #[test]
    fn a_claim_that_is_not_the_authenticated_key_is_refused() {
        let a = Identity::generate();
        let b = Identity::generate();
        let victim = Identity::generate();
        // `a` holds its own static key but claims to be `victim`, whom the
        // responder does pin.
        let (_, first) = Initiator::start_claiming(&a, &b.peer_id(), &victim.peer_id()).unwrap();
        let victim_id = victim.peer_id();
        assert!(matches!(
            accept(&b, &first, |p| *p == victim_id),
            Err(HandshakeError::IdentityMismatch)
        ));
    }

    #[test]
    fn a_tampered_message_is_refused_on_either_side() {
        let a = Identity::generate();
        let b = Identity::generate();
        let (initiator, first) = Initiator::start(&a, &b.peer_id()).unwrap();
        let mut bad = first.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(accept(&b, &bad, |_| true).is_err());

        let a_id = a.peer_id();
        let accepted = accept(&b, &first, |p| *p == a_id).unwrap();
        let mut bad_reply = accepted.reply.clone();
        let last = bad_reply.len() - 1;
        bad_reply[last] ^= 1;
        assert!(initiator.finish(&bad_reply).is_err());
    }

    #[test]
    fn a_reply_from_an_impostor_responder_is_refused() {
        let a = Identity::generate();
        let b = Identity::generate();
        let impostor = Identity::generate();
        let (initiator, first) = Initiator::start(&a, &b.peer_id()).unwrap();
        // The impostor cannot read the first message, so it answers a
        // handshake of its own.
        let (_, its_own_first) = Initiator::start(&a, &impostor.peer_id()).unwrap();
        let a_id = a.peer_id();
        let reply = accept(&impostor, &its_own_first, |p| *p == a_id)
            .unwrap()
            .reply;
        assert!(initiator.finish(&reply).is_err());
        drop(first);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let b = Identity::generate();
        for len in [0usize, 1, 31, 32, 127, 128, 129, 300] {
            let junk = vec![0xa5u8; len];
            assert!(accept(&b, &junk, |_| true).is_err());
        }
    }
}

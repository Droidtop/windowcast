//! The authenticated exchange that brings two devices together, without any
//! I/O: each side's hello (identity, a fresh nonce, the mode), the
//! transcript both sides sign, and the proof each sends over it. It is the
//! one pairing mechanism of windowcast and droidtop-agent.
//! - windowcast's signaling (`windowcast-transport`) carries a session
//!   description (the SDP) in it.
//! - droidtop-agent carries the device's name.
//!
//! Each caller frames the messages its own way and names its exchange with
//! its own label, so a transcript from one can never be replayed into the
//! other.
//!
//! The exchange, client first:
//! 1. both send a [`Hello`];
//! 2. pairing only: one SPAKE2 message each way, seeded with the PIN
//!    ([`crate::start_client`], [`crate::start_host`], [`crate::finish`]);
//! 3. each sends its description with a [`Proof`] over the
//!    [`transcript`]: the sender's identity signs it, and while pairing the
//!    PIN-derived key also tags it. The tag ties both identities to the PIN,
//!    and the signature proves the sender holds its key. Resuming, the
//!    signer must already be trusted.

use rand_core::{OsRng, RngCore};
use windowcast_identity::{Identity, PeerId};

use crate::{authenticate_fingerprint, verify_fingerprint, PairingError, SessionKey};

/// One side's hello.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    /// The sender's persistent identity.
    pub peer: PeerId,
    /// Fresh for every exchange, so no transcript repeats.
    pub nonce: [u8; 32],
    /// What the client asks for, as a number the caller defines (windowcast:
    /// 0 pair, 1 resume, 2 account); the host echoes it.
    pub mode: u8,
}

impl Hello {
    pub fn new(peer: PeerId, mode: u8) -> Hello {
        Hello {
            peer,
            nonce: fresh_nonce(),
            mode,
        }
    }
}

pub fn fresh_nonce() -> [u8; 32] {
    let mut nonce = [0u8; 32];
    OsRng.fill_bytes(&mut nonce);
    nonce
}

/// What a description's proof covers: the caller's label, which of the two
/// descriptions this is (0 the client's, 1 the host's), the mode, both
/// nonces and both identities, a binding to anything authenticated before
/// it (windowcast's account sign-in; zeros without one), and the
/// description itself.
pub fn transcript(
    label: &[u8],
    kind: u8,
    client: &Hello,
    host: &Hello,
    binding: &[u8; 32],
    description: &[u8],
) -> Vec<u8> {
    let mut t = Vec::with_capacity(label.len() + 2 + 5 * 32 + description.len());
    t.extend_from_slice(label);
    t.push(kind);
    t.push(client.mode);
    t.extend_from_slice(&client.nonce);
    t.extend_from_slice(&host.nonce);
    t.extend_from_slice(&client.peer.0);
    t.extend_from_slice(&host.peer.0);
    t.extend_from_slice(binding);
    t.extend_from_slice(description);
    t
}

/// The sender's proof over a transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proof {
    /// The sender's identity's Ed25519 signature (64 bytes).
    pub signature: Vec<u8>,
    /// While pairing: the PIN-derived key's HMAC tag.
    pub pin_tag: Option<[u8; 32]>,
}

/// Signs [`transcript`] and, while pairing, tags it with the PIN-derived key.
pub fn prove(identity: &Identity, key: Option<&SessionKey>, transcript: &[u8]) -> Proof {
    Proof {
        signature: identity.sign(transcript).to_bytes().to_vec(),
        pin_tag: key.map(|key| authenticate_fingerprint(key, transcript)),
    }
}

/// Checks a proof from [`signer`]: its signature always, and while pairing
/// ([`key`] given) its tag, which must then be there.
pub fn check(
    signer: &PeerId,
    key: Option<&SessionKey>,
    transcript: &[u8],
    proof: &Proof,
) -> Result<(), PairingError> {
    if let Some(key) = key {
        let tag = proof
            .pin_tag
            .as_ref()
            .ok_or(PairingError::FingerprintAuthFailed)?;
        verify_fingerprint(key, transcript, tag)?;
    }
    if !windowcast_identity::verify_bytes(signer, transcript, &proof.signature) {
        return Err(PairingError::SignatureFailed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{finish, start_client, start_host};

    fn keys(client_pin: &str, host_pin: &str) -> (SessionKey, SessionKey) {
        let (c, h) = (start_client(client_pin), start_host(host_pin));
        let (cm, hm) = (c.outbound_message.clone(), h.outbound_message.clone());
        (finish(c, &hm).unwrap(), finish(h, &cm).unwrap())
    }

    #[test]
    fn a_paired_exchange_checks_on_both_sides() {
        let (client_id, host_id) = (Identity::generate(), Identity::generate());
        let client = Hello::new(client_id.peer_id(), 0);
        let host = Hello::new(host_id.peer_id(), 0);
        let (ck, hk) = keys("123456", "123456");
        let t = transcript(
            b"test\0",
            0,
            &client,
            &host,
            &[0; 32],
            b"client's description",
        );
        let proof = prove(&client_id, Some(&ck), &t);
        assert!(check(&client_id.peer_id(), Some(&hk), &t, &proof).is_ok());
        // Another identity cannot claim it, and the description cannot change.
        assert!(check(&host_id.peer_id(), Some(&hk), &t, &proof).is_err());
        let other = transcript(b"test\0", 0, &client, &host, &[0; 32], b"swapped");
        assert!(check(&client_id.peer_id(), Some(&hk), &other, &proof).is_err());
    }

    #[test]
    fn a_wrong_pin_or_a_missing_tag_fails() {
        let id = Identity::generate();
        let (a, b) = (
            Hello::new(id.peer_id(), 0),
            Hello::new(Identity::generate().peer_id(), 0),
        );
        let t = transcript(b"test\0", 1, &a, &b, &[0; 32], b"d");
        let (ck, hk) = keys("111111", "222222");
        assert!(check(&id.peer_id(), Some(&hk), &t, &prove(&id, Some(&ck), &t)).is_err());
        assert!(check(&id.peer_id(), Some(&hk), &t, &prove(&id, None, &t)).is_err());
        // Resuming has no tag; the signature alone is checked.
        assert!(check(&id.peer_id(), None, &t, &prove(&id, None, &t)).is_ok());
    }

    #[test]
    fn labels_keep_exchanges_apart() {
        let id = Identity::generate();
        let (a, b) = (Hello::new(id.peer_id(), 0), Hello::new(id.peer_id(), 0));
        assert_ne!(
            transcript(b"one\0", 0, &a, &b, &[0; 32], b"d"),
            transcript(b"two\0", 0, &a, &b, &[0; 32], b"d")
        );
    }
}

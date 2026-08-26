//! First-pairing PAKE (password-authenticated key exchange), seeded by a
//! short PIN the host displays and the user types into the client. Real
//! goal: authenticate the WebRTC DTLS fingerprint exchange itself, so an
//! attacker controlling the rendezvous/signaling path (which is NOT
//! assumed trustworthy — it may be a relay, a QR code, plain text over a
//! network) cannot substitute their own fingerprint and sit in the middle.
//! The PIN never crosses the wire; SPAKE2 proves both sides know it while
//! deriving a shared secret an eavesdropper who doesn't know the PIN can't
//! compute even having seen every message.
//!
//! This crate only runs once, at first pairing. Every later connection
//! authenticates via the persistent Ed25519 identities (`windowcast-identity`)
//! established at the end of a successful run here — see `SessionKey` and
//! the `authenticate_fingerprint`/`verify_fingerprint` helpers below, and
//! the identity-binding step callers are expected to perform afterward.

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};
use zeroize::Zeroize;

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("SPAKE2 key exchange failed (wrong PIN, or tampered message)")]
    KeyExchangeFailed,
    #[error("fingerprint authentication tag did not match — possible MITM")]
    FingerprintAuthFailed,
}

/// A fixed-length key derived from the PAKE run. Zeroized on drop since
/// it's short-lived secret material, not a long-term key.
pub struct SessionKey([u8; 32]);

impl Drop for SessionKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Which side of the PIN exchange this peer is. The host displays the PIN
/// (Responder); the client's user types it in (Initiator). SPAKE2 requires
/// picking consistent, distinct identity labels for the two sides — using
/// fixed roles here (rather than the "symmetric" SPAKE2 variant) makes the
/// protocol easier to reason about and matches how the PIN is actually
/// shown to the user in one specific direction.
const ROLE_LABEL_CLIENT: &[u8] = b"windowcast-client";
const ROLE_LABEL_HOST: &[u8] = b"windowcast-host";

/// Generates a display PIN. 6 digits (10^6 guesses) rather than 4 —
/// SPAKE2 only rate-limits guessing to one attempt per network round-trip
/// (an attacker can't brute-force offline from transcripts), but a longer
/// PIN costs the legitimate user almost nothing to type once and raises
/// the bar further against an attacker racing to guess it during the
/// pairing window.
pub fn generate_pin() -> String {
    let mut rng = OsRng;
    format!("{:06}", rng.next_u32() % 1_000_000)
}

pub struct PairingStart {
    spake: Spake2<Ed25519Group>,
    pub outbound_message: Vec<u8>,
}

pub fn start_client(pin: &str) -> PairingStart {
    let (spake, outbound) = Spake2::<Ed25519Group>::start_a(
        &Password::new(pin.as_bytes()),
        &SpakeIdentity::new(ROLE_LABEL_CLIENT),
        &SpakeIdentity::new(ROLE_LABEL_HOST),
    );
    PairingStart {
        spake,
        outbound_message: outbound,
    }
}

pub fn start_host(pin: &str) -> PairingStart {
    let (spake, outbound) = Spake2::<Ed25519Group>::start_b(
        &Password::new(pin.as_bytes()),
        &SpakeIdentity::new(ROLE_LABEL_CLIENT),
        &SpakeIdentity::new(ROLE_LABEL_HOST),
    );
    PairingStart {
        spake,
        outbound_message: outbound,
    }
}

/// Completes the exchange given the peer's message, deriving a fixed-length
/// session key via HKDF-SHA256 over the raw (variable-length,
/// group-element-derived) SPAKE2 output. This can only fail on a
/// structurally malformed peer message — by design, SPAKE2 does NOT reveal
/// whether the two sides used the same PIN at this step (that's the
/// point: it doesn't leak a password-guessing oracle). A wrong PIN instead
/// makes both sides derive *different* keys silently, which the
/// fingerprint-authentication step below is what actually detects and
/// fails on — never skip that step.
pub fn finish(start: PairingStart, peer_message: &[u8]) -> Result<SessionKey, PairingError> {
    let raw = start
        .spake
        .finish(peer_message)
        .map_err(|_| PairingError::KeyExchangeFailed)?;
    let hk = Hkdf::<Sha256>::new(None, &raw);
    let mut key = [0u8; 32];
    hk.expand(b"windowcast-pairing-session-key-v1", &mut key)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    Ok(SessionKey(key))
}

type HmacSha256 = Hmac<Sha256>;

/// Authenticates a DTLS fingerprint (or any other short byte string) using
/// the PAKE-derived session key. Both sides compute and exchange this tag
/// over their own outbound fingerprint before proceeding to the actual
/// WebRTC handshake, so a substituted fingerprint fails verification here
/// instead of silently succeeding a DTLS handshake with the wrong party.
pub fn authenticate_fingerprint(key: &SessionKey, fingerprint: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(&key.0).expect("HMAC accepts any key length");
    mac.update(fingerprint);
    mac.finalize().into_bytes().into()
}

pub fn verify_fingerprint(
    key: &SessionKey,
    fingerprint: &[u8],
    tag: &[u8; 32],
) -> Result<(), PairingError> {
    let mut mac = HmacSha256::new_from_slice(&key.0).expect("HMAC accepts any key length");
    mac.update(fingerprint);
    mac.verify_slice(tag)
        .map_err(|_| PairingError::FingerprintAuthFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_pin_derives_a_session_key_both_sides_agree_on() {
        let pin = "123456";
        let client = start_client(pin);
        let host = start_host(pin);
        let client_msg = client.outbound_message.clone();
        let host_msg = host.outbound_message.clone();

        let client_key = finish(client, &host_msg).unwrap();
        let host_key = finish(host, &client_msg).unwrap();

        let fingerprint = b"fake-dtls-fingerprint";
        let tag = authenticate_fingerprint(&client_key, fingerprint);
        assert!(verify_fingerprint(&host_key, fingerprint, &tag).is_ok());
    }

    #[test]
    fn mismatched_pin_fails_fingerprint_authentication() {
        let client = start_client("111111");
        let host = start_host("222222");
        let client_msg = client.outbound_message.clone();
        let host_msg = host.outbound_message.clone();

        let client_key = finish(client, &host_msg).unwrap();
        let host_key = finish(host, &client_msg).unwrap();

        let fingerprint = b"fake-dtls-fingerprint";
        let tag = authenticate_fingerprint(&client_key, fingerprint);
        assert!(verify_fingerprint(&host_key, fingerprint, &tag).is_err());
    }

    #[test]
    fn pin_is_six_digits() {
        let pin = generate_pin();
        assert_eq!(pin.len(), 6);
        assert!(pin.chars().all(|c| c.is_ascii_digit()));
    }
}

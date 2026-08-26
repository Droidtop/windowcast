//! Session certificates: what a directory issues to a logged-in account,
//! and what a host verifies to authenticate an *account* credential
//! (rather than a device-pinned one). A host that trusts one directory's
//! CA key can accept any account it vouches for, without individually
//! pinning every user the way device pairing pins one specific peer.
//!
//! This is deliberately session-scoped, not device-scoped: the same
//! account can log in from different client devices and get a fresh
//! certificate each time, bound to that session's own ephemeral peer id
//! (see [`SessionClaims::session_peer_id`]) — the certificate can't be
//! replayed by a different client than the one it was issued to, since a
//! host also requires the presenter to sign a fresh nonce/DTLS fingerprint
//! with the private key matching that peer id.

use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use windowcast_identity::{Identity, PeerId};

use crate::accounts::Account;

pub const DEFAULT_SESSION_TTL_SECONDS: u64 = 12 * 60 * 60;

#[derive(Debug, thiserror::Error)]
pub enum CertificateError {
    #[error("certificate expired at {expires_at}, now is {now}")]
    Expired { expires_at: u64, now: u64 },
    #[error("certificate signature does not verify against the trusted directory key")]
    BadSignature,
    #[error("failed to (de)serialize claims: {0}")]
    Encode(#[from] bincode::Error),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionClaims {
    pub account: String,
    pub role: String,
    /// The client's own Ed25519 public key for this login session — ties
    /// the certificate to whoever actually holds the matching private key,
    /// not just to whoever is holding a copy of the certificate bytes.
    pub session_peer_id: [u8; 32],
    pub issued_at: u64,
    pub expires_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCertificate {
    pub claims: SessionClaims,
    /// Always exactly 64 bytes (an Ed25519 signature) — stored as `Vec<u8>`
    /// because serde's built-in array impls don't cover arrays this large;
    /// see [`verify`] and [`DirectoryCa::issue_session_certificate`] for
    /// the only two places that construct/consume it.
    signature: Vec<u8>,
}

/// A directory's own signing identity — the CA every issued certificate
/// chains back to. A host configured to trust this key (by its `PeerId`)
/// will accept any unexpired, validly-signed certificate this CA issues,
/// for any account.
pub struct DirectoryCa {
    identity: Identity,
}

impl DirectoryCa {
    pub fn load_or_generate(
        path: &std::path::Path,
    ) -> Result<Self, windowcast_identity::IdentityError> {
        Ok(DirectoryCa {
            identity: Identity::load_or_generate(path)?,
        })
    }

    pub fn public_key(&self) -> PeerId {
        self.identity.peer_id()
    }

    pub fn issue_session_certificate(
        &self,
        account: &Account,
        session_peer_id: [u8; 32],
        ttl_seconds: u64,
    ) -> Result<SessionCertificate, CertificateError> {
        let now = unix_now();
        let claims = SessionClaims {
            account: account.username.clone(),
            role: account.role.clone(),
            session_peer_id,
            issued_at: now,
            expires_at: now + ttl_seconds,
        };
        let signature = self.identity.sign(&bincode::serialize(&claims)?);
        Ok(SessionCertificate {
            claims,
            signature: signature.to_bytes().to_vec(),
        })
    }
}

/// Verifies a certificate against a trusted directory CA's public key.
/// Callers (host agents) still need to separately verify that whoever
/// presented this certificate actually controls `claims.session_peer_id`'s
/// private key (e.g. by requiring a signature over the connection's DTLS
/// fingerprint from that same key) — this function only establishes that
/// the *claims themselves* genuinely came from a trusted directory.
pub fn verify<'a>(
    directory_ca: &PeerId,
    cert: &'a SessionCertificate,
) -> Result<&'a SessionClaims, CertificateError> {
    let now = unix_now();
    if cert.claims.expires_at < now {
        return Err(CertificateError::Expired {
            expires_at: cert.claims.expires_at,
            now,
        });
    }
    let bytes = bincode::serialize(&cert.claims)?;
    let signature_bytes: [u8; 64] = cert
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| CertificateError::BadSignature)?;
    let signature = Signature::from_bytes(&signature_bytes);
    if !windowcast_identity::verify(directory_ca, &bytes, &signature) {
        return Err(CertificateError::BadSignature);
    }
    Ok(&cert.claims)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::AccountStore;

    fn test_ca() -> DirectoryCa {
        DirectoryCa {
            identity: Identity::generate(),
        }
    }

    #[test]
    fn issues_and_verifies_a_valid_certificate() {
        let mut store = AccountStore::default();
        store
            .create_account("alice", "hunter2000", "operator")
            .unwrap();
        let account = store
            .verify_password("alice", "hunter2000")
            .unwrap()
            .clone();

        let ca = test_ca();
        let session_peer_id = [7u8; 32];
        let cert = ca
            .issue_session_certificate(&account, session_peer_id, 3600)
            .unwrap();

        let claims = verify(&ca.public_key(), &cert).unwrap();
        assert_eq!(claims.account, "alice");
        assert_eq!(claims.role, "operator");
        assert_eq!(claims.session_peer_id, session_peer_id);
    }

    #[test]
    fn rejects_a_certificate_from_an_untrusted_ca() {
        let mut store = AccountStore::default();
        store
            .create_account("alice", "hunter2000", "operator")
            .unwrap();
        let account = store
            .verify_password("alice", "hunter2000")
            .unwrap()
            .clone();

        let real_ca = test_ca();
        let impostor_ca = test_ca();
        let cert = real_ca
            .issue_session_certificate(&account, [1u8; 32], 3600)
            .unwrap();

        assert!(matches!(
            verify(&impostor_ca.public_key(), &cert),
            Err(CertificateError::BadSignature)
        ));
    }

    #[test]
    fn rejects_an_expired_certificate() {
        let mut store = AccountStore::default();
        store
            .create_account("alice", "hunter2000", "operator")
            .unwrap();
        let account = store
            .verify_password("alice", "hunter2000")
            .unwrap()
            .clone();

        let ca = test_ca();
        let mut cert = ca
            .issue_session_certificate(&account, [1u8; 32], 3600)
            .unwrap();
        cert.claims.expires_at = 0; // force expiry without needing to sleep in a test

        assert!(matches!(
            verify(&ca.public_key(), &cert),
            Err(CertificateError::Expired { .. })
        ));
    }

    #[test]
    fn rejects_claims_tampered_after_signing() {
        let mut store = AccountStore::default();
        store
            .create_account("alice", "hunter2000", "operator")
            .unwrap();
        let account = store
            .verify_password("alice", "hunter2000")
            .unwrap()
            .clone();

        let ca = test_ca();
        let mut cert = ca
            .issue_session_certificate(&account, [1u8; 32], 3600)
            .unwrap();
        // Escalate the role after the CA signed it — the signature must
        // no longer match, catching exactly this kind of tampering.
        cert.claims.role = "admin".to_string();

        assert!(matches!(
            verify(&ca.public_key(), &cert),
            Err(CertificateError::BadSignature)
        ));
    }
}

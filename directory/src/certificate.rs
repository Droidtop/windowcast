//! Session certificates: what a directory issues to a logged-in account,
//! and what a host verifies to authenticate an *account* credential
//! (rather than a device-pinned one). A host that trusts one directory's
//! CA key can accept any account it vouches for, without individually
//! pinning every user the way device pairing pins one specific peer.
//!
//! Issued as [PASETO](https://paseto.io/) v4.public tokens — a standard,
//! audited "sign a short-lived claims payload" format — rather than a
//! hand-rolled bincode-then-sign scheme. PASETO's own default parser
//! enforces expiration automatically, so this module doesn't hand-roll
//! that check either.
//!
//! Deliberately session-scoped, not device-scoped: the same account can
//! log in from different client devices and get a fresh certificate each
//! time, bound to that session's own ephemeral peer id (see
//! [`SessionClaims::session_peer_id`]) — the certificate can't be replayed
//! by a different client than the one it was issued to, since a host also
//! requires the presenter to sign a fresh nonce/DTLS fingerprint with the
//! private key matching that peer id.

use rusty_paseto::prelude::*;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use windowcast_identity::PeerId;

use crate::accounts::Account;

pub const DEFAULT_SESSION_TTL_SECONDS: u64 = 12 * 60 * 60;

#[derive(Debug, thiserror::Error)]
pub enum CertificateError {
    #[error("failed to build the session certificate: {0}")]
    Build(String),
    #[error("session certificate is invalid, expired, or not signed by a trusted directory: {0}")]
    Invalid(String),
    #[error("session_peer_id in the certificate is not a valid 32-byte hex string")]
    BadPeerId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionClaims {
    pub account: String,
    pub role: String,
    /// Hex-encoded — PASETO claims are JSON, so the raw 32 bytes are
    /// carried as text; see [`SessionClaims::session_peer_id_bytes`].
    pub session_peer_id: String,
}

impl SessionClaims {
    /// Callers (host agents) still need to separately verify that whoever
    /// presented this certificate actually controls this key's private
    /// half (e.g. by requiring a signature over the connection's DTLS
    /// fingerprint) — [`verify`] only establishes that the *claims*
    /// genuinely came from a trusted directory, not who is holding them.
    pub fn session_peer_id_bytes(&self) -> Result<[u8; 32], CertificateError> {
        PeerId::from_hex(&self.session_peer_id)
            .map(|p| p.0)
            .map_err(|_| CertificateError::BadPeerId)
    }
}

/// A directory's own signing identity — the CA every issued certificate
/// chains back to. A host configured to trust this key (by its `PeerId`)
/// will accept any unexpired, validly-signed certificate this CA issues,
/// for any account.
pub struct DirectoryCa {
    identity: windowcast_identity::Identity,
}

impl DirectoryCa {
    pub fn load_or_generate(
        path: &std::path::Path,
    ) -> Result<Self, windowcast_identity::IdentityError> {
        Ok(DirectoryCa {
            identity: windowcast_identity::Identity::load_or_generate(path)?,
        })
    }

    pub fn public_key(&self) -> PeerId {
        self.identity.peer_id()
    }

    /// Issues a PASETO v4.public token (a plain `String`, safe to send
    /// over the wire as-is) binding `account` to `session_peer_id` for
    /// `ttl_seconds`.
    pub fn issue_session_certificate(
        &self,
        account: &Account,
        session_peer_id: [u8; 32],
        ttl_seconds: u64,
    ) -> Result<String, CertificateError> {
        let expiration = (OffsetDateTime::now_utc() + Duration::seconds(ttl_seconds as i64))
            .format(&Rfc3339)
            .map_err(|e| CertificateError::Build(e.to_string()))?;
        // Kept as a local so the signing key never outlives this call —
        // no leaked/'static allocation needed just to satisfy the
        // library's borrow, unlike an earlier version of this function.
        let keypair_bytes = Key::<64>::from(self.identity.to_keypair_bytes());
        let private_key = PasetoAsymmetricPrivateKey::<V4, Public>::from(&keypair_bytes);
        let session_peer_id_hex = PeerId(session_peer_id).to_hex();

        let token = PasetoBuilder::<V4, Public>::default()
            .set_claim(
                ExpirationClaim::try_from(expiration.as_str())
                    .map_err(|e| CertificateError::Build(e.to_string()))?,
            )
            .set_claim(
                CustomClaim::try_from(("account", account.username.as_str()))
                    .map_err(|e| CertificateError::Build(e.to_string()))?,
            )
            .set_claim(
                CustomClaim::try_from(("role", account.role.as_str()))
                    .map_err(|e| CertificateError::Build(e.to_string()))?,
            )
            .set_claim(
                CustomClaim::try_from(("session_peer_id", session_peer_id_hex.as_str()))
                    .map_err(|e| CertificateError::Build(e.to_string()))?,
            )
            .build(&private_key)
            .map_err(|e| CertificateError::Build(e.to_string()))?;
        Ok(token)
    }
}

/// Verifies a certificate against a trusted directory CA's public key,
/// including expiration (enforced by PASETO's own default parser — see
/// module docs). Returns the account/role/session-key claims on success.
pub fn verify(directory_ca: &PeerId, token: &str) -> Result<SessionClaims, CertificateError> {
    let public_key_bytes = Key::<32>::from(directory_ca.0);
    let public_key = PasetoAsymmetricPublicKey::<V4, Public>::from(&public_key_bytes);
    let claims: SessionClaims = PasetoParser::<V4, Public>::default()
        .parse_into(token, &public_key)
        .map_err(|e| CertificateError::Invalid(e.to_string()))?;
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::AccountStore;
    use windowcast_identity::Identity;

    fn test_ca() -> DirectoryCa {
        DirectoryCa {
            identity: Identity::generate(),
        }
    }

    fn test_account() -> Account {
        let mut store = AccountStore::default();
        store
            .create_account("alice", "hunter2000", "operator")
            .unwrap();
        store
            .verify_password("alice", "hunter2000")
            .unwrap()
            .clone()
    }

    #[test]
    fn issues_and_verifies_a_valid_certificate() {
        let account = test_account();
        let ca = test_ca();
        let session_peer_id = [7u8; 32];
        let cert = ca
            .issue_session_certificate(&account, session_peer_id, 3600)
            .unwrap();

        let claims = verify(&ca.public_key(), &cert).unwrap();
        assert_eq!(claims.account, "alice");
        assert_eq!(claims.role, "operator");
        assert_eq!(claims.session_peer_id_bytes().unwrap(), session_peer_id);
    }

    #[test]
    fn rejects_a_certificate_from_an_untrusted_ca() {
        let account = test_account();
        let real_ca = test_ca();
        let impostor_ca = test_ca();
        let cert = real_ca
            .issue_session_certificate(&account, [1u8; 32], 3600)
            .unwrap();

        assert!(verify(&impostor_ca.public_key(), &cert).is_err());
    }

    #[test]
    fn rejects_an_expired_certificate() {
        let account = test_account();
        let ca = test_ca();
        // TTL of 0 seconds: expiration is already in the past by the time
        // verify() runs, without needing to actually sleep in a test.
        let cert = ca
            .issue_session_certificate(&account, [1u8; 32], 0)
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(verify(&ca.public_key(), &cert).is_err());
    }

    #[test]
    fn rejects_a_tampered_token() {
        let account = test_account();
        let ca = test_ca();
        let mut cert = ca
            .issue_session_certificate(&account, [1u8; 32], 3600)
            .unwrap();
        // Flip a character in the payload -- PASETO tokens are
        // base64url segments joined by '.'; corrupting the payload
        // segment must fail signature verification.
        let last = cert.pop().unwrap();
        cert.push(if last == 'A' { 'B' } else { 'A' });

        assert!(verify(&ca.public_key(), &cert).is_err());
    }
}

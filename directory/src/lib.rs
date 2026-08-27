//! Account-credential authentication: a directory server's local user
//! database plus a certificate authority that mints short-lived,
//! session-scoped certificates for logged-in accounts. This is the
//! account half of windowcast's two credential types — see
//! `windowcast-identity`/`windowcast-pairing` for the device half
//! (Moonlight-style, one pinned peer key per device) and
//! `docs/SECURITY.md` in the repo root for how the two relate.
//!
//! A host that trusts a directory's CA key (`DirectoryCa::public_key`)
//! can authenticate any account that directory vouches for, without
//! individually pinning every user — the directory is the trust anchor,
//! not each client device.

mod accounts;
mod certificate;

pub use accounts::{Account, AccountError, AccountStore};
pub use certificate::{
    verify, CertificateError, DirectoryCa, SessionClaims, DEFAULT_SESSION_TTL_SECONDS,
};

/// A PASETO v4.public token string — see `certificate` module docs.
pub type SessionCertificate = String;

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error(transparent)]
    Account(#[from] AccountError),
    #[error(transparent)]
    Certificate(#[from] CertificateError),
}

/// The full login flow: verify the account's password, then mint a
/// session certificate binding it to `session_peer_id` (the client's own
/// Ed25519 public key for this session — see [`SessionClaims`]).
pub fn login(
    accounts: &AccountStore,
    ca: &DirectoryCa,
    username: &str,
    password: &str,
    session_peer_id: [u8; 32],
) -> Result<SessionCertificate, LoginError> {
    let account = accounts.verify_password(username, password)?;
    Ok(ca.issue_session_certificate(account, session_peer_id, DEFAULT_SESSION_TTL_SECONDS)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn end_to_end_login_and_verify() {
        let dir =
            std::env::temp_dir().join(format!("windowcast-directory-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        let mut accounts = AccountStore::default();
        accounts
            .create_account("alice", "hunter2000", "operator")
            .unwrap();

        let ca = DirectoryCa::load_or_generate(&dir.join("ca.key")).unwrap();
        let session_peer_id = [42u8; 32];

        let cert = login(&accounts, &ca, "alice", "hunter2000", session_peer_id).unwrap();
        let claims = verify(&ca.public_key(), &cert).unwrap();
        assert_eq!(claims.account, "alice");
        assert_eq!(claims.session_peer_id_bytes().unwrap(), session_peer_id);

        assert!(login(&accounts, &ca, "alice", "wrong password", session_peer_id).is_err());

        fs::remove_dir_all(&dir).ok();
    }
}

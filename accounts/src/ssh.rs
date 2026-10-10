//! SSH user certificates from a sign-in: a host holding an OpenSSH user CA
//! key signs a short-lived certificate for a signed-in account's SSH
//! public key, so any sshd that trusts the CA (`TrustedUserCAKeys`) admits
//! that user without a password of its own. This is how an OIDC, LDAP or
//! Kerberos sign-in reaches plain SSH servers (the command stream,
//! Droidtop/tracker#444).

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand_core::OsRng;
use ssh_key::certificate::{Builder, CertType};
use ssh_key::{Algorithm, LineEnding, PrivateKey, PublicKey};

use crate::Account;

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("ssh key: {0}")]
    Key(#[from] ssh_key::Error),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

/// An OpenSSH user certificate authority (Ed25519).
pub struct CertificateAuthority {
    key: PrivateKey,
}

impl CertificateAuthority {
    /// Loads the CA key (OpenSSH private key format, unencrypted) from
    /// `path`, making one there if there is none.
    pub fn load_or_generate(path: &Path) -> Result<Self, SshError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(CertificateAuthority {
                key: PrivateKey::from_openssh(text)?,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)?;
                std::fs::write(path, key.to_openssh(LineEnding::LF)?.as_bytes())?;
                Ok(CertificateAuthority { key })
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The CA's public key, the line an sshd's `TrustedUserCAKeys` file
    /// holds.
    pub fn public_key(&self) -> Result<String, SshError> {
        Ok(self.key.public_key().to_openssh()?)
    }

    /// Signs a user certificate for `user_key` (an OpenSSH public key
    /// line) with `account`'s name as its one principal, valid from a
    /// minute ago for `validity`.
    pub fn issue(
        &self,
        user_key: &str,
        account: &Account,
        validity: Duration,
    ) -> Result<String, SshError> {
        let user_key = PublicKey::from_openssh(user_key)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut builder = Builder::new_with_random_nonce(
            &mut OsRng,
            user_key.key_data().clone(),
            now.saturating_sub(60),
            now + validity.as_secs(),
        )?;
        builder
            .cert_type(CertType::User)?
            .key_id(format!(
                "windowcast {} {} {}",
                account.method.name(),
                account.provider,
                account.name
            ))?
            .valid_principal(account.name.clone())?;
        for extension in [
            "permit-pty",
            "permit-port-forwarding",
            "permit-agent-forwarding",
            "permit-X11-forwarding",
            "permit-user-rc",
        ] {
            builder.extension(extension, "")?;
        }
        Ok(builder.sign(&self.key)?.to_openssh()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Method;
    use ssh_key::{Certificate, HashAlg};

    #[test]
    fn issues_a_certificate_sshd_would_accept() {
        let dir = std::env::temp_dir().join(format!("windowcast-ssh-ca-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ssh-user-ca");
        let ca = CertificateAuthority::load_or_generate(&path).unwrap();
        let again = CertificateAuthority::load_or_generate(&path).unwrap();
        assert_eq!(ca.public_key().unwrap(), again.public_key().unwrap());

        let user = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
        let account = Account {
            name: "alice".into(),
            groups: Vec::new(),
            method: Method::Oidc,
            provider: "corp".into(),
        };
        let text = ca
            .issue(
                &user.public_key().to_openssh().unwrap(),
                &account,
                Duration::from_secs(600),
            )
            .unwrap();
        let certificate = Certificate::from_openssh(&text).unwrap();
        let fingerprint = ca.key.public_key().fingerprint(HashAlg::Sha256);
        certificate.validate([&fingerprint]).unwrap();
        assert_eq!(certificate.valid_principals(), ["alice".to_owned()]);
        assert_eq!(certificate.cert_type(), CertType::User);
        assert_eq!(certificate.public_key(), user.public_key().key_data());
        std::fs::remove_dir_all(&dir).ok();
    }
}

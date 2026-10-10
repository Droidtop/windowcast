//! SSH user certificates from a sign-in: a host holding an OpenSSH user CA
//! key signs a short-lived certificate for a signed-in account's SSH
//! public key, so any sshd that trusts the CA (`TrustedUserCAKeys`) admits
//! that user without a password of its own. This is how an OIDC, LDAP or
//! Kerberos sign-in reaches plain SSH servers (the command stream,
//! Droidtop/tracker#444).
//!
//! The client's half: its own SSH key ([`UserKey`]), kept beside its
//! device identity, is the key a host certifies; the certificate is
//! checked against it before a login uses it.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand_core::OsRng;
use ssh_key::certificate::{Builder, CertType};
use ssh_key::{Algorithm, Certificate, LineEnding, PrivateKey, PublicKey};

use crate::Account;

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("ssh key: {0}")]
    Key(#[from] ssh_key::Error),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("the certificate is not for this key")]
    NotThisKey,
}

/// Loads an Ed25519 private key (OpenSSH format, unencrypted) from `path`,
/// making one there, readable by its owner only, if there is none.
fn load_or_generate(path: &Path) -> Result<PrivateKey, SshError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(PrivateKey::from_openssh(text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)?;
            let text = key.to_openssh(LineEnding::LF)?;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            std::io::Write::write_all(&mut options.open(path)?, text.as_bytes())?;
            Ok(key)
        }
        Err(e) => Err(e.into()),
    }
}

/// An OpenSSH user certificate authority (Ed25519).
pub struct CertificateAuthority {
    key: PrivateKey,
}

impl CertificateAuthority {
    /// Loads the CA key (OpenSSH private key format, unencrypted) from
    /// `path`, making one there if there is none.
    pub fn load_or_generate(path: &Path) -> Result<Self, SshError> {
        Ok(CertificateAuthority {
            key: load_or_generate(path)?,
        })
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

/// A client's own SSH key (Ed25519), the one a host's certificate
/// authority certifies for the account the client signed in with.
pub struct UserKey {
    key: PrivateKey,
}

impl UserKey {
    /// Loads the key from `path`, making one there if there is none.
    pub fn load_or_generate(path: &Path) -> Result<Self, SshError> {
        Ok(UserKey {
            key: load_or_generate(path)?,
        })
    }

    /// The public key as an OpenSSH line, what a certificate is asked for.
    pub fn public_key(&self) -> Result<String, SshError> {
        Ok(self.key.public_key().to_openssh()?)
    }

    /// The private key in OpenSSH form, for an SSH client to log in with.
    pub fn private_key(&self) -> Result<String, SshError> {
        Ok(self.key.to_openssh(LineEnding::LF)?.to_string())
    }

    /// Checks that `certificate` (an OpenSSH certificate line) is a user
    /// certificate for this key, and gives it back trimmed.
    pub fn check_certificate(&self, certificate: &str) -> Result<String, SshError> {
        let certificate = certificate.trim();
        let parsed = Certificate::from_openssh(certificate)?;
        if parsed.cert_type() != CertType::User
            || parsed.public_key() != self.key.public_key().key_data()
        {
            return Err(SshError::NotThisKey);
        }
        Ok(certificate.to_owned())
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

    #[test]
    fn a_user_key_takes_only_its_own_certificates() {
        let dir = std::env::temp_dir().join(format!("windowcast-ssh-user-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ca = CertificateAuthority::load_or_generate(&dir.join("ca")).unwrap();
        let mine = UserKey::load_or_generate(&dir.join("mine")).unwrap();
        let again = UserKey::load_or_generate(&dir.join("mine")).unwrap();
        assert_eq!(mine.public_key().unwrap(), again.public_key().unwrap());
        assert!(mine
            .private_key()
            .unwrap()
            .starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("mine"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "{mode:o}");
        }
        let theirs = UserKey::load_or_generate(&dir.join("theirs")).unwrap();
        let account = Account {
            name: "alice".into(),
            groups: Vec::new(),
            method: Method::Password,
            provider: "local".into(),
        };
        let validity = Duration::from_secs(600);
        let for_me = ca
            .issue(&mine.public_key().unwrap(), &account, validity)
            .unwrap();
        let for_them = ca
            .issue(&theirs.public_key().unwrap(), &account, validity)
            .unwrap();
        assert_eq!(
            mine.check_certificate(&format!("{for_me}\n")).unwrap(),
            for_me
        );
        assert!(matches!(
            mine.check_certificate(&for_them),
            Err(SshError::NotThisKey)
        ));
        assert!(mine.check_certificate("ssh-ed25519 nonsense").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}

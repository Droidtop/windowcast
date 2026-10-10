//! A host's own accounts: usernames with Argon2id password hashes and the
//! groups policy rules name, kept as JSON in the host's data folder. The
//! simplest password source (`PasswordSource::Local`); the host OS and
//! LDAP are the others.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand_core::OsRng;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    #[error("account {0:?} not found")]
    NotFound(String),
    #[error("account {0:?} already exists")]
    AlreadyExists(String),
    #[error("wrong password")]
    WrongPassword,
    #[error("password hashing failed: {0}")]
    Hash(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse the account store: {0}")]
    Parse(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalAccount {
    pub username: String,
    /// A PHC string (`$argon2id$v=19$...`), salt included.
    password_hash: String,
    pub groups: Vec<String>,
    pub revoked: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LocalAccounts {
    accounts: HashMap<String, LocalAccount>,
}

impl LocalAccounts {
    pub fn load(path: &Path) -> Result<Self, LocalError> {
        match fs::read_to_string(path) {
            Ok(contents) => Ok(serde_json::from_str(&contents)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LocalAccounts::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), LocalError> {
        fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn create(
        &mut self,
        username: &str,
        password: &str,
        groups: &[String],
    ) -> Result<(), LocalError> {
        if self.accounts.contains_key(username) {
            return Err(LocalError::AlreadyExists(username.to_string()));
        }
        let password_hash = hash_password(password)?;
        self.accounts.insert(
            username.to_string(),
            LocalAccount {
                username: username.to_string(),
                password_hash,
                groups: groups.to_vec(),
                revoked: false,
            },
        );
        Ok(())
    }

    pub fn set_password(&mut self, username: &str, password: &str) -> Result<(), LocalError> {
        let hash = hash_password(password)?;
        self.get_mut(username)?.password_hash = hash;
        Ok(())
    }

    pub fn set_groups(&mut self, username: &str, groups: &[String]) -> Result<(), LocalError> {
        self.get_mut(username)?.groups = groups.to_vec();
        Ok(())
    }

    /// Stops the account signing in. Devices it registered stop at their
    /// next connection: the host checks the account is still there.
    pub fn revoke(&mut self, username: &str) -> Result<(), LocalError> {
        self.get_mut(username)?.revoked = true;
        Ok(())
    }

    pub fn remove(&mut self, username: &str) -> Result<(), LocalError> {
        self.accounts
            .remove(username)
            .map(|_| ())
            .ok_or_else(|| LocalError::NotFound(username.to_string()))
    }

    /// Whether `username` exists and is not revoked.
    pub fn active(&self, username: &str) -> bool {
        self.accounts.get(username).is_some_and(|a| !a.revoked)
    }

    /// Verifies a sign-in. A revoked account fails like a wrong password;
    /// [`LocalError`]'s variants are for the host's own log and choice of
    /// the next source, never for the other side, which learns only
    /// "authentication failed".
    pub fn verify_password(
        &self,
        username: &str,
        password: &str,
    ) -> Result<&LocalAccount, LocalError> {
        let account = self
            .accounts
            .get(username)
            .ok_or_else(|| LocalError::NotFound(username.to_string()))?;
        if account.revoked {
            return Err(LocalError::WrongPassword);
        }
        let parsed_hash = PasswordHash::new(&account.password_hash)
            .map_err(|e| LocalError::Hash(e.to_string()))?;
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed_hash)
            .map_err(|_| LocalError::WrongPassword)?;
        Ok(account)
    }

    pub fn list(&self) -> impl Iterator<Item = &LocalAccount> {
        self.accounts.values()
    }

    fn get_mut(&mut self, username: &str) -> Result<&mut LocalAccount, LocalError> {
        self.accounts
            .get_mut(username)
            .ok_or_else(|| LocalError::NotFound(username.to_string()))
    }
}

/// Argon2id (the crate's default algorithm and parameters) with a random
/// salt, as a PHC string.
fn hash_password(password: &str) -> Result<String, LocalError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| LocalError::Hash(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_verifies_an_account() {
        let mut store = LocalAccounts::default();
        store
            .create("alice", "correct horse battery staple", &[])
            .unwrap();
        let hash = &store.accounts["alice"].password_hash;
        assert!(hash.starts_with("$argon2id$"), "{hash}");

        assert!(store
            .verify_password("alice", "correct horse battery staple")
            .is_ok());
        assert!(matches!(
            store.verify_password("alice", "wrong"),
            Err(LocalError::WrongPassword)
        ));
        assert!(matches!(
            store.verify_password("bob", "anything"),
            Err(LocalError::NotFound(_))
        ));
    }

    #[test]
    fn revoked_account_fails_sign_in() {
        let mut store = LocalAccounts::default();
        store.create("alice", "hunter2000", &[]).unwrap();
        store.revoke("alice").unwrap();
        assert!(!store.active("alice"));
        assert!(matches!(
            store.verify_password("alice", "hunter2000"),
            Err(LocalError::WrongPassword)
        ));
    }

    #[test]
    fn changes_password_and_groups() {
        let mut store = LocalAccounts::default();
        store.create("alice", "old password", &[]).unwrap();
        store.set_password("alice", "new password").unwrap();
        store.set_groups("alice", &["admins".into()]).unwrap();
        assert!(store.verify_password("alice", "old password").is_err());
        let account = store.verify_password("alice", "new password").unwrap();
        assert_eq!(account.groups, vec!["admins".to_owned()]);
    }

    #[test]
    fn persists_across_reloads() {
        let dir =
            std::env::temp_dir().join(format!("windowcast-local-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.json");

        let mut store = LocalAccounts::default();
        store
            .create("alice", "hunter2000", &["staff".into()])
            .unwrap();
        store.save(&path).unwrap();

        let reloaded = LocalAccounts::load(&path).unwrap();
        assert!(reloaded.verify_password("alice", "hunter2000").is_ok());

        fs::remove_dir_all(&dir).ok();
    }
}

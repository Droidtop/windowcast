//! User accounts: the root of trust for the *account* credential type
//! (distinct from the device-pinned credential in `windowcast-identity`).
//! A device credential identifies one specific machine (Moonlight-style);
//! an account credential identifies a person, who may log in from any
//! client device — that's the whole point of separating the two.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand_core::OsRng;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum AccountError {
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
    #[error("failed to parse account store: {0}")]
    Parse(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub username: String,
    password_hash: String,
    /// Opaque to this crate — interpreted by whatever authorization logic
    /// a host layers on top (see the project's authorization model).
    /// Not a fixed enum: windowcast itself doesn't enforce role semantics,
    /// only carries the label through a signed certificate.
    pub role: String,
    pub revoked: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AccountStore {
    accounts: HashMap<String, Account>,
}

impl AccountStore {
    pub fn load(path: &Path) -> Result<Self, AccountError> {
        match fs::read_to_string(path) {
            Ok(contents) => Ok(serde_json::from_str(&contents)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AccountStore::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), AccountError> {
        fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn create_account(
        &mut self,
        username: &str,
        password: &str,
        role: &str,
    ) -> Result<(), AccountError> {
        if self.accounts.contains_key(username) {
            return Err(AccountError::AlreadyExists(username.to_string()));
        }
        let hash = hash_password(password)?;
        self.accounts.insert(
            username.to_string(),
            Account {
                username: username.to_string(),
                password_hash: hash,
                role: role.to_string(),
                revoked: false,
            },
        );
        Ok(())
    }

    pub fn revoke(&mut self, username: &str) -> Result<(), AccountError> {
        let account = self
            .accounts
            .get_mut(username)
            .ok_or_else(|| AccountError::NotFound(username.to_string()))?;
        account.revoked = true;
        Ok(())
    }

    /// Verifies a login attempt. Fails closed on both "no such account" and
    /// "revoked" the same way it fails on a wrong password — a login
    /// endpoint that distinguishes those cases in its response would leak
    /// which usernames exist, so that distinction only exists internally
    /// via [`AccountError`]'s variants for the caller's own logging, not
    /// meant to be echoed back to whoever is attempting the login.
    pub fn verify_password(
        &self,
        username: &str,
        password: &str,
    ) -> Result<&Account, AccountError> {
        let account = self
            .accounts
            .get(username)
            .ok_or_else(|| AccountError::NotFound(username.to_string()))?;
        if account.revoked {
            return Err(AccountError::WrongPassword);
        }
        let parsed_hash = PasswordHash::new(&account.password_hash)
            .map_err(|e| AccountError::Hash(e.to_string()))?;
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed_hash)
            .map_err(|_| AccountError::WrongPassword)?;
        Ok(account)
    }

    pub fn list(&self) -> impl Iterator<Item = &Account> {
        self.accounts.values()
    }
}

fn hash_password(password: &str) -> Result<String, AccountError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AccountError::Hash(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_verifies_an_account() {
        let mut store = AccountStore::default();
        store
            .create_account("alice", "correct horse battery staple", "operator")
            .unwrap();

        assert!(store
            .verify_password("alice", "correct horse battery staple")
            .is_ok());
        assert!(matches!(
            store.verify_password("alice", "wrong"),
            Err(AccountError::WrongPassword)
        ));
        assert!(matches!(
            store.verify_password("bob", "anything"),
            Err(AccountError::NotFound(_))
        ));
    }

    #[test]
    fn revoked_account_fails_login() {
        let mut store = AccountStore::default();
        store
            .create_account("alice", "hunter2000", "viewer")
            .unwrap();
        store.revoke("alice").unwrap();
        assert!(matches!(
            store.verify_password("alice", "hunter2000"),
            Err(AccountError::WrongPassword)
        ));
    }

    #[test]
    fn persists_across_reloads() {
        let dir =
            std::env::temp_dir().join(format!("windowcast-accounts-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.json");

        let mut store = AccountStore::default();
        store
            .create_account("alice", "hunter2000", "operator")
            .unwrap();
        store.save(&path).unwrap();

        let reloaded = AccountStore::load(&path).unwrap();
        assert!(reloaded.verify_password("alice", "hunter2000").is_ok());

        fs::remove_dir_all(&dir).ok();
    }
}

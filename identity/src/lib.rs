//! Persistent Ed25519 identity for a windowcast peer (client or host agent),
//! plus a simple pinned-peer trust store. This is the long-term-identity
//! half of the security model: after the first PAKE-authenticated pairing
//! (see `windowcast-pairing`), every later connection authenticates via
//! these keys instead of re-running the PIN exchange.

use std::fs;
use std::io;
use std::path::Path;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey, SECRET_KEY_LENGTH};
use rand_core::OsRng;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("io error loading/saving identity: {0}")]
    Io(#[from] io::Error),
    #[error("identity file is corrupt (expected {SECRET_KEY_LENGTH} bytes)")]
    Corrupt,
}

/// A peer's public key, in the form pinned/compared/persisted everywhere
/// outside this crate. Hex-encoded for display and file storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerId(pub [u8; 32]);

impl PeerId {
    pub fn from_verifying_key(key: &VerifyingKey) -> Self {
        PeerId(key.to_bytes())
    }

    pub fn to_verifying_key(&self) -> Result<VerifyingKey, IdentityError> {
        VerifyingKey::from_bytes(&self.0).map_err(|_| IdentityError::Corrupt)
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(s: &str) -> Result<Self, IdentityError> {
        if s.len() != 64 {
            return Err(IdentityError::Corrupt);
        }
        let mut bytes = [0u8; 32];
        for i in 0..32 {
            bytes[i] =
                u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| IdentityError::Corrupt)?;
        }
        Ok(PeerId(bytes))
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

/// A persistent Ed25519 keypair for this peer, generated once and reused
/// across sessions — a host or client that generated a new identity every
/// launch would make every pinned trust relationship useless.
pub struct Identity {
    signing_key: SigningKey,
}

impl Identity {
    pub fn generate() -> Self {
        Identity {
            signing_key: SigningKey::generate(&mut OsRng),
        }
    }

    /// Loads the identity from `path` if it exists, otherwise generates a
    /// new one and persists it there. `path`'s parent directory must
    /// already exist (callers own where identity state lives — e.g.
    /// app-private storage on Android, `$XDG_DATA_HOME` on Linux).
    pub fn load_or_generate(path: &Path) -> Result<Self, IdentityError> {
        if let Ok(bytes) = fs::read(path) {
            let seed: [u8; SECRET_KEY_LENGTH] =
                bytes.try_into().map_err(|_| IdentityError::Corrupt)?;
            return Ok(Identity {
                signing_key: SigningKey::from_bytes(&seed),
            });
        }
        let identity = Self::generate();
        fs::write(path, identity.signing_key.to_bytes())?;
        Ok(identity)
    }

    pub fn peer_id(&self) -> PeerId {
        PeerId::from_verifying_key(&self.signing_key.verifying_key())
    }

    pub fn sign(&self, message: &[u8]) -> Signature {
        self.signing_key.sign(message)
    }

    /// The raw 64-byte Ed25519 keypair (32-byte seed + 32-byte public key)
    /// — the form other libraries that consume a raw Ed25519 key (e.g. a
    /// PASETO v4.public signer) typically expect, rather than this crate's
    /// own [`Identity`]/[`PeerId`] wrapper types.
    pub fn to_keypair_bytes(&self) -> [u8; 64] {
        self.signing_key.to_keypair_bytes()
    }
}

pub fn verify(peer: &PeerId, message: &[u8], signature: &Signature) -> bool {
    match peer.to_verifying_key() {
        Ok(key) => key.verify(message, signature).is_ok(),
        Err(_) => false,
    }
}

/// A host's (or client's) list of pinned peers it trusts, persisted as one
/// hex pubkey per line. Deliberately dumb storage — authorization *scope*
/// per peer (which windows they may see) is a separate concern layered on
/// top by whatever embeds this crate, not stored here.
#[derive(Debug, Default, Clone)]
pub struct TrustStore {
    pinned: std::collections::HashSet<PeerId>,
}

impl TrustStore {
    pub fn load(path: &Path) -> Result<Self, IdentityError> {
        let mut pinned = std::collections::HashSet::new();
        if let Ok(contents) = fs::read_to_string(path) {
            for line in contents.lines().filter(|l| !l.trim().is_empty()) {
                pinned.insert(PeerId::from_hex(line.trim())?);
            }
        }
        Ok(TrustStore { pinned })
    }

    pub fn save(&self, path: &Path) -> Result<(), IdentityError> {
        let contents: String = self.pinned.iter().map(|p| format!("{p}\n")).collect();
        fs::write(path, contents)?;
        Ok(())
    }

    pub fn pin(&mut self, peer: PeerId) {
        self.pinned.insert(peer);
    }

    pub fn revoke(&mut self, peer: &PeerId) {
        self.pinned.remove(peer);
    }

    pub fn is_pinned(&self, peer: &PeerId) -> bool {
        self.pinned.contains(peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_and_verifies() {
        let identity = Identity::generate();
        let sig = identity.sign(b"hello");
        assert!(verify(&identity.peer_id(), b"hello", &sig));
        assert!(!verify(&identity.peer_id(), b"tampered", &sig));
    }

    #[test]
    fn persists_across_reloads() {
        let dir =
            std::env::temp_dir().join(format!("windowcast-identity-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("identity.key");

        let first = Identity::load_or_generate(&path).unwrap();
        let second = Identity::load_or_generate(&path).unwrap();
        assert_eq!(first.peer_id(), second.peer_id());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn peer_id_hex_round_trips() {
        let identity = Identity::generate();
        let hex = identity.peer_id().to_hex();
        assert_eq!(PeerId::from_hex(&hex).unwrap(), identity.peer_id());
    }

    #[test]
    fn trust_store_pin_and_revoke() {
        let peer = Identity::generate().peer_id();
        let mut store = TrustStore::default();
        assert!(!store.is_pinned(&peer));
        store.pin(peer);
        assert!(store.is_pinned(&peer));
        store.revoke(&peer);
        assert!(!store.is_pinned(&peer));
    }
}

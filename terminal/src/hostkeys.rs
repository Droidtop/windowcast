//! Pinned SSH server identities. The first connection to a server records
//! its host key's fingerprint (trust on first use) or checks it against a
//! fingerprint the user typed; every later connection must present the
//! same key, and a changed key is refused, never replaced silently.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// What the store knows about a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Known {
    Unknown,
    /// Pinned, and the key matches.
    Match,
    /// Pinned to another key.
    Changed {
        pinned: String,
    },
}

/// Asks the user whether to trust a server's key: called with the server
/// and the fingerprint it presents.
pub type AskUser = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;

/// How an unknown server is dealt with.
#[derive(Clone)]
pub enum HostKeyPolicy {
    /// Record the key the server presents.
    TrustOnFirstUse,
    /// Accept only this fingerprint (`SHA256:...`, as `ssh-keygen -l`
    /// prints it), and record it.
    Fingerprint(String),
    /// Ask the user: called with the server and the fingerprint it
    /// presents; `true` pins it.
    Ask(AskUser),
    /// Refuse servers not pinned already.
    Pinned,
}

/// The pinned fingerprints, one per `host:port`, kept in a JSON file.
pub struct HostKeyStore {
    path: Option<PathBuf>,
    pins: Mutex<BTreeMap<String, String>>,
}

impl HostKeyStore {
    /// A store that remembers nothing past this process.
    pub fn in_memory() -> Arc<Self> {
        Arc::new(HostKeyStore {
            path: None,
            pins: Mutex::default(),
        })
    }

    /// Loads (or starts) the store kept in `path`.
    pub fn open(path: &Path) -> std::io::Result<Arc<Self>> {
        let pins = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e),
        };
        Ok(Arc::new(HostKeyStore {
            path: Some(path.to_owned()),
            pins: Mutex::new(pins),
        }))
    }

    pub fn check(&self, server: &str, fingerprint: &str) -> Known {
        match self.pins.lock().expect("pins").get(server) {
            None => Known::Unknown,
            Some(pinned) if pinned == fingerprint => Known::Match,
            Some(pinned) => Known::Changed {
                pinned: pinned.clone(),
            },
        }
    }

    pub fn pin(&self, server: &str, fingerprint: &str) -> std::io::Result<()> {
        let mut pins = self.pins.lock().expect("pins");
        pins.insert(server.to_owned(), fingerprint.to_owned());
        self.save(&pins)
    }

    /// Forgets a server, for a user who knows its key changed.
    pub fn forget(&self, server: &str) -> std::io::Result<()> {
        let mut pins = self.pins.lock().expect("pins");
        pins.remove(server);
        self.save(&pins)
    }

    fn save(&self, pins: &BTreeMap<String, String>) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let temp = path.with_extension("tmp");
        std::fs::write(
            &temp,
            serde_json::to_vec_pretty(pins).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(temp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_survive_a_restart_and_a_changed_key_is_noticed() {
        let dir = std::env::temp_dir().join(format!("wc-hostkeys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known.json");
        let store = HostKeyStore::open(&path).unwrap();
        assert_eq!(store.check("a:22", "SHA256:one"), Known::Unknown);
        store.pin("a:22", "SHA256:one").unwrap();

        let again = HostKeyStore::open(&path).unwrap();
        assert_eq!(again.check("a:22", "SHA256:one"), Known::Match);
        assert_eq!(
            again.check("a:22", "SHA256:two"),
            Known::Changed {
                pinned: "SHA256:one".into()
            }
        );
        again.forget("a:22").unwrap();
        assert_eq!(again.check("a:22", "SHA256:two"), Known::Unknown);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

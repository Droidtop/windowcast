//! Devices registered to accounts on a host: what a sign-in leaves
//! behind, so later connections resume with the device key alone. Kept as
//! JSON beside the host's trusted-client list.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use windowcast_identity::PeerId;

use crate::Account;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Registration {
    pub account: Account,
    /// Seconds since the Unix epoch.
    pub registered: u64,
    /// Seconds since the Unix epoch; the device must sign in again after.
    pub expires: u64,
}

#[derive(Debug)]
pub struct Registrations {
    path: PathBuf,
    /// By device key, hex.
    devices: HashMap<String, Registration>,
}

impl Registrations {
    /// Loads them from `path`; a missing or unreadable file is none.
    pub fn load(path: &Path) -> Self {
        let devices = fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Registrations {
            path: path.to_owned(),
            devices,
        }
    }

    /// Registers `device` to `account` for `days` days (0: this connection
    /// only, nothing is kept) and saves.
    pub fn register(&mut self, device: PeerId, account: Account, days: u32) -> std::io::Result<()> {
        if days == 0 {
            return Ok(());
        }
        let now = now();
        self.devices.insert(
            device.to_hex(),
            Registration {
                account,
                registered: now,
                expires: now + u64::from(days) * 24 * 60 * 60,
            },
        );
        self.save()
    }

    /// The account `device` is registered to, unless it has expired.
    pub fn get(&self, device: &PeerId) -> Option<&Registration> {
        self.devices
            .get(&device.to_hex())
            .filter(|r| r.expires > now())
    }

    /// Every unexpired registration.
    pub fn active(&self) -> impl Iterator<Item = (PeerId, &Registration)> {
        let now = now();
        self.devices
            .iter()
            .filter(move |(_, r)| r.expires > now)
            .filter_map(|(key, r)| PeerId::from_hex(key).ok().map(|peer| (peer, r)))
    }

    /// Forgets `device`: it has to sign in again.
    pub fn remove(&mut self, device: &PeerId) -> std::io::Result<()> {
        self.devices.remove(&device.to_hex());
        self.save()
    }

    /// Forgets every device of the account named `name` from `provider`.
    pub fn remove_account(&mut self, name: &str, provider: &str) -> std::io::Result<()> {
        self.devices
            .retain(|_, r| !(r.account.name == name && r.account.provider == provider));
        self.save()
    }

    fn save(&self) -> std::io::Result<()> {
        let now = now();
        let kept: HashMap<_, _> = self
            .devices
            .iter()
            .filter(|(_, r)| r.expires > now)
            .collect();
        fs::write(
            &self.path,
            serde_json::to_string_pretty(&kept).map_err(std::io::Error::other)?,
        )
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Method;

    #[test]
    fn registers_reloads_and_forgets() {
        let dir = std::env::temp_dir().join(format!(
            "windowcast-registrations-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registrations.json");
        let device = PeerId([9; 32]);
        let account = Account {
            name: "alice".into(),
            groups: vec!["staff".into()],
            method: Method::Oidc,
            provider: "corp".into(),
        };

        let mut registrations = Registrations::load(&path);
        registrations
            .register(PeerId([8; 32]), account.clone(), 0)
            .unwrap();
        assert!(registrations.get(&PeerId([8; 32])).is_none());
        registrations.register(device, account.clone(), 30).unwrap();

        let reloaded = Registrations::load(&path);
        assert_eq!(reloaded.get(&device).unwrap().account, account);
        assert_eq!(reloaded.active().count(), 1);

        let mut reloaded = reloaded;
        reloaded.remove_account("alice", "corp").unwrap();
        assert!(Registrations::load(&path).get(&device).is_none());
        fs::remove_dir_all(&dir).ok();
    }
}

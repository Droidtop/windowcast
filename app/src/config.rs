//! The app's configuration: which roles it runs and each role's settings,
//! kept as JSON in the data folder and saved whenever the UI changes them.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use windowcast_protocol::selection::BackendRule;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Roles {
    Host,
    Client,
    Both,
}

impl Roles {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "host" => Some(Self::Host),
            "client" => Some(Self::Client),
            "both" => Some(Self::Both),
            _ => None,
        }
    }

    pub fn host(self) -> bool {
        matches!(self, Self::Host | Self::Both)
    }

    pub fn client(self) -> bool {
        matches!(self, Self::Client | Self::Both)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub roles: Roles,
    pub host: HostSettings,
    pub client: ClientSettings,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            roles: Roles::Both,
            host: HostSettings::default(),
            client: ClientSettings::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostSettings {
    /// Signaling address, `ADDR:PORT`.
    pub listen: String,
    /// Whether a PIN is shown at start for a new client to pair.
    pub pairing: bool,
    /// The agent's encoder option (`auto`, `nvenc`, `quicksync`, ...).
    pub encoder: String,
    /// Offer only this codec (`H264`, `H265`, `Av1`); empty offers all.
    pub codec: String,
    pub fps: u32,
    /// Megabits per second at 1920x1080, scaled by area.
    pub bitrate_mbps: f32,
    /// Let clients drive the windows they stream (pointer, keys, text).
    pub input: bool,
    /// Share the clipboard's text with clients, both ways.
    pub clipboard: bool,
}

impl Default for HostSettings {
    fn default() -> Self {
        HostSettings {
            listen: "0.0.0.0:47100".into(),
            pairing: true,
            encoder: "auto".into(),
            codec: String::new(),
            fps: 30,
            bitrate_mbps: 8.0,
            input: false,
            clipboard: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientSettings {
    /// Hosts this client paired with, newest first.
    pub saved: Vec<SavedHost>,
    /// The user's backend rules (per-app overrides), before the defaults.
    pub rules: Vec<BackendRule>,
    /// Send pointer and keys from the stream windows to the host.
    pub send_input: bool,
    /// Stream windows open borderless, covering their display.
    pub fullscreen: bool,
    /// The display stream windows open on, by Windows' number (1 is
    /// DISPLAY1); 0 for the primary display.
    pub display: usize,
    /// The codec to ask for (`H264`, `H265`, `Av1`); empty: the best this
    /// PC decodes.
    pub codec: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedHost {
    pub address: String,
    pub host_id: String,
}

/// The configuration and the file it lives in.
pub struct Store {
    path: PathBuf,
    config: Mutex<Config>,
}

impl Store {
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("config.json");
        let config = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| match serde_json::from_str(&text) {
                Ok(config) => Some(config),
                Err(e) => {
                    eprintln!("ignoring {}: {e}", path.display());
                    None
                }
            })
            .unwrap_or_default();
        Store {
            path,
            config: Mutex::new(config),
        }
    }

    pub fn get(&self) -> Config {
        self.config.lock().expect("config").clone()
    }

    /// Changes the configuration and saves it.
    pub fn update(&self, change: impl FnOnce(&mut Config)) {
        let mut config = self.config.lock().expect("config");
        change(&mut config);
        let text = serde_json::to_string_pretty(&*config).expect("config serializes");
        if let Err(e) = std::fs::write(&self.path, text) {
            eprintln!("could not save {}: {e}", self.path.display());
        }
    }
}

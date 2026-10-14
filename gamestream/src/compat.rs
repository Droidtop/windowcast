//! Compatibility mode's GameStream endpoint (docs/BACKENDS.md,
//! "Third-party clients and RemoteApp"): the windows in compatibility mode
//! are the apps stock Moonlight sees. The host's GameStream face starts the
//! first time a window needs it, on the standard GameStream ports, and
//! stays up; Moonlight pairs with it by the PIN its owner types on the host
//! (`GameStreamCompat::server`, `GameStreamServer::enter_pin`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use windowcast_host::compat::CompatProvider;
use windowcast_host::WindowSource;
use windowcast_protocol::{CompatEndpoint, WindowId};

use crate::client::App;
use crate::rtsp::RTSP_PORT;
use crate::server::{GameStreamServer, HTTPS_PORT};
use crate::stream::{Apps, InputSink, StreamConfig, VideoSource};
use crate::windows::{app_id, WindowApps};

/// GameStream's plain HTTP port.
pub const HTTP_PORT: u16 = 47989;

/// The windows in compatibility mode, as apps.
struct CompatApps {
    windows: WindowApps,
    shown: RwLock<HashMap<WindowId, String>>,
}

impl CompatApps {
    fn shows(&self, app: u32) -> bool {
        self.shown
            .read()
            .expect("shown")
            .keys()
            .any(|w| app_id(*w) == app)
    }
}

impl Apps for CompatApps {
    fn apps(&self) -> Vec<App> {
        let mut apps: Vec<App> = self
            .shown
            .read()
            .expect("shown")
            .iter()
            .map(|(window, title)| App {
                id: app_id(*window),
                title: title.clone(),
                hdr: false,
            })
            .collect();
        apps.sort_by_key(|app| app.id);
        apps
    }

    fn open(&self, app: u32, config: &StreamConfig) -> Result<Box<dyn VideoSource>, String> {
        if !self.shows(app) {
            return Err("that window is not in compatibility mode".into());
        }
        self.windows.open(app, config)
    }

    fn input(&self, app: u32, config: &StreamConfig) -> Option<Box<dyn InputSink>> {
        self.shows(app).then(|| self.windows.input(app, config))?
    }

    fn audio(&self, app: u32) -> Option<Box<dyn windowcast_host::audio::AudioSource>> {
        self.shows(app).then(|| self.windows.audio(app))?
    }
}

/// The GameStream endpoint, and the server once it runs.
pub struct GameStreamCompat {
    apps: Arc<CompatApps>,
    server: Mutex<Option<Arc<GameStreamServer>>>,
    name: String,
    dir: PathBuf,
    bind: std::net::IpAddr,
    runtime: tokio::runtime::Handle,
}

impl GameStreamCompat {
    /// For the host `name` serving `source`'s windows, its credentials and
    /// paired clients kept in `dir`, listening on `bind`, served on
    /// `runtime`.
    pub fn new(
        name: &str,
        source: Arc<dyn WindowSource>,
        dir: PathBuf,
        bind: std::net::IpAddr,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        GameStreamCompat {
            apps: Arc::new(CompatApps {
                windows: WindowApps(source),
                shown: RwLock::default(),
            }),
            server: Mutex::new(None),
            name: name.to_owned(),
            dir,
            bind,
            runtime,
        }
    }

    /// The running server, for its pairing requests and PIN.
    pub fn server(&self) -> Option<Arc<GameStreamServer>> {
        self.server.lock().expect("server").clone()
    }

    fn start(&self) -> Result<Arc<GameStreamServer>, String> {
        let mut server = self.server.lock().expect("server");
        if let Some(running) = &*server {
            return Ok(Arc::clone(running));
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let started = GameStreamServer::open(&self.name, &self.dir, Arc::clone(&self.apps) as _)
            .map_err(|e| e.to_string())?;
        let bind = |port: u16| {
            let listener = std::net::TcpListener::bind((self.bind, port))
                .map_err(|e| format!("GameStream's port {port} is taken ({e})"))?;
            listener.set_nonblocking(true).map_err(|e| e.to_string())?;
            Ok::<_, String>(listener)
        };
        let (http, https, rtsp) = (bind(HTTP_PORT)?, bind(HTTPS_PORT)?, bind(RTSP_PORT)?);
        let serving = Arc::clone(&started);
        self.runtime.spawn(async move {
            let listeners = (
                tokio::net::TcpListener::from_std(http),
                tokio::net::TcpListener::from_std(https),
                tokio::net::TcpListener::from_std(rtsp),
            );
            match listeners {
                (Ok(http), Ok(https), Ok(rtsp)) => {
                    if let Err(e) = serving.serve(http, https, rtsp).await {
                        eprintln!("gamestream: {e}");
                    }
                }
                _ => eprintln!("gamestream: could not listen"),
            }
        });
        *server = Some(Arc::clone(&started));
        Ok(started)
    }
}

/// Takes a window out of the app list when dropped.
struct Shown {
    apps: Arc<CompatApps>,
    window: WindowId,
}

impl Drop for Shown {
    fn drop(&mut self) {
        self.apps.shown.write().expect("shown").remove(&self.window);
    }
}

impl CompatProvider for GameStreamCompat {
    fn open(
        &self,
        window: WindowId,
        title: &str,
    ) -> Result<(CompatEndpoint, Box<dyn Send + Sync>), String> {
        self.start()?;
        self.apps
            .shown
            .write()
            .expect("shown")
            .insert(window, title.to_owned());
        Ok((
            CompatEndpoint::GameStream {
                https_port: HTTPS_PORT,
                app_id: app_id(window),
                title: title.to_owned(),
            },
            Box::new(Shown {
                apps: Arc::clone(&self.apps),
                window,
            }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windowcast_host::{FrameSource, PictureSource};
    use windowcast_protocol::{VideoCodec, WindowInfo};

    /// Two windows, 1 and 2.
    struct Two;

    impl WindowSource for Two {
        fn list_windows(&self) -> Vec<WindowInfo> {
            [1, 2]
                .into_iter()
                .map(|id| WindowInfo {
                    id: WindowId(id),
                    title: format!("window {id}"),
                    app_id: String::new(),
                    width: 64,
                    height: 64,
                    focused: false,
                    owner: None,
                    kind: windowcast_protocol::WindowKind::Normal,
                    position: None,
                    content: windowcast_protocol::ContentHint::General,
                })
                .collect()
        }
        fn encoders(&self) -> Vec<VideoCodec> {
            vec![VideoCodec::H264]
        }
        fn open(&self, _: WindowId, _: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
            Err("no frames here".into())
        }
        fn open_pictures(&self, _: WindowId) -> Option<Result<Box<dyn PictureSource>, String>> {
            None
        }
    }

    #[test]
    fn moonlight_sees_only_the_windows_in_compatibility_mode() {
        let apps = CompatApps {
            windows: WindowApps(Arc::new(Two)),
            shown: RwLock::default(),
        };
        assert!(apps.apps().is_empty());
        apps.shown
            .write()
            .unwrap()
            .insert(WindowId(2), "window 2".into());
        let listed = apps.apps();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, app_id(WindowId(2)));
        assert!(!apps.shows(app_id(WindowId(1))));
    }
}

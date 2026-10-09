//! The host role: the platform's agent served by host-core, with the
//! settings the host window changes applied live, and a snapshot of the
//! host (PIN, clients, windows with their content hints, live streams)
//! refreshed once a second for the window to show.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
use windowcast_host::{FrameSource, HostConfig, HostControl, WindowSource};
use windowcast_protocol::selection;
use windowcast_protocol::{BackendKind, InputEvent, VideoCodec, WindowId, WindowInfo};

use crate::config::{HostSettings, Store};
use crate::platform::{self, Agent};

/// The agent, with input and clipboard sharing switched by the host's
/// settings.
struct Gated {
    agent: Agent,
    input: AtomicBool,
    clipboard: AtomicBool,
    microphone: Arc<AtomicBool>,
}

/// A virtual microphone that stays silent while the host does not allow
/// clients' microphones.
struct GatedMicrophone {
    sink: Box<dyn windowcast_host::audio::MicrophoneSink>,
    allowed: Arc<AtomicBool>,
}

impl windowcast_host::audio::MicrophoneSink for GatedMicrophone {
    fn play(&mut self, samples: &[i16]) {
        if self.allowed.load(Ordering::SeqCst) {
            self.sink.play(samples);
        }
    }
}

impl WindowSource for Gated {
    fn list_windows(&self) -> Vec<WindowInfo> {
        self.agent.list_windows()
    }

    fn encoders(&self) -> Vec<VideoCodec> {
        self.agent.encoders()
    }

    fn backends(&self) -> Vec<BackendKind> {
        self.agent.backends()
    }

    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        self.agent.open(window, codec)
    }

    fn open_desktop(
        &self,
        window: WindowId,
        codec: VideoCodec,
    ) -> Result<Box<dyn FrameSource>, String> {
        self.agent.open_desktop(window, codec)
    }

    fn input(&self, event: &InputEvent, focus: Option<WindowId>) {
        if self.input.load(Ordering::SeqCst) {
            self.agent.input(event, focus);
        }
    }

    fn clipboard(&self) -> Option<(u64, String)> {
        // While sharing is off the host reports a clipboard that never
        // changes, so turning it on mid-session shares from then on.
        if self.clipboard.load(Ordering::SeqCst) {
            self.agent.clipboard()
        } else {
            Some((0, String::new()))
        }
    }

    fn set_clipboard(&self, text: &str) {
        if self.clipboard.load(Ordering::SeqCst) {
            self.agent.set_clipboard(text);
        }
    }

    fn open_audio(
        &self,
        window: WindowId,
    ) -> Option<Result<Box<dyn windowcast_host::audio::AudioSource>, String>> {
        self.agent.open_audio(window)
    }

    fn microphone(
        &self,
    ) -> Option<Result<Box<dyn windowcast_host::audio::MicrophoneSink>, String>> {
        if !self.microphone.load(Ordering::SeqCst) {
            return Some(Err("this host does not take clients' microphones".into()));
        }
        Some(self.agent.microphone()?.map(|sink| {
            Box::new(GatedMicrophone {
                sink,
                allowed: Arc::clone(&self.microphone),
            }) as _
        }))
    }
}

/// One of the host's windows, as the host window lists it.
#[derive(Clone)]
pub struct HostWindow {
    pub info: WindowInfo,
    /// What the default rules ask for, by the window's content.
    pub default_backend: BackendKind,
    /// What this host would serve for that.
    pub served: BackendKind,
}

#[derive(Clone)]
pub struct HostStream {
    pub client: String,
    pub title: String,
    pub requested: BackendKind,
    pub backend: BackendKind,
    pub codec: VideoCodec,
    pub encoder: String,
    pub size: Option<(u32, u32)>,
    pub fps: f64,
    pub mbps: f64,
    pub keyframes: u64,
    pub keyframe_requests: u64,
    /// Opus packets sent, when the stream has sound.
    pub audio_packets: Option<u64>,
    pub seconds: u64,
}

#[derive(Clone)]
pub struct HostClient {
    pub peer: String,
    pub address: String,
    pub seconds: u64,
}

/// Everything the host window shows.
#[derive(Clone, Default)]
pub struct HostSnapshot {
    pub pin: Option<String>,
    pub identity: String,
    pub offered: Vec<VideoCodec>,
    pub windows: Vec<HostWindow>,
    pub streams: Vec<HostStream>,
    pub clients: Vec<HostClient>,
    pub trusted: Vec<String>,
}

/// Counters at the last rate sample of one stream.
struct Sample {
    at: Instant,
    frames: u64,
    bytes: u64,
    fps: f64,
    mbps: f64,
}

pub struct HostRole {
    runtime: Handle,
    control: Arc<HostControl>,
    source: Arc<Gated>,
    store: Arc<Store>,
    pub listen: String,
    /// Each encoder option and the codecs it has on this machine.
    pub encoders: Vec<(&'static str, Vec<VideoCodec>)>,
    samples: Mutex<HashMap<(String, u64, Instant), Sample>>,
    snapshot: Mutex<HostSnapshot>,
}

impl HostRole {
    /// Starts the host on `runtime`: opens the agent with the saved
    /// settings and serves on `listen`. Its snapshot is refreshed once a
    /// second from then on.
    pub fn start(
        runtime: Handle,
        store: Arc<Store>,
        data_dir: &std::path::Path,
        listen: String,
    ) -> Result<Arc<Self>, String> {
        let settings = store.get().host;
        let agent = platform::open_agent(&settings)?;
        let encoders = platform::encoders();
        let config = HostConfig {
            listen: listen.clone(),
            pairing: settings.pairing,
            data_dir: data_dir.join("host"),
        };
        let control = HostControl::open(&config).map_err(|e| e.to_string())?;
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind(&listen))
            .map_err(|e| format!("cannot listen on {listen}: {e}"))?;
        println!("host listening on {listen}");
        let source = Arc::new(Gated {
            agent,
            input: AtomicBool::new(settings.input),
            clipboard: AtomicBool::new(settings.clipboard),
            microphone: Arc::new(AtomicBool::new(settings.microphone)),
        });
        runtime.spawn(windowcast_host::serve_with(
            listener,
            Arc::clone(&control),
            Arc::clone(&source) as Arc<dyn WindowSource>,
        ));
        let role = Arc::new(HostRole {
            runtime,
            control,
            source,
            store,
            listen,
            encoders,
            samples: Mutex::new(HashMap::new()),
            snapshot: Mutex::new(HostSnapshot::default()),
        });
        let refresher = Arc::clone(&role);
        std::thread::spawn(move || loop {
            let snapshot = refresher.take_snapshot();
            *refresher.snapshot.lock().expect("snapshot") = snapshot;
            std::thread::sleep(Duration::from_secs(1));
        });
        Ok(role)
    }

    pub fn snapshot(&self) -> HostSnapshot {
        self.snapshot.lock().expect("snapshot").clone()
    }

    pub fn settings(&self) -> HostSettings {
        self.store.get().host
    }

    fn take_snapshot(&self) -> HostSnapshot {
        let windows = self.source.list_windows();
        let available = self.source.backends();
        let titles: HashMap<u64, String> = windows
            .iter()
            .map(|window| (window.id.0, window.title.clone()))
            .collect();
        let windows = windows
            .into_iter()
            .map(|info| {
                let default_backend = selection::choose_backend(&info, &[]);
                HostWindow {
                    served: selection::serve(default_backend, &available),
                    default_backend,
                    info,
                }
            })
            .collect();
        let clients = self
            .control
            .clients()
            .iter()
            .map(|client| HostClient {
                peer: client.peer.to_hex(),
                address: client.address.clone(),
                seconds: client.since.elapsed().as_secs(),
            })
            .collect();
        let (pin, trusted) = self
            .runtime
            .block_on(async { (self.control.pin().await, self.control.trusted().await) });
        HostSnapshot {
            pin,
            identity: self.control.peer_id().to_hex(),
            offered: self.source.encoders(),
            windows,
            streams: self.streams(&titles),
            clients,
            trusted: trusted.iter().map(|peer| peer.to_hex()).collect(),
        }
    }

    /// One row per live stream, with its rates since the last snapshot.
    fn streams(&self, titles: &HashMap<u64, String>) -> Vec<HostStream> {
        let streams = self.control.streams();
        let mut samples = self.samples.lock().expect("samples");
        let now = Instant::now();
        let mut live = Vec::new();
        let rows = streams
            .iter()
            .map(|stream| {
                let key = (stream.peer.to_hex(), stream.window.0, stream.since);
                live.push(key.clone());
                let sample = samples.entry(key).or_insert(Sample {
                    at: stream.since,
                    frames: 0,
                    bytes: 0,
                    fps: 0.0,
                    mbps: 0.0,
                });
                let elapsed = now.duration_since(sample.at).as_secs_f64();
                if elapsed >= 0.9 {
                    sample.fps =
                        (stream.frames - sample.frames.min(stream.frames)) as f64 / elapsed;
                    sample.mbps = (stream.bytes - sample.bytes.min(stream.bytes)) as f64 * 8.0
                        / 1_000_000.0
                        / elapsed;
                    sample.at = now;
                    sample.frames = stream.frames;
                    sample.bytes = stream.bytes;
                }
                HostStream {
                    client: stream.peer.to_hex(),
                    title: titles
                        .get(&stream.window.0)
                        .cloned()
                        .unwrap_or_else(|| format!("window {}", stream.window.0)),
                    requested: stream.requested,
                    backend: stream.backend,
                    codec: stream.codec,
                    encoder: stream.encoder.clone(),
                    size: stream.size,
                    fps: sample.fps,
                    mbps: sample.mbps,
                    keyframes: stream.keyframes,
                    keyframe_requests: stream.keyframe_requests,
                    audio_packets: stream.audio_packets,
                    seconds: stream.since.elapsed().as_secs(),
                }
            })
            .collect();
        samples.retain(|key, _| live.contains(key));
        rows
    }

    /// Applies new settings: the encoder, codec, frame rate and bitrate go
    /// to the agent (running streams follow at their next picture), input
    /// and clipboard sharing switch at once.
    pub fn apply(&self, settings: HostSettings) -> Result<(), String> {
        platform::configure(&self.source.agent, &settings)?;
        self.source.input.store(settings.input, Ordering::SeqCst);
        self.source
            .clipboard
            .store(settings.clipboard, Ordering::SeqCst);
        self.source
            .microphone
            .store(settings.microphone, Ordering::SeqCst);
        self.store.update(|config| config.host = settings);
        Ok(())
    }

    pub fn pairing(&self, open: bool) {
        self.runtime.block_on(async {
            if open {
                self.control.open_pairing().await;
            } else {
                self.control.close_pairing().await;
            }
        });
    }

    pub fn forget(&self, peer: &str) -> Result<(), String> {
        let peer = windowcast_identity::PeerId::from_hex(peer).map_err(|e| e.to_string())?;
        self.runtime
            .block_on(self.control.forget(&peer))
            .map_err(|e| e.to_string())
    }
}

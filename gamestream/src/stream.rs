//! The host's GameStream session, after `/launch`: RTSP to agree on the
//! stream (Sunshine's `rtsp.cpp`: OPTIONS, DESCRIBE, SETUP for audio, video
//! and control, ANNOUNCE with the client's stream settings, PLAY), then the
//! video packets to the address the client's pings come from, and the
//! encrypted control stream for keyframe requests, input (handed to the
//! app's [`InputSink`]) and the end. The app's sound goes as Opus to where
//! the client's audio pings come from ([`crate::audio`]).

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;

use crate::audio::{self, AudioKey, AudioPacketizer};
use crate::client::App;
use crate::control::{self, Control, ControlEvent};
use crate::input::{self, Input};
use crate::rtsp::{self, Message};
use crate::video::Packetizer;

pub const VIDEO_PORT: u16 = 47998;
pub const AUDIO_PORT: u16 = 48000;

/// What a client asked for in its ANNOUNCE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// The NV header and payload of each video packet.
    pub packet_size: usize,
    /// 0 H.264, 1 HEVC, 2 AV1.
    pub codec: u32,
    /// Milliseconds of sound in each audio packet (5 or 10).
    pub audio_ms: u32,
    /// Whether the client wants its audio encrypted.
    pub audio_encrypted: bool,
}

/// One encoded frame for a GameStream client.
pub struct VideoFrame {
    pub data: Vec<u8>,
    pub idr: bool,
}

/// A running capture and encoder for one app.
pub trait VideoSource: Send {
    /// Blocks until the next frame; a keyframe when asked. `None` ends the
    /// stream.
    fn next_frame(&mut self, keyframe: bool) -> Option<VideoFrame>;
}

/// Where a stream's input goes, in the order it came.
pub trait InputSink: Send {
    fn input(&mut self, event: Input);
}

/// What a host offers over GameStream.
pub trait Apps: Send + Sync {
    fn apps(&self) -> Vec<App>;
    fn open(&self, app: u32, config: &StreamConfig) -> Result<Box<dyn VideoSource>, String>;
    /// Where the client's input for `app` goes; `None` drops it.
    fn input(&self, _app: u32, _config: &StreamConfig) -> Option<Box<dyn InputSink>> {
        None
    }
    /// The app's sound, if it has any.
    fn audio(&self, _app: u32) -> Option<Box<dyn windowcast_host::audio::AudioSource>> {
        None
    }
}

/// A launched app, waiting for or in its stream.
pub struct Launch {
    pub app: u32,
    pub key: [u8; 16],
    pub key_id: u32,
    /// The client's requested mode.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Echoed by the client in its UDP pings (Sunshine's ping payload).
    pub ping_payload: String,
    /// Echoed by the client in its ENet connect.
    pub connect_data: u32,
    /// Set to end the stream.
    pub stop: Arc<AtomicBool>,
    streaming: AtomicBool,
}

impl Launch {
    pub fn new(app: u32, key: [u8; 16], key_id: u32, mode: (u32, u32, u32)) -> Arc<Self> {
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let random: [u8; 16] = crate::crypto::random();
        let ping_payload = random
            .iter()
            .map(|b| alphabet[usize::from(*b) % alphabet.len()] as char)
            .collect();
        Arc::new(Launch {
            app,
            key,
            key_id,
            width: mode.0,
            height: mode.1,
            fps: mode.2,
            ping_payload,
            connect_data: u32::from_le_bytes(crate::crypto::random()),
            stop: Arc::new(AtomicBool::new(false)),
            streaming: AtomicBool::new(false),
        })
    }
}

/// The DESCRIBE answer: Sunshine's feature and encryption attributes.
/// windowcast's host encodes H.264 only, takes touch and pen input, and
/// supports control and audio encryption (asking for the control one).
///
/// Its sound is stereo. A client set to 5.1 or 7.1 is told the stream is
/// one coupled Opus stream mapped onto every speaker, left and right
/// alternating, so the stereo packets decode as they are.
fn describe() -> Vec<u8> {
    format!(
        "a=x-ss-general.featureFlags:{}\n\
         a=x-ss-general.encryptionSupported:{}\n\
         a=x-ss-general.encryptionRequested:1\n\
         a=fmtp:97 surround-params=611010101\n\
         a=fmtp:97 surround-params=81101010101\n",
        input::FEATURE_PEN_TOUCH,
        1 | audio::SS_AUDIO_ENCRYPTION,
    )
    .into_bytes()
}

/// Serves one RTSP connection of the current launch.
pub async fn serve_rtsp(
    mut stream: TcpStream,
    launch: Option<Arc<Launch>>,
    apps: Arc<dyn Apps>,
) -> Result<(), crate::GameStreamError> {
    let request = rtsp::read_request(&mut stream).await?;
    let cseq = request.option("CSeq").map(str::to_owned);
    let Some(launch) = launch else {
        return rtsp::respond(
            &mut stream,
            &Message::response(503, "Service Unavailable", cseq.as_deref()),
        )
        .await;
    };
    let ok = || Message::response(200, "OK", cseq.as_deref());
    let response = match request.command.as_str() {
        "OPTIONS" | "PLAY" => ok(),
        "DESCRIBE" => {
            let mut response = ok();
            response.payload = describe();
            response
        }
        "SETUP" => {
            let kind = request
                .target
                .split_once('=')
                .map(|(_, rest)| rest.split('/').next().unwrap_or_default().to_owned())
                .unwrap_or_default();
            let port = match kind.as_str() {
                "audio" => Some(AUDIO_PORT),
                "video" => Some(VIDEO_PORT),
                "control" => Some(control::CONTROL_PORT),
                _ => None,
            };
            match port {
                None => Message::response(404, "NOT FOUND", cseq.as_deref()),
                Some(port) => {
                    let response = ok()
                        .with("Session", "DEADBEEFCAFE;timeout = 90")
                        .with("Transport", &format!("server_port={port}"));
                    if kind == "control" {
                        response.with("X-SS-Connect-Data", &launch.connect_data.to_string())
                    } else {
                        response.with("X-SS-Ping-Payload", &launch.ping_payload)
                    }
                }
            }
        }
        "ANNOUNCE" => match announce(&request.payload, &launch) {
            Some(config) if config.codec == 0 => {
                if !launch.streaming.swap(true, Ordering::SeqCst) {
                    let launch = Arc::clone(&launch);
                    let address = stream.peer_addr().ok();
                    std::thread::spawn(move || {
                        if let Err(e) = run(&launch, &config, apps.as_ref(), address) {
                            eprintln!("gamestream: the stream ended: {e}");
                        }
                        launch.streaming.store(false, Ordering::SeqCst);
                    });
                }
                ok()
            }
            Some(_) => Message::response(400, "BAD REQUEST", cseq.as_deref()),
            None => Message::response(400, "BAD REQUEST", cseq.as_deref()),
        },
        _ => Message::response(404, "NOT FOUND", cseq.as_deref()),
    };
    rtsp::respond(&mut stream, &response).await
}

/// The stream settings in a client's ANNOUNCE.
pub fn announce(payload: &[u8], launch: &Launch) -> Option<StreamConfig> {
    let attributes = rtsp::sdp_attributes(payload);
    let number = |name: &str| {
        attributes
            .get(name)
            .and_then(|v| v.trim().parse::<u32>().ok())
    };
    Some(StreamConfig {
        width: number("x-nv-video[0].clientViewportWd").unwrap_or(launch.width),
        height: number("x-nv-video[0].clientViewportHt").unwrap_or(launch.height),
        fps: number("x-nv-video[0].maxFPS").unwrap_or(launch.fps),
        bitrate_kbps: number("x-nv-vqos[0].bw.maximumBitrateKbps").unwrap_or(10_000),
        packet_size: number("x-nv-video[0].packetSize")? as usize,
        codec: number("x-nv-vqos[0].bitStreamFormat").unwrap_or(0),
        audio_ms: match number("x-nv-aqos.packetDuration") {
            Some(10) => 10,
            _ => 5,
        },
        audio_encrypted: number("x-nv-general.featureFlags").unwrap_or(0)
            & audio::NV_AUDIO_ENCRYPTION
            != 0
            || number("x-ss-general.encryptionEnabled").unwrap_or(0) & audio::SS_AUDIO_ENCRYPTION
                != 0,
    })
}

/// Streams one launch until the client goes or the launch is stopped.
fn run(
    launch: &Launch,
    config: &StreamConfig,
    apps: &dyn Apps,
    rtsp_peer: Option<SocketAddr>,
) -> Result<(), String> {
    let any = |port: u16| SocketAddr::from(([0, 0, 0, 0], port));
    let video = UdpSocket::bind(any(VIDEO_PORT)).map_err(|e| format!("video port: {e}"))?;
    let audio = UdpSocket::bind(any(AUDIO_PORT)).map_err(|e| format!("audio port: {e}"))?;
    let key = config.audio_encrypted.then_some(AudioKey {
        key: launch.key,
        key_id: launch.key_id,
    });
    let sound = apps.audio(launch.app);
    let audio_thread = {
        let (stop, ping) = (Arc::clone(&launch.stop), launch.ping_payload.clone());
        let duration = config.audio_ms;
        std::thread::spawn(move || send_audio(audio, sound, duration, key, &ping, rtsp_peer, &stop))
    };
    let control = Control::listen(any(control::CONTROL_PORT), launch.key, launch.connect_data)
        .map_err(|e| format!("control port: {e}"))?;

    // The client's video pings say where to send.
    video
        .set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|e| e.to_string())?;
    let started = Instant::now();
    let mut buf = [0u8; 256];
    let peer = loop {
        if launch.stop.load(Ordering::SeqCst) || started.elapsed() > Duration::from_secs(15) {
            return Err("the client never pinged the video port".into());
        }
        if let Ok((n, from)) = video.recv_from(&mut buf) {
            let ping = &buf[..n];
            let ours = ping.len() >= 16 && &ping[..16] == launch.ping_payload.as_bytes();
            let legacy = ping == b"PING";
            if (ours || legacy) && rtsp_peer.is_none_or(|p| p.ip() == from.ip()) {
                break from;
            }
        }
    };
    println!(
        "gamestream: streaming app {} to {peer} ({}x{} at {} fps)",
        launch.app, config.width, config.height, config.fps
    );
    let mut source = apps.open(launch.app, config)?;

    // The control stream on its own thread, so input is delivered as it
    // comes rather than between frames.
    let wants_keyframe = Arc::new(AtomicBool::new(true));
    let left = Arc::new(AtomicBool::new(false));
    let mut sink = apps.input(launch.app, config);
    let events = control.events;
    let listener = {
        let (wants_keyframe, left) = (Arc::clone(&wants_keyframe), Arc::clone(&left));
        let stop = Arc::clone(&launch.stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) && !left.load(Ordering::SeqCst) {
                let event = match events.recv_timeout(Duration::from_millis(100)) {
                    Ok(event) => event,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                };
                match event {
                    ControlEvent::Message(control::REQUEST_IDR, _)
                    | ControlEvent::Message(control::INVALIDATE_REFS, _) => {
                        wants_keyframe.store(true, Ordering::SeqCst)
                    }
                    ControlEvent::Message(control::INPUT, payload) => {
                        if let Some(sink) = sink.as_mut() {
                            for event in input::parse(&payload) {
                                sink.input(event);
                            }
                        }
                    }
                    ControlEvent::Disconnected => break,
                    _ => {}
                }
            }
            left.store(true, Ordering::SeqCst);
        })
    };
    let control = control.sender;

    let mut packetizer = Packetizer::new(config.packet_size);
    let epoch = Instant::now();
    let mut sent = 0u64;
    loop {
        if launch.stop.load(Ordering::SeqCst) {
            control.send(
                control::TERMINATION,
                &control::TERMINATION_GRACEFUL.to_be_bytes(),
                control::CHANNEL_GENERIC,
                true,
            );
            std::thread::sleep(Duration::from_millis(200));
            let _ = listener.join();
            let _ = audio_thread.join();
            return Ok(());
        }
        if left.load(Ordering::SeqCst) {
            println!("gamestream: the client left after {sent} frames");
            return Ok(());
        }
        let keyframe = wants_keyframe.swap(false, Ordering::SeqCst);
        let Some(frame) = source.next_frame(keyframe) else {
            left.store(true, Ordering::SeqCst);
            return Err("the app's picture ended".into());
        };
        let timestamp = (epoch.elapsed().as_micros() * 9 / 100) as u32;
        for packet in packetizer.packets(&frame.data, frame.idr, timestamp) {
            let _ = video.send_to(&packet, peer);
        }
        sent += 1;
    }
}

/// Waits for the client's audio pings, then sends the app's sound there
/// until the stream stops. Without sound it only takes the pings.
fn send_audio(
    socket: UdpSocket,
    sound: Option<Box<dyn windowcast_host::audio::AudioSource>>,
    duration_ms: u32,
    key: Option<AudioKey>,
    ping_payload: &str,
    rtsp_peer: Option<SocketAddr>,
    stop: &AtomicBool,
) {
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .ok();
    let mut buf = [0u8; 256];
    let peer = loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if let Ok((n, from)) = socket.recv_from(&mut buf) {
            let ping = &buf[..n];
            let ours = ping.len() >= 16 && &ping[..16] == ping_payload.as_bytes();
            if (ours || ping == b"PING") && rtsp_peer.is_none_or(|p| p.ip() == from.ip()) {
                break from;
            }
        }
    };
    let Some(mut sound) = sound else {
        while !stop.load(Ordering::SeqCst) {
            let _ = socket.recv_from(&mut buf);
        }
        return;
    };
    // A constant rate, so each block's packets are one size for the parity.
    let encoder = windowcast_host::audio::OpusPackets::with_duration(duration_ms)
        .and_then(|mut encoder| encoder.constant_rate().map(|()| encoder));
    let mut encoder = match encoder {
        Ok(encoder) => encoder,
        Err(e) => {
            eprintln!("gamestream: no sound: {e}");
            return;
        }
    };
    let mut packetizer = AudioPacketizer::new(duration_ms, key);
    while !stop.load(Ordering::SeqCst) {
        let Some(samples) = sound.next_samples() else {
            return;
        };
        match encoder.push(&samples) {
            Ok(packets) => {
                for opus in packets {
                    for packet in packetizer.packets(&opus) {
                        let _ = socket.send_to(&packet, peer);
                    }
                }
            }
            Err(e) => {
                eprintln!("gamestream: the sound stopped: {e}");
                return;
            }
        }
    }
}

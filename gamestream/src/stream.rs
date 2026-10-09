//! The host's GameStream session, after `/launch`: RTSP to agree on the
//! stream (Sunshine's `rtsp.cpp`: OPTIONS, DESCRIBE, SETUP for audio, video
//! and control, ANNOUNCE with the client's stream settings, PLAY), then the
//! video packets to the address the client's pings come from, and the
//! encrypted control stream for keyframe requests, input and the end.
//!
//! Sound over GameStream is not sent yet: the audio port takes the
//! client's pings and stays quiet, which Moonlight accepts.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;

use crate::client::App;
use crate::control::{self, Control, ControlEvent};
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

/// What a host offers over GameStream.
pub trait Apps: Send + Sync {
    fn apps(&self) -> Vec<App>;
    fn open(&self, app: u32, config: &StreamConfig) -> Result<Box<dyn VideoSource>, String>;
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
/// windowcast's host encodes H.264 only, asks for (and supports) only the
/// control stream encryption, and sends stereo.
fn describe() -> Vec<u8> {
    concat!(
        "a=x-ss-general.featureFlags:0\n",
        "a=x-ss-general.encryptionSupported:1\n",
        "a=x-ss-general.encryptionRequested:1\n",
    )
    .as_bytes()
    .to_vec()
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
    // The client pings the audio port too; nothing is sent there yet.
    let audio = UdpSocket::bind(any(AUDIO_PORT)).map_err(|e| format!("audio port: {e}"))?;
    audio
        .set_read_timeout(Some(Duration::from_millis(100)))
        .ok();
    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        loop {
            if let Err(e) = audio.recv_from(&mut buf) {
                if !matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) {
                    return;
                }
            }
        }
    });
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
    let mut packetizer = Packetizer::new(config.packet_size);
    let epoch = Instant::now();
    let mut keyframe = true;
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
            return Ok(());
        }
        while let Ok(event) = control.events.try_recv() {
            match event {
                ControlEvent::Message(control::REQUEST_IDR, _)
                | ControlEvent::Message(control::INVALIDATE_REFS, _) => keyframe = true,
                ControlEvent::Disconnected => {
                    println!("gamestream: the client left after {sent} frames");
                    return Ok(());
                }
                _ => {}
            }
        }
        let Some(frame) = source.next_frame(keyframe) else {
            return Err("the app's picture ended".into());
        };
        keyframe = false;
        let timestamp = (epoch.elapsed().as_micros() * 9 / 100) as u32;
        for packet in packetizer.packets(&frame.data, frame.idr, timestamp) {
            let _ = video.send_to(&packet, peer);
        }
        sent += 1;
    }
}

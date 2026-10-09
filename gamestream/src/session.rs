//! A GameStream client's stream, after `/launch`, as moonlight-common-c
//! sets one up (`RtspConnection.c` `performRtspHandshake`, `SdpGenerator.c`,
//! `VideoStream.c`, `ControlStream.c`): RTSP OPTIONS, DESCRIBE, SETUP for
//! audio, video and control (their ports, the ping payload and the ENet
//! connect data), ANNOUNCE with our stream settings, PLAY; then pings to
//! the video port so the host knows where to send, the video packets put
//! back together into frames, and the encrypted ENet control stream for
//! keyframe requests and the host's end of the stream.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::control::{self, Control, ControlEvent};
use crate::rtsp::{self, Message};
use crate::video::{Depacketizer, Frame, Received};
use crate::GameStreamError;

/// What our client asks a host for.
#[derive(Debug, Clone)]
pub struct StreamRequest {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// The NV header and payload of each video packet (Moonlight's default
    /// is 1392 on a LAN, 1024 remote).
    pub packet_size: u32,
}

impl Default for StreamRequest {
    fn default() -> Self {
        StreamRequest {
            width: 1280,
            height: 720,
            fps: 60,
            bitrate_kbps: 10_000,
            packet_size: 1392,
        }
    }
}

/// A running stream: frames as they complete, and the end.
pub struct Stream {
    pub frames: Receiver<Frame>,
    stop: Arc<AtomicBool>,
    pub ended: Arc<AtomicBool>,
}

impl Stream {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop();
    }
}

fn rtsp_address(session_url: &str, fallback: IpAddr) -> Result<SocketAddr, GameStreamError> {
    let rest = session_url
        .strip_prefix("rtsp://")
        .ok_or(GameStreamError::Rtsp(
            "only plain rtsp:// sessions are spoken here",
        ))?;
    let rest = rest.trim_end_matches('/');
    if let Ok(address) = rest.parse::<SocketAddr>() {
        return Ok(address);
    }
    let port = rest
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .unwrap_or(rtsp::RTSP_PORT);
    Ok(SocketAddr::new(fallback, port))
}

fn port_of(response: &Message, fallback: u16) -> u16 {
    response
        .option("Transport")
        .and_then(|t| t.split("server_port=").nth(1))
        .and_then(|p| {
            p.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .ok()
        })
        .unwrap_or(fallback)
}

/// Sets up the stream of a launched app (`session_url` from `/launch`) and
/// starts receiving it. `key` and `key_id` are the launch's input key.
pub async fn start(
    host: IpAddr,
    session_url: &str,
    key: [u8; 16],
    request: &StreamRequest,
) -> Result<Stream, GameStreamError> {
    let address = rtsp_address(session_url, host)?;
    let target = format!("rtsp://{}:{}", address.ip(), address.port());
    let mut cseq = 0;
    let mut next = |command: &str, target: &str| {
        cseq += 1;
        Message::request(command, target)
            .with("CSeq", &cseq.to_string())
            .with("X-GS-ClientVersion", "14")
            .with("Host", &address.ip().to_string())
    };

    let options = rtsp::transact(address, &next("OPTIONS", &target)).await?;
    if options.status != 200 {
        return Err(GameStreamError::Rtsp("OPTIONS refused"));
    }
    let describe = rtsp::transact(
        address,
        &next("DESCRIBE", &target)
            .with("Accept", "application/sdp")
            .with("If-Modified-Since", "Thu, 01 Jan 1970 00:00:00 GMT"),
    )
    .await?;
    if describe.status != 200 {
        return Err(GameStreamError::Rtsp("DESCRIBE refused"));
    }
    let attributes = rtsp::sdp_attributes(&describe.payload);
    let flag = |name: &str| {
        attributes
            .get(name)
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0)
    };
    let (supported, requested) = (
        flag("x-ss-general.encryptionSupported"),
        flag("x-ss-general.encryptionRequested"),
    );
    if requested & 0x06 != 0 {
        return Err(GameStreamError::Rtsp(
            "the host requires encrypted video or audio, which is not built here yet",
        ));
    }
    let encryption = supported & 0x01;

    let setup =
        |next: &mut dyn FnMut(&str, &str) -> Message, target: &str, session: Option<&str>| {
            let mut message = next("SETUP", target)
                .with("Transport", "unicast;X-GS-ClientPort=50000-50001")
                .with("If-Modified-Since", "Thu, 01 Jan 1970 00:00:00 GMT");
            if let Some(session) = session {
                message = message.with("Session", session);
            }
            message
        };
    let audio = rtsp::transact(address, &setup(&mut next, "streamid=audio/0/0", None)).await?;
    if audio.status != 200 {
        return Err(GameStreamError::Rtsp("SETUP audio refused"));
    }
    let session = audio
        .option("Session")
        .and_then(|s| s.split(';').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(GameStreamError::Rtsp("no session in SETUP"))?
        .to_owned();
    let audio_port = port_of(&audio, crate::stream::AUDIO_PORT);
    let video = rtsp::transact(
        address,
        &setup(&mut next, "streamid=video/0/0", Some(&session)),
    )
    .await?;
    if video.status != 200 {
        return Err(GameStreamError::Rtsp("SETUP video refused"));
    }
    let video_port = port_of(&video, crate::stream::VIDEO_PORT);
    let ping_payload = video
        .option("X-SS-Ping-Payload")
        .filter(|p| p.len() == 16)
        .map(|p| p.as_bytes().to_vec());
    let control_setup = rtsp::transact(
        address,
        &setup(&mut next, "streamid=control/13/0", Some(&session)),
    )
    .await?;
    if control_setup.status != 200 {
        return Err(GameStreamError::Rtsp("SETUP control refused"));
    }
    let control_port = port_of(&control_setup, control::CONTROL_PORT);
    let connect_data: u32 = control_setup
        .option("X-SS-Connect-Data")
        .and_then(|c| c.trim().parse().ok())
        .unwrap_or(0);

    let sdp = format!(
        concat!(
            "v=0\r\n",
            "o=android 0 14 IN {family} {ip}\r\n",
            "s=NVIDIA Streaming Client\r\n",
            "a=x-ml-general.featureFlags:0\r\n",
            "a=x-ss-general.encryptionEnabled:{encryption}\r\n",
            "a=x-ss-video[0].chromaSamplingType:0\r\n",
            "a=x-nv-video[0].clientViewportWd:{width}\r\n",
            "a=x-nv-video[0].clientViewportHt:{height}\r\n",
            "a=x-nv-video[0].maxFPS:{fps}\r\n",
            "a=x-nv-video[0].packetSize:{packet_size}\r\n",
            "a=x-nv-video[0].rateControlMode:4\r\n",
            "a=x-nv-video[0].timeoutLengthMs:7000\r\n",
            "a=x-nv-video[0].framesWithInvalidRefThreshold:0\r\n",
            "a=x-nv-video[0].initialBitrateKbps:{bitrate}\r\n",
            "a=x-nv-video[0].initialPeakBitrateKbps:{bitrate}\r\n",
            "a=x-nv-vqos[0].bw.minimumBitrateKbps:{bitrate}\r\n",
            "a=x-nv-vqos[0].bw.maximumBitrateKbps:{bitrate}\r\n",
            "a=x-ml-video.configuredBitrateKbps:{bitrate}\r\n",
            "a=x-nv-vqos[0].fec.enable:1\r\n",
            "a=x-nv-vqos[0].videoQualityScoreUpdateTime:5000\r\n",
            "a=x-nv-vqos[0].qosTrafficType:5\r\n",
            "a=x-nv-aqos.qosTrafficType:4\r\n",
            "a=x-nv-general.featureFlags:135\r\n",
            "a=x-nv-general.useReliableUdp:13\r\n",
            "a=x-nv-vqos[0].fec.minRequiredFecPackets:2\r\n",
            "a=x-nv-vqos[0].bllFec.enable:0\r\n",
            "a=x-nv-vqos[0].drc.enable:0\r\n",
            "a=x-nv-general.enableRecoveryMode:0\r\n",
            "a=x-nv-video[0].videoEncoderSlicesPerFrame:1\r\n",
            "a=x-nv-clientSupportHevc:0\r\n",
            "a=x-nv-vqos[0].bitStreamFormat:0\r\n",
            "a=x-nv-video[0].dynamicRangeMode:0\r\n",
            "a=x-nv-video[0].maxNumReferenceFrames:1\r\n",
            "a=x-nv-video[0].clientRefreshRateX100:0\r\n",
            "a=x-nv-audio.surround.numChannels:2\r\n",
            "a=x-nv-audio.surround.channelMask:3\r\n",
            "a=x-nv-audio.surround.enable:0\r\n",
            "a=x-nv-audio.surround.AudioQuality:0\r\n",
            "a=x-nv-aqos.packetDuration:5\r\n",
            "a=x-nv-video[0].encoderCscMode:0\r\n",
            "t=0 0\r\n",
            "m=video {video_port}  \r\n",
        ),
        family = if address.is_ipv4() { "IPv4" } else { "IPv6" },
        ip = address.ip(),
        encryption = encryption,
        width = request.width,
        height = request.height,
        fps = request.fps,
        packet_size = request.packet_size,
        bitrate = request.bitrate_kbps,
        video_port = video_port,
    );
    let mut announce = next("ANNOUNCE", "streamid=control/13/0")
        .with("Session", &session)
        .with("Content-type", "application/sdp")
        .with("Content-length", &sdp.len().to_string());
    announce.payload = sdp.into_bytes();
    let announced = rtsp::transact(address, &announce).await?;
    if announced.status != 200 {
        return Err(GameStreamError::Rtsp("ANNOUNCE refused"));
    }
    let play = rtsp::transact(address, &next("PLAY", "/").with("Session", &session)).await?;
    if play.status != 200 {
        return Err(GameStreamError::Rtsp("PLAY refused"));
    }

    let host = address.ip();
    let control = Control::connect(SocketAddr::new(host, control_port), key, connect_data)?;
    let video = UdpSocket::bind(if host.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })?;
    let audio = UdpSocket::bind(if host.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })?;
    let stop = Arc::new(AtomicBool::new(false));
    let ended = Arc::new(AtomicBool::new(false));
    let (frames_tx, frames) = mpsc::sync_channel(8);

    // Pings to the video and audio ports, so the host knows where to send.
    {
        let (video, audio, stop) = (video.try_clone()?, audio.try_clone()?, Arc::clone(&stop));
        let (video_to, audio_to) = (
            SocketAddr::new(host, video_port),
            SocketAddr::new(host, audio_port),
        );
        std::thread::spawn(move || {
            let mut count = 0u32;
            while !stop.load(Ordering::SeqCst) {
                count += 1;
                let ping = match &ping_payload {
                    Some(payload) => {
                        let mut ping = payload.clone();
                        ping.extend_from_slice(&count.to_be_bytes());
                        ping
                    }
                    None => b"PING".to_vec(),
                };
                let _ = video.send_to(&ping, video_to);
                let _ = audio.send_to(&ping, audio_to);
                std::thread::sleep(Duration::from_millis(500));
            }
        });
    }

    // The control stream: the start messages, periodic pings, keyframe
    // requests; the host's termination ends the stream.
    let wants_keyframe = Arc::new(AtomicBool::new(false));
    {
        let (stop, ended, wants_keyframe) = (
            Arc::clone(&stop),
            Arc::clone(&ended),
            Arc::clone(&wants_keyframe),
        );
        std::thread::spawn(move || {
            match control.events.recv_timeout(Duration::from_secs(10)) {
                Ok(ControlEvent::Connected) => {}
                _ => {
                    ended.store(true, Ordering::SeqCst);
                    return;
                }
            }
            control.send(control::REQUEST_IDR, &[0, 0], control::CHANNEL_URGENT, true);
            control.send(control::START_B, &[0], control::CHANNEL_GENERIC, true);
            let mut last_ping = Instant::now();
            while !stop.load(Ordering::SeqCst) {
                if wants_keyframe.swap(false, Ordering::SeqCst) {
                    control.send(control::REQUEST_IDR, &[0, 0], control::CHANNEL_URGENT, true);
                }
                if last_ping.elapsed() >= Duration::from_millis(100) {
                    control.send(
                        control::PERIODIC_PING,
                        &[4, 0, 0, 0, 0, 0],
                        control::CHANNEL_GENERIC,
                        true,
                    );
                    last_ping = Instant::now();
                }
                match control.events.recv_timeout(Duration::from_millis(20)) {
                    Ok(ControlEvent::Message(control::TERMINATION, _))
                    | Ok(ControlEvent::Disconnected) => break,
                    Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            ended.store(true, Ordering::SeqCst);
        });
    }

    // Video packets into frames.
    {
        let (stop, ended) = (Arc::clone(&stop), Arc::clone(&ended));
        video.set_read_timeout(Some(Duration::from_millis(100)))?;
        std::thread::spawn(move || {
            let mut depacketizer = Depacketizer::default();
            let mut buf = vec![0u8; 64 * 1024];
            while !stop.load(Ordering::SeqCst) && !ended.load(Ordering::SeqCst) {
                let Ok((n, from)) = video.recv_from(&mut buf) else {
                    continue;
                };
                if from.ip() != host {
                    continue;
                }
                for received in depacketizer.add(&buf[..n]) {
                    match received {
                        Received::Frame(frame) => {
                            if frames_tx.try_send(frame).is_err() {
                                // Nobody keeping up: start again from a keyframe.
                                wants_keyframe.store(true, Ordering::SeqCst);
                            }
                        }
                        Received::Lost(_) => wants_keyframe.store(true, Ordering::SeqCst),
                        Received::Pending => {}
                    }
                }
            }
        });
    }
    Ok(Stream {
        frames,
        stop,
        ended,
    })
}

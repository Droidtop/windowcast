//! A window's sound end to end: a process playing a 440 Hz tone is
//! captured by process loopback, cut into Opus packets, sent over a real
//! session beside a test-pattern picture, and decoded by the client back
//! into a 440 Hz tone. Needs an audio output device; skipped where there is
//! none. `WINDOWCAST_TONE_DEVICE` names a virtual output for the tone, to
//! keep the room quiet (see the tone's own documentation).
#![cfg(windows)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windowcast_agent_windows::audio::ProcessAudio;
use windowcast_cli_tools::testpattern::{TestPatternSource, WINDOW};
use windowcast_client::{AudioPoll, Client, Event};
use windowcast_host::audio::AudioSource;
use windowcast_host::{FrameSource, HostConfig, WindowSource};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{VideoCodec, WindowId, WindowInfo};

const WAIT: Duration = Duration::from_secs(20);

struct Tone(std::process::Child, u64);

impl Drop for Tone {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The tone process and its hidden window; `None` without an output device.
fn tone() -> Option<Tone> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_windowcast-test-tone"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let Some(hwnd) = line.trim().strip_prefix("window ") else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    Some(Tone(child, hwnd.parse().unwrap()))
}

/// Interleaved stereo samples' left channel: its RMS level and its
/// frequency from zero crossings.
fn level_and_pitch(samples: &[f32]) -> (f32, f32) {
    let left: Vec<f32> = samples.iter().step_by(2).copied().collect();
    let rms = (left.iter().map(|s| s * s).sum::<f32>() / left.len() as f32).sqrt();
    let crossings = left
        .windows(2)
        .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
        .count();
    (rms, crossings as f32 * 48_000.0 / left.len() as f32)
}

#[test]
fn process_loopback_hears_a_process() {
    let Some(tone) = tone() else {
        println!("skipped: no audio output device");
        return;
    };
    let mut capture = ProcessAudio::open(WindowId(tone.1)).unwrap();
    let mut samples = Vec::new();
    let deadline = Instant::now() + WAIT;
    while samples.len() < 48_000 * 2 {
        assert!(Instant::now() < deadline, "no sound captured");
        samples.extend(capture.next_samples().unwrap());
    }
    let floats: Vec<f32> = samples[24_000..]
        .iter()
        .map(|s| f32::from(*s) / 32768.0)
        .collect();
    let (rms, pitch) = level_and_pitch(&floats);
    println!("captured: level {rms:.3}, pitch {pitch:.0} Hz");
    assert!(rms > 0.05, "captured silence (level {rms})");
    assert!((pitch - 440.0).abs() < 15.0, "pitch {pitch}");
}

/// The test pattern's picture with the tone process's sound.
struct PatternWithTone(u64);

impl WindowSource for PatternWithTone {
    fn list_windows(&self) -> Vec<WindowInfo> {
        TestPatternSource.list_windows()
    }

    fn encoders(&self) -> Vec<VideoCodec> {
        TestPatternSource.encoders()
    }

    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        TestPatternSource.open(window, codec)
    }

    fn open_audio(&self, _: WindowId) -> Option<Result<Box<dyn AudioSource>, String>> {
        Some(ProcessAudio::open(WindowId(self.0)).map(|audio| Box::new(audio) as _))
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("windowcast-audio-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_window_sound_reaches_the_client_as_opus() {
    let Some(tone) = tone() else {
        println!("skipped: no audio output device");
        return;
    };
    let host_dir = temp_dir("host");
    let client_dir = temp_dir("client");
    let host_id = Identity::load_or_generate(&host_dir.join("agent-identity.key"))
        .unwrap()
        .peer_id();
    let client_id = Identity::load_or_generate(&client_dir.join("client-identity.key"))
        .unwrap()
        .peer_id();
    let mut trust = TrustStore::default();
    trust.pin(client_id);
    trust.save(&host_dir.join("agent-trusted-clients")).unwrap();
    let mut trust = TrustStore::default();
    trust.pin(host_id);
    trust
        .save(&client_dir.join("client-trusted-hosts"))
        .unwrap();

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let address = listener.local_addr().unwrap().to_string();
    runtime.spawn(windowcast_host::serve(
        listener,
        HostConfig {
            listen: address.clone(),
            pairing: false,
            data_dir: host_dir.clone(),
        },
        Arc::new(PatternWithTone(tone.1)),
    ));

    let client = Client::new(&client_dir).unwrap();
    let session = client.connect(&address, None).unwrap();
    session.start_window(WINDOW, &[VideoCodec::H264]).unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(Instant::now() < deadline, "the stream did not start");
        if let Some(Event::StreamStarted { .. }) = session.next_event(Duration::from_millis(200)) {
            break;
        }
    }

    let mut decoder = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
    let mut pcm = vec![0f32; 5760 * 2];
    let mut sound = Vec::new();
    let mut packets = 0;
    let deadline = Instant::now() + WAIT;
    // Drain the window's frames on the side so its queue never fills.
    while sound.len() < 48_000 * 2 {
        assert!(Instant::now() < deadline, "no sound arrived");
        let _ = session.next_frame(WINDOW, Duration::ZERO);
        if let AudioPoll::Packet(packet) = session.next_audio(WINDOW, Duration::from_millis(50)) {
            let frames = decoder.decode_float(&packet.data, &mut pcm, false).unwrap();
            sound.extend_from_slice(&pcm[..frames * 2]);
            packets += 1;
        }
    }
    let (rms, pitch) = level_and_pitch(&sound[24_000..]);
    println!("received {packets} Opus packets: level {rms:.3}, pitch {pitch:.0} Hz");
    assert!(rms > 0.05, "received silence (level {rms})");
    assert!((pitch - 440.0).abs() < 15.0, "pitch {pitch}");

    session.stop_window(WINDOW).unwrap();
    drop(session);
    drop(client);
    runtime.shutdown_timeout(Duration::from_secs(1));
    let _ = std::fs::remove_dir_all(host_dir);
    let _ = std::fs::remove_dir_all(client_dir);
}

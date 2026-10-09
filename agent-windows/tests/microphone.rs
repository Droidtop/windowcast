//! A client's microphone into the host's virtual microphone, end to end:
//! the client sends a 440 Hz tone as Opus over a real session and the host
//! plays it into the output device `WINDOWCAST_MICROPHONE_DEVICE` names
//! (part of its name). What the host plays is heard back through process
//! loopback of this very process (the host runs in it), which checks the
//! whole path without depending on how a particular cable driver passes
//! sound on to its recording side. Skipped where the variable is not set;
//! name a virtual output so nothing plays in the room.
#![cfg(windows)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windowcast_agent_windows::audio::ProcessAudio;
use windowcast_agent_windows::microphone::WindowsMicrophone;
use windowcast_cli_tools::testpattern::TestPatternSource;
use windowcast_client::Client;
use windowcast_host::audio::{AudioSource, MicrophoneSink};
use windowcast_host::{FrameSource, HostConfig, WindowSource};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{VideoCodec, WindowId, WindowInfo};

struct WithMicrophone(String);

impl WindowSource for WithMicrophone {
    fn list_windows(&self) -> Vec<WindowInfo> {
        TestPatternSource.list_windows()
    }

    fn encoders(&self) -> Vec<VideoCodec> {
        TestPatternSource.encoders()
    }

    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        TestPatternSource.open(window, codec)
    }

    fn microphone(&self) -> Option<Result<Box<dyn MicrophoneSink>, String>> {
        Some(WindowsMicrophone::open(Some(&self.0)).map(|mic| Box::new(mic) as _))
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("windowcast-mic-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_client_microphone_plays_into_the_virtual_cable() {
    let Ok(device) = std::env::var("WINDOWCAST_MICROPHONE_DEVICE") else {
        println!("skipped: name an output device in WINDOWCAST_MICROPHONE_DEVICE");
        return;
    };
    let mut heard = ProcessAudio::open_process(std::process::id()).unwrap();
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
        Arc::new(WithMicrophone(device)),
    ));

    let client = Client::new(&client_dir).unwrap();
    let session = Arc::new(client.connect(&address, None).unwrap());
    session.start_microphone().unwrap();
    let talking = {
        let session = Arc::clone(&session);
        std::thread::spawn(move || {
            let mut encoder =
                opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Voip)
                    .unwrap();
            let mut phase = 0f32;
            let mut packet = vec![0u8; 4000];
            // Paced to real time, as a microphone delivers.
            let start = Instant::now();
            for n in 0..150u32 {
                let mut pcm = Vec::with_capacity(1920);
                for _ in 0..960 {
                    let v = (phase.sin() * 8192.0) as i16;
                    pcm.extend([v, v]);
                    phase =
                        (phase + 440.0 * std::f32::consts::TAU / 48_000.0) % std::f32::consts::TAU;
                }
                let len = encoder.encode(&pcm, &mut packet).unwrap();
                session.send_microphone(&packet[..len]).unwrap();
                let due = start + Duration::from_millis(20) * (n + 1);
                std::thread::sleep(due.saturating_duration_since(Instant::now()));
            }
        })
    };
    // Skip the first second, then listen for one.
    let mut samples = Vec::new();
    let start = Instant::now();
    while samples.len() < 48_000 * 2 {
        assert!(start.elapsed() < Duration::from_secs(10), "nothing heard");
        let chunk = heard.next_samples().unwrap();
        if start.elapsed() > Duration::from_secs(1) {
            samples.extend(chunk);
        }
    }
    talking.join().unwrap();
    let left: Vec<f32> = samples
        .iter()
        .step_by(2)
        .map(|s| f32::from(*s) / 32768.0)
        .collect();
    let sound = &left[left.len() / 4..];
    let rms = (sound.iter().map(|s| s * s).sum::<f32>() / sound.len() as f32).sqrt();
    let crossings = sound
        .windows(2)
        .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
        .count();
    let pitch = crossings as f32 * 48_000.0 / sound.len() as f32;
    let quiet = sound.iter().filter(|s| s.abs() < 0.001).count();
    println!(
        "heard the host play: level {rms:.3}, pitch {pitch:.0} Hz, {quiet} of {} samples silent",
        sound.len()
    );
    assert!(rms > 0.05, "silence (level {rms})");
    assert!((pitch - 440.0).abs() < 15.0, "pitch {pitch}");

    session.stop_microphone().unwrap();
    drop(session);
    drop(client);
    runtime.shutdown_timeout(Duration::from_secs(1));
    let _ = std::fs::remove_dir_all(host_dir);
    let _ = std::fs::remove_dir_all(client_dir);
}

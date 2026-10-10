//! A host that streams a test pattern instead of real windows: for trying
//! a client (the Android viewer, droidtop) end to end on any machine,
//! Windows included, before that machine's capture exists. Pairing,
//! sessions and streams are the real host library.
//!
//! Usage: `windowcast-testhost [--listen ADDR:PORT] [--no-pairing] [--tone]
//! [--microphone-level]` (`--tone`: the window also sounds a 440 Hz tone,
//! for trying a client's audio; `--microphone-level`: a client's
//! microphone is taken and its level printed once a second, the sound
//! itself kept nowhere).

use std::sync::Arc;
use std::time::Instant;

use windowcast_cli_tools::testpattern::{TestPatternSource, TestPatternWithTone};
use windowcast_host::audio::{AudioSource, MicrophoneSink};
use windowcast_host::{FrameSource, HostConfig, WindowSource, DEFAULT_LISTEN};
use windowcast_protocol::{VideoCodec, WindowId, WindowInfo};

/// Another source, plus a microphone that only measures.
struct MeasuringMicrophone(Arc<dyn WindowSource>);

impl WindowSource for MeasuringMicrophone {
    fn list_windows(&self) -> Vec<WindowInfo> {
        self.0.list_windows()
    }
    fn encoders(&self) -> Vec<VideoCodec> {
        self.0.encoders()
    }
    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        self.0.open(window, codec)
    }
    fn open_audio(&self, window: WindowId) -> Option<Result<Box<dyn AudioSource>, String>> {
        self.0.open_audio(window)
    }
    fn microphone(&self) -> Option<Result<Box<dyn MicrophoneSink>, String>> {
        println!("microphone: a client started sending");
        Some(Ok(Box::new(Level {
            since: Instant::now(),
            squares: 0.0,
            peak: 0,
            samples: 0,
        })))
    }
}

/// Prints the level of what plays into it once a second; keeps nothing.
struct Level {
    since: Instant,
    squares: f64,
    peak: i32,
    samples: u64,
}

impl MicrophoneSink for Level {
    fn play(&mut self, samples: &[i16]) {
        for sample in samples {
            self.squares += f64::from(*sample).powi(2);
            self.peak = self.peak.max(i32::from(*sample).abs());
        }
        self.samples += samples.len() as u64;
        if self.since.elapsed().as_secs() >= 1 && self.samples > 0 {
            let rms = (self.squares / self.samples as f64).sqrt() / 32768.0;
            println!(
                "microphone: {} ms received, level {rms:.4}, peak {:.4}",
                self.samples * 1000 / 96_000,
                f64::from(self.peak) / 32768.0
            );
            *self = Level {
                since: Instant::now(),
                squares: 0.0,
                peak: 0,
                samples: 0,
            };
        }
    }
}

#[tokio::main]
async fn main() {
    let mut listen = DEFAULT_LISTEN.to_owned();
    let mut pairing = true;
    let mut tone = false;
    let mut measure = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next().expect("--listen needs ADDR:PORT"),
            "--no-pairing" => pairing = false,
            "--tone" => tone = true,
            "--microphone-level" => measure = true,
            other => panic!("unknown argument {other}"),
        }
    }
    let config = HostConfig {
        listen,
        pairing,
        data_dir: windowcast_cli_tools::data_dir().join("testhost"),
    };
    let source: Arc<dyn WindowSource> = if tone {
        Arc::new(TestPatternWithTone)
    } else {
        Arc::new(TestPatternSource)
    };
    let source: Arc<dyn WindowSource> = if measure {
        Arc::new(MeasuringMicrophone(source))
    } else {
        source
    };
    // RDP for clients that ask for it (windowcast-rdp is the protocol's
    // own client; windowcast clients get the login over the session).
    let source: Arc<dyn WindowSource> =
        match windowcast_rdp::host::WithRdp::new(source, std::net::IpAddr::from([0, 0, 0, 0])) {
            Ok(with_rdp) => Arc::new(with_rdp),
            Err(e) => {
                eprintln!("no RDP: {e}");
                std::process::exit(1);
            }
        };
    if let Err(e) = windowcast_host::run(config, source).await {
        eprintln!("host stopped: {e}");
        std::process::exit(1);
    }
}

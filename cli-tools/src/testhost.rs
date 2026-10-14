//! A host that streams a test pattern instead of real windows: for trying
//! a client (the Android viewer, droidtop) end to end on any machine,
//! Windows included, before that machine's capture exists. Pairing,
//! sessions and streams are the real host library.
//!
//! Usage: `windowcast-testhost [--listen ADDR:PORT] [--no-pairing] [--tone]
//! [--microphone-level] [--content general|text|game|video] [--carriers native,rdp]
//! [--pattern moving|still] [--locked] [--cursor]`
//! (`--carriers`: the carriers offered, both by default; `--carriers native`
//! refuses a switch to RDP pictures, for trying a refused switch; `--content`: the
//! window's content hint, which the default rules turn into a backend, so
//! `--content text` makes a client get RDP; `--tone`: the window also sounds a 440 Hz tone,
//! for trying a client's audio; `--microphone-level`: a client's
//! microphone is taken and its level printed once a second, the sound
//! itself kept nowhere; `--pattern still`: the window stands still, lines
//! of "text" sent once a second, so a client's selector moves it to RDP
//! pictures by itself; `--locked`: the host says it is locked for the first
//! 30 s of every minute and unlocked for the rest, for trying how a client
//! shows a locked host, which keeps streaming; `--cursor`: the host's pointer
//! circles the window as a small arrow, for trying how a client draws it).

use std::sync::Arc;
use std::time::Instant;

use windowcast_cli_tools::testpattern::{
    parse_content, StillTextSource, TestPatternSource, TestPatternWithTone, WithContent,
    WithPointer,
};
use windowcast_host::audio::{AudioSource, MicrophoneSink};
use windowcast_host::{
    FrameSource, Handoff, HostConfig, PictureSource, WindowSource, DEFAULT_LISTEN,
};
use windowcast_protocol::{BackendKind, HostState, InputEvent, VideoCodec, WindowId, WindowInfo};

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
    fn input(&self, event: &InputEvent, focus: Option<WindowId>) {
        self.0.input(event, focus)
    }
    fn host_state(&self) -> HostState {
        self.0.host_state()
    }
    fn set_private_session(
        &self,
        start: bool,
        password: Option<&str>,
    ) -> Result<(), windowcast_host::PrivateRefusal> {
        self.0.set_private_session(start, password)
    }
    fn cursor(&self) -> Option<windowcast_host::CursorState> {
        self.0.cursor()
    }
    fn cursor_image(&self, shape: u64) -> Option<windowcast_protocol::CursorImage> {
        self.0.cursor_image(shape)
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

/// Another source on a host that says it is locked for the first 30 s of
/// every minute (`--locked`); everything else is the source's.
struct LockedHalfTheTime(Arc<dyn WindowSource>, Instant);

impl WindowSource for LockedHalfTheTime {
    fn list_windows(&self) -> Vec<WindowInfo> {
        self.0.list_windows()
    }
    fn encoders(&self) -> Vec<VideoCodec> {
        self.0.encoders()
    }
    fn backends(&self) -> Vec<BackendKind> {
        self.0.backends()
    }
    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        self.0.open(window, codec)
    }
    fn handoff(&self, kind: BackendKind, window: WindowId) -> Option<Result<Handoff, String>> {
        self.0.handoff(kind, window)
    }
    fn open_pictures(&self, window: WindowId) -> Option<Result<Box<dyn PictureSource>, String>> {
        self.0.open_pictures(window)
    }
    fn open_audio(&self, window: WindowId) -> Option<Result<Box<dyn AudioSource>, String>> {
        self.0.open_audio(window)
    }
    fn microphone(&self) -> Option<Result<Box<dyn MicrophoneSink>, String>> {
        self.0.microphone()
    }
    fn input(&self, event: &InputEvent, focus: Option<WindowId>) {
        self.0.input(event, focus)
    }
    fn host_state(&self) -> HostState {
        let locked = self.1.elapsed().as_secs() % 60 < 30;
        HostState {
            locked,
            ..self.0.host_state()
        }
    }
    fn set_private_session(
        &self,
        start: bool,
        password: Option<&str>,
    ) -> Result<(), windowcast_host::PrivateRefusal> {
        self.0.set_private_session(start, password)
    }
    fn cursor(&self) -> Option<windowcast_host::CursorState> {
        self.0.cursor()
    }
    fn cursor_image(&self, shape: u64) -> Option<windowcast_protocol::CursorImage> {
        self.0.cursor_image(shape)
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
    let mut content = None;
    let mut rdp = true;
    let mut still = false;
    let mut locked = false;
    let mut pointer = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next().expect("--listen needs ADDR:PORT"),
            "--no-pairing" => pairing = false,
            "--tone" => tone = true,
            "--microphone-level" => measure = true,
            "--content" => {
                let name = args
                    .next()
                    .expect("--content needs general, text, game or video");
                let Some(hint) = parse_content(&name) else {
                    panic!("unknown content {name}");
                };
                content = Some(hint);
            }
            "--carriers" => {
                let list = args.next().expect("--carriers needs a list: native, rdp");
                let names: Vec<&str> = list.split(',').map(str::trim).collect();
                if let Some(unknown) = names.iter().find(|n| !matches!(**n, "native" | "rdp")) {
                    panic!("unknown carrier {unknown} (native, rdp)");
                }
                rdp = names.contains(&"rdp");
            }
            "--pattern" => match args.next().as_deref() {
                Some("moving") => still = false,
                Some("still") => still = true,
                other => panic!("--pattern needs moving or still, not {other:?}"),
            },
            "--locked" => locked = true,
            "--cursor" => pointer = true,
            other => panic!("unknown argument {other}"),
        }
    }
    let config = HostConfig {
        listen,
        pairing,
        data_dir: windowcast_cli_tools::data_dir().join("testhost"),
    };
    if still && tone {
        panic!("--tone sounds over the moving pattern only");
    }
    let source: Arc<dyn WindowSource> = if still {
        Arc::new(StillTextSource)
    } else if tone {
        Arc::new(TestPatternWithTone)
    } else {
        Arc::new(TestPatternSource)
    };
    let source: Arc<dyn WindowSource> = match content {
        Some(content) => Arc::new(WithContent::new(source, content)),
        None => source,
    };
    let source: Arc<dyn WindowSource> = if pointer {
        println!("the host's pointer circles the window");
        Arc::new(WithPointer::new(source))
    } else {
        source
    };
    let source: Arc<dyn WindowSource> = if measure {
        Arc::new(MeasuringMicrophone(source))
    } else {
        source
    };
    // RDP for clients that ask for it (windowcast-rdp is the protocol's
    // own client; windowcast clients get the login over the session).
    let source: Arc<dyn WindowSource> = if !rdp {
        println!("carriers: native only (a switch to RDP is refused)");
        source
    } else {
        match windowcast_rdp::host::WithRdp::new(source, std::net::IpAddr::from([0, 0, 0, 0])) {
            Ok(with_rdp) => Arc::new(with_rdp),
            Err(e) => {
                eprintln!("no RDP: {e}");
                std::process::exit(1);
            }
        }
    };
    let source: Arc<dyn WindowSource> = if locked {
        println!("the host says it is locked for the first 30 s of every minute");
        Arc::new(LockedHalfTheTime(source, Instant::now()))
    } else {
        source
    };
    if let Err(e) = windowcast_host::run(config, source).await {
        eprintln!("host stopped: {e}");
        std::process::exit(1);
    }
}

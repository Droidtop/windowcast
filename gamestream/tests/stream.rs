//! Our GameStream client against our GameStream host on loopback: pair,
//! launch the test pattern window, set the stream up over RTSP, and decode
//! the frames that arrive (the first a keyframe); hear the window's tone
//! through the encrypted audio packets; then send input of each kind and
//! see it reach the window and its gamepads. Uses the standard
//! GameStream ports, so it runs on Linux only (Windows machines often run
//! Sunshine on them).
#![cfg(target_os = "linux")]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::{TestPatternSource, TestPatternWithTone, HEIGHT, WIDTH};
use windowcast_cli_tools::H264Check;
use windowcast_gamestream::client::{GameStreamClient, Host};
use windowcast_gamestream::crypto::Credentials;
use windowcast_gamestream::input::Input;
use windowcast_gamestream::server::{GameStreamServer, HTTPS_PORT};
use windowcast_gamestream::session::StreamRequest;
use windowcast_gamestream::windows::WindowApps;
use windowcast_host::gamepad::GamepadSink;
use windowcast_host::{FrameSource, WindowSource};
use windowcast_protocol::{
    GamepadState, InputEvent, PointerButton, TouchPhase, VideoCodec, WindowId, WindowInfo,
};

/// The test pattern, keeping what input reaches it.
#[derive(Default)]
struct Recording {
    events: Arc<Mutex<Vec<InputEvent>>>,
}

struct Pads(Arc<Mutex<Vec<InputEvent>>>);

impl GamepadSink for Pads {
    fn set(&mut self, pad: u8, state: &GamepadState) {
        self.0
            .lock()
            .unwrap()
            .push(InputEvent::Gamepad { pad, state: *state });
    }
    fn remove(&mut self, pad: u8) {
        self.0.lock().unwrap().push(InputEvent::GamepadGone { pad });
    }
}

impl WindowSource for Recording {
    fn list_windows(&self) -> Vec<WindowInfo> {
        TestPatternSource.list_windows()
    }
    fn encoders(&self) -> Vec<VideoCodec> {
        TestPatternSource.encoders()
    }
    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        TestPatternSource.open(window, codec)
    }
    fn open_audio(
        &self,
        window: WindowId,
    ) -> Option<Result<Box<dyn windowcast_host::audio::AudioSource>, String>> {
        TestPatternWithTone.open_audio(window)
    }
    fn input(&self, event: &InputEvent, _focus: Option<WindowId>) {
        self.events.lock().unwrap().push(event.clone());
    }
    fn gamepads(&self) -> Option<Result<Box<dyn GamepadSink>, String>> {
        Some(Ok(Box::new(Pads(Arc::clone(&self.events)))))
    }
}

#[test]
fn our_client_streams_from_our_host() {
    let dir = std::env::temp_dir().join(format!("windowcast-gs-stream-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let recording = Recording::default();
    let events = Arc::clone(&recording.events);
    let server = GameStreamServer::open(
        "windowcast test host",
        &dir,
        Arc::new(WindowApps(Arc::new(recording))),
    )
    .unwrap();
    let (http, https, rtsp) = runtime.block_on(async {
        (
            tokio::net::TcpListener::bind(("127.0.0.1", 47989))
                .await
                .unwrap(),
            tokio::net::TcpListener::bind(("127.0.0.1", HTTPS_PORT))
                .await
                .unwrap(),
            tokio::net::TcpListener::bind(("127.0.0.1", 48010))
                .await
                .unwrap(),
        )
    });
    runtime.spawn(Arc::clone(&server).serve(http, https, rtsp));

    let client = GameStreamClient::new(Credentials::generate("NVIDIA GameStream Client").unwrap());
    let host = Host {
        address: "127.0.0.1".into(),
        http_port: 47989,
        https_port: HTTPS_PORT,
        cert: None,
    };
    let typist = {
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(30) {
                if !server.pairing_requests().is_empty() {
                    return server.enter_pin("2468", None);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            false
        })
    };
    let host = client.pair(&host, "2468", "stream test").unwrap();
    assert!(typist.join().unwrap());
    let app = client.apps(&host).unwrap()[0].id;

    let stream = client
        .stream(
            &host,
            app,
            &StreamRequest {
                width: WIDTH as u32,
                height: HEIGHT as u32,
                fps: 30,
                bitrate_kbps: 2000,
                packet_size: 1024,
            },
        )
        .unwrap();
    let mut check = H264Check::new().unwrap();
    let mut frames = 0;
    let mut first_idr = None;
    let deadline = Instant::now() + Duration::from_secs(15);
    while frames < 30 && Instant::now() < deadline {
        if let Ok(frame) = stream.frames.recv_timeout(Duration::from_millis(500)) {
            first_idr.get_or_insert(frame.idr);
            check.decode(&frame.data).unwrap();
            frames += 1;
        }
    }
    println!(
        "{frames} frames, {} decoded at {:?}",
        check.pictures, check.dimensions
    );
    assert_eq!(first_idr, Some(true), "the first frame must be a keyframe");
    assert!(frames >= 30, "only {frames} frames");
    assert_eq!(check.dimensions, Some((WIDTH, HEIGHT)));

    // The window's 440 Hz tone, in 5 ms packets: a second of it.
    let mut decoder = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
    let mut sound = Vec::new();
    let mut pcm = vec![0i16; 5760 * 2];
    let deadline = Instant::now() + Duration::from_secs(10);
    while sound.len() < 48_000 * 2 && Instant::now() < deadline {
        if let Ok(packet) = stream.audio.recv_timeout(Duration::from_millis(500)) {
            let samples = decoder.decode(&packet, &mut pcm, false).unwrap();
            assert_eq!(samples, 240, "5 ms packets");
            sound.extend_from_slice(&pcm[..samples * 2]);
        }
    }
    assert!(
        sound.len() >= 48_000 * 2,
        "only {} samples of sound",
        sound.len() / 2
    );
    let left: Vec<f32> = sound.iter().step_by(2).map(|s| *s as f32).collect();
    let level = (left.iter().map(|s| s * s).sum::<f32>() / left.len() as f32).sqrt() / 32768.0;
    let crossings = left
        .windows(2)
        .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
        .count();
    println!("sound: level {level:.3}, {crossings} cycles in a second");
    assert!(level > 0.1, "level {level}");
    assert!((420..=460).contains(&crossings), "{crossings} Hz");

    // Input of each kind, as Moonlight sends it.
    let window = TestPatternSource.list_windows()[0].id;
    let pad = GamepadState {
        buttons: 0x1001,
        left_x: -32768,
        left_y: 32767,
        right_x: 5,
        right_y: -5,
        left_trigger: 255,
        right_trigger: 0,
    };
    let sent = [
        Input::PointerTo { x: 0.25, y: 0.5 },
        // 64 pixels right of the 160-wide picture's quarter mark.
        Input::PointerBy { dx: 64, dy: 0 },
        Input::Button {
            button: PointerButton::Left,
            pressed: true,
        },
        Input::Scroll { dx: 0.0, dy: 1.0 },
        Input::Key {
            keycode: 30,
            pressed: true,
        },
        Input::Text("ok".into()),
        Input::Touch {
            id: 3,
            x: 0.5,
            y: 0.5,
            phase: TouchPhase::Start,
        },
        Input::Gamepad {
            pad: 0,
            active: 1,
            state: pad,
        },
        Input::Gamepad {
            pad: 0,
            active: 0,
            state: GamepadState::default(),
        },
    ];
    for input in &sent {
        assert!(stream.input(input));
    }
    let expected = vec![
        InputEvent::PointerMove {
            window,
            x: 0.25,
            y: 0.5,
        },
        InputEvent::PointerMove {
            window,
            x: 0.25 + 64.0 / WIDTH as f32,
            y: 0.5,
        },
        InputEvent::PointerButton {
            window,
            button: PointerButton::Left,
            pressed: true,
        },
        InputEvent::PointerScroll {
            window,
            dx: 0.0,
            dy: 1.0,
        },
        InputEvent::Key {
            keycode: 30,
            pressed: true,
        },
        InputEvent::Text { text: "ok".into() },
        InputEvent::Touch {
            window,
            id: 3,
            x: 0.5,
            y: 0.5,
            phase: TouchPhase::Start,
        },
        InputEvent::Gamepad { pad: 0, state: pad },
        InputEvent::GamepadGone { pad: 0 },
    ];
    let deadline = Instant::now() + Duration::from_secs(5);
    while events.lock().unwrap().len() < expected.len() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let got = events.lock().unwrap().clone();
    // The absolute position goes through a 4096-step reference size.
    assert_eq!(got.len(), expected.len(), "{got:?}");
    for (got, expected) in got.iter().zip(&expected) {
        match (got, expected) {
            (
                InputEvent::PointerMove { x, y, .. },
                InputEvent::PointerMove { x: ex, y: ey, .. },
            ) => assert!((x - ex).abs() < 0.001 && (y - ey).abs() < 0.001, "{got:?}"),
            _ => assert_eq!(got, expected),
        }
    }
    client.cancel(&host).unwrap();
    drop(stream);
    let _ = std::fs::remove_dir_all(dir);
}

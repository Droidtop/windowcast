//! The Linux agent end to end under sway: a test window painting one
//! colour is captured (by itself, and cut out of its output for the
//! desktop backend), streamed over loopback and decoded, and a client's
//! click and keys reach it through the virtual pointer and keyboard. Needs
//! a running sway (`WAYLAND_DISPLAY`, `SWAYSOCK`; CI starts a headless
//! one), so it runs where `WINDOWCAST_TEST_SWAY` is set and is skipped
//! elsewhere.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windowcast_agent_linux::{toplevels, LinuxSource, Options};
use windowcast_cli_tools::H264Check;
use windowcast_client::{Client, ClientSession, Event, FramePoll};
use windowcast_host::HostConfig;
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{BackendKind, InputEvent, PointerButton, VideoCodec, WindowId};

const WAIT: Duration = Duration::from_secs(20);
const TITLE: &str = "windowcast sway test";
/// 0x3366cc in BT.601 studio range, as the decoder reports the centre.
const COLOUR_YUV: (u8, u8, u8) = (100, 180, 98);

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("windowcast-sway-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_for<T>(what: &str, mut found: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = found() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The test window, ended however the test ends.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn close_to(a: u8, b: u8) -> bool {
    (i32::from(a) - i32::from(b)).abs() <= 4
}

/// Streams `window` over `backend` and checks the centre of the decoded
/// picture is the window's colour.
fn check_stream(session: &ClientSession, window: WindowId, backend: BackendKind) {
    session.set_rules(vec![windowcast_protocol::selection::BackendRule {
        when: Default::default(),
        backend,
    }]);
    session.start_window(window, &[VideoCodec::H264]).unwrap();
    wait_for("the stream to start", || {
        match session.next_event(Duration::from_millis(100)) {
            Some(Event::StreamStarted { backend: got, .. }) => {
                assert_eq!(got, backend);
                Some(())
            }
            Some(Event::StreamRefused { reason, .. }) => panic!("refused: {reason}"),
            _ => None,
        }
    });
    let mut check = H264Check::new().unwrap();
    for _ in 0..3 {
        match session.next_frame(window, WAIT) {
            FramePoll::Frame(frame) => check.decode(&frame.data).unwrap(),
            _ => panic!("no frame over {backend:?}"),
        }
    }
    let centre = check.center.expect("a decoded picture");
    println!(
        "{backend:?}: {:?} centre {centre:?}",
        check.dimensions.unwrap()
    );
    assert!(
        close_to(centre.0, COLOUR_YUV.0)
            && close_to(centre.1, COLOUR_YUV.1)
            && close_to(centre.2, COLOUR_YUV.2),
        "{backend:?}: centre {centre:?}, expected {COLOUR_YUV:?}"
    );
    session.stop_window(window).unwrap();
    wait_for("the stream to stop", || {
        matches!(
            session.next_event(Duration::from_millis(100)),
            Some(Event::StreamStopped { .. })
        )
        .then_some(())
    });
}

/// Streams `window` and checks its sound decodes to the test window's
/// 440 Hz tone.
fn check_sound(session: &ClientSession, window: WindowId) {
    session.set_rules(Vec::new());
    session.start_window(window, &[VideoCodec::H264]).unwrap();
    let mut decoder = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
    let mut pcm = vec![0f32; 5760 * 2];
    let mut left: Vec<f32> = Vec::new();
    let deadline = Instant::now() + WAIT;
    while left.len() < 48_000 {
        assert!(Instant::now() < deadline, "no sound arrived");
        let _ = session.next_event(Duration::ZERO);
        let _ = session.next_frame(window, Duration::ZERO);
        if let windowcast_client::AudioPoll::Packet(packet) =
            session.next_audio(window, Duration::from_millis(50))
        {
            let frames = decoder.decode_float(&packet.data, &mut pcm, false).unwrap();
            left.extend(pcm[..frames * 2].iter().step_by(2));
        }
    }
    let sound = &left[12_000..];
    let rms = (sound.iter().map(|s| s * s).sum::<f32>() / sound.len() as f32).sqrt();
    let crossings = sound
        .windows(2)
        .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
        .count();
    let pitch = crossings as f32 * 48_000.0 / sound.len() as f32;
    println!("sound: level {rms:.3}, pitch {pitch:.0} Hz");
    assert!(rms > 0.05, "silence (level {rms})");
    assert!((pitch - 440.0).abs() < 15.0, "pitch {pitch}");
    session.stop_window(window).unwrap();
    wait_for("the stream to stop", || {
        matches!(
            session.next_event(Duration::from_millis(100)),
            Some(Event::StreamStopped { .. })
        )
        .then_some(())
    });
}

/// Sends a 660 Hz tone as the client's microphone and records it back from
/// the host's virtual microphone with parec. The window's own 440 Hz tone
/// keeps playing, so its leaking into the microphone shows in the level.
fn check_microphone(session: &ClientSession) {
    use std::io::Read;
    session.start_microphone().unwrap();
    let mut encoder =
        opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Voip).unwrap();
    let mut phase = 0f32;
    let mut packet = vec![0u8; 4000];
    // Paced to real time on an absolute schedule, as a microphone delivers;
    // sleeping 20 ms after each packet runs slow and leaves gaps.
    let mut talk = |packets: u32| {
        let start = Instant::now();
        for n in 0..packets {
            let mut pcm = Vec::with_capacity(1920);
            for _ in 0..960 {
                let v = (phase.sin() * 8192.0) as i16;
                pcm.extend([v, v]);
                phase = (phase + 660.0 * std::f32::consts::TAU / 48_000.0) % std::f32::consts::TAU;
            }
            let len = encoder.encode(&pcm, &mut packet).unwrap();
            session.send_microphone(&packet[..len]).unwrap();
            let due = start + Duration::from_millis(20) * (n + 1);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
    };
    // The host makes its virtual microphone when the first packets come.
    talk(50);
    let mut recorder = KillOnDrop(
        Command::new("parec")
            .args([
                "-d",
                windowcast_agent_linux::microphone::SOURCE,
                "--raw",
                "--rate=48000",
                "--channels=2",
                "--format=s16le",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdout = recorder.0.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = vec![0u8; 48_000 * 4 * 2];
        stdout.read_exact(&mut bytes).map(|()| bytes)
    });
    talk(150);
    let bytes = reader.join().unwrap().expect("parec recorded nothing");
    drop(recorder);
    let left: Vec<f32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|frame| f32::from(i16::from_le_bytes([frame[0], frame[1]])) / 32768.0)
        .collect();
    // The second of the two seconds recorded, well after the sound settles.
    let sound = &left[48_000..];
    let rms = (sound.iter().map(|s| s * s).sum::<f32>() / sound.len() as f32).sqrt();
    let crossings = sound
        .windows(2)
        .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
        .count();
    let pitch = crossings as f32 * 48_000.0 / sound.len() as f32;
    println!("microphone: level {rms:.3}, pitch {pitch:.0} Hz");
    assert!(rms > 0.05, "silence (level {rms})");
    assert!((pitch - 660.0).abs() < 15.0, "pitch {pitch}");
    // The tone alone is 0.177; the window's tone on top makes 0.25.
    assert!(rms < 0.21, "more than the client's voice (level {rms})");
    session.stop_microphone().unwrap();
}

#[test]
fn a_window_streams_and_takes_input_under_sway() {
    if std::env::var_os("WINDOWCAST_TEST_SWAY").is_none() {
        println!("skipped: needs a running sway; set WINDOWCAST_TEST_SWAY to run");
        return;
    }
    // With a Pulse server (WINDOWCAST_TEST_PULSE), the window plays a tone.
    let pulse = std::env::var_os("WINDOWCAST_TEST_PULSE").is_some();
    if pulse {
        // Speakers for the window to play into. A server with no output
        // has only module-always-sink's placeholder, which gives way to the
        // first real sink (the virtual microphone's) and takes the
        // window's sound with it: nothing a host can put back.
        let speakers = Command::new("pactl")
            .args([
                "load-module",
                "module-null-sink",
                "sink_name=windowcast_test_speakers",
            ])
            .status()
            .and_then(|_| {
                Command::new("pactl")
                    .args(["set-default-sink", "windowcast_test_speakers"])
                    .status()
            });
        assert!(
            speakers.is_ok_and(|s| s.success()),
            "pactl could not make speakers"
        );
        // And a server that hands the default, and its streams, to every
        // new sink, as some desktops do: the host must put them back.
        let switching = Command::new("pactl")
            .args([
                "load-module",
                "module-switch-on-connect",
                "ignore_virtual=no",
            ])
            .status();
        assert!(
            switching.is_ok_and(|s| s.success()),
            "pactl could not load module-switch-on-connect"
        );
    }
    let mut child = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_windowcast-test-window"))
            .args([TITLE, "3366cc"])
            .args(pulse.then_some("tone"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let lines = Arc::clone(&lines);
        let stdout = child.0.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                lines.lock().unwrap().push(line);
            }
        });
    }
    let saw = |prefix: &str| {
        lines
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|line| line.starts_with(prefix))
            .cloned()
    };
    // Tiled by sway: the second configure has the window's real size.
    wait_for("the window", || {
        (lines
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with("ready"))
            .count()
            >= 2)
            .then_some(())
    });
    let size = saw("ready").unwrap();
    let (width, height) = size[6..].split_once('x').unwrap();
    let (width, height): (f32, f32) = (width.parse().unwrap(), height.parse().unwrap());

    let window = wait_for("the window in the list", || {
        toplevels::list_windows()
            .unwrap()
            .into_iter()
            .find(|w| w.title == TITLE)
    });
    println!("window {} {}x{}", window.id.0, window.width, window.height);
    let id = window.id;

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
        Arc::new(LinuxSource::new(Options::default())),
    ));

    let client = Client::new(&client_dir).unwrap();
    let session = client.connect(&address, None).unwrap();
    session.request_windows().unwrap();
    wait_for("the window list", || {
        matches!(
            session.next_event(Duration::from_millis(100)),
            Some(Event::Windows { .. })
        )
        .then_some(())
    });

    check_stream(&session, id, BackendKind::Native);
    check_stream(&session, id, BackendKind::Desktop);
    if pulse {
        check_sound(&session, id);
        check_microphone(&session);
    }

    // Input needs a streamed window: stream it again, then click a
    // quarter of the way across, half way down, and press A.
    session.set_rules(Vec::new());
    session.start_window(id, &[VideoCodec::H264]).unwrap();
    wait_for("the stream to start", || {
        matches!(
            session.next_event(Duration::from_millis(100)),
            Some(Event::StreamStarted { .. })
        )
        .then_some(())
    });
    assert!(matches!(session.next_frame(id, WAIT), FramePoll::Frame(_)));
    for event in [
        InputEvent::PointerMove {
            window: id,
            x: 0.25,
            y: 0.5,
        },
        InputEvent::PointerButton {
            window: id,
            button: PointerButton::Left,
            pressed: true,
        },
        InputEvent::PointerButton {
            window: id,
            button: PointerButton::Left,
            pressed: false,
        },
        InputEvent::Key {
            keycode: 30,
            pressed: true,
        },
        InputEvent::Key {
            keycode: 30,
            pressed: false,
        },
        InputEvent::Text { text: "B".into() },
    ] {
        session.send_input(event).unwrap();
    }
    wait_for("the click", || saw("button 272 pressed"));
    let motion = saw("motion").unwrap();
    let mut parts = motion.split(' ').skip(1).map(|v| v.parse::<f32>().unwrap());
    let (x, y) = (parts.next().unwrap(), parts.next().unwrap());
    println!("pointer at ({x}, {y}) in a {width}x{height} window");
    assert!(((x - (width - 1.0) * 0.25).abs()) <= 2.0, "x {x}");
    assert!(((y - (height - 1.0) * 0.5).abs()) <= 2.0, "y {y}");
    wait_for("A", || saw("key 30 pressed"));
    // "B" is Shift (42) then B (48).
    wait_for("B", || saw("key 48 pressed"));
    assert!(saw("key 42 pressed").is_some(), "Shift for a capital B");

    drop(session);
    drop(client);
    runtime.shutdown_timeout(Duration::from_secs(1));
    drop(child);
    let _ = std::fs::remove_dir_all(host_dir);
    let _ = std::fs::remove_dir_all(client_dir);
}

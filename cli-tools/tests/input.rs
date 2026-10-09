//! The input back-channel, clipboard and resize messages through both ends
//! of the library: the client's events reach the host's WindowSource in
//! order with the right focus, input for a window the session does not
//! stream is dropped, the clipboard goes both ways without echoing, and
//! the client hears the picture size before the first frame. Gamepads go
//! to the session's own virtual pads, which go when the session does.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::{TestPatternSource, HEIGHT, WIDTH, WINDOW};
use windowcast_client::{Client, ClientSession, Event, FramePoll};
use windowcast_host::gamepad::GamepadSink;
use windowcast_host::{FrameSource, HostConfig, WindowSource};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{
    GamepadButtons, GamepadState, InputEvent, PointerButton, TouchPhase, VideoCodec, WindowId,
    WindowInfo,
};

const WAIT: Duration = Duration::from_secs(20);

/// The test pattern, plus a record of the input and clipboard it got and a
/// clipboard the test can change.
#[derive(Default)]
struct Recorder {
    inputs: Mutex<Vec<(InputEvent, Option<WindowId>)>>,
    set_clipboard: Mutex<Vec<String>>,
    clipboard: Mutex<(u64, String)>,
    pads: Mutex<Vec<PadEvent>>,
}

#[derive(Debug, Clone, PartialEq)]
enum PadEvent {
    Made,
    Set(u8, GamepadState),
    Removed(u8),
    Dropped,
}

struct Pads(Arc<Recorder>);

impl GamepadSink for Pads {
    fn set(&mut self, pad: u8, state: &GamepadState) {
        self.0.pads.lock().unwrap().push(PadEvent::Set(pad, *state));
    }
    fn remove(&mut self, pad: u8) {
        self.0.pads.lock().unwrap().push(PadEvent::Removed(pad));
    }
}

impl Drop for Pads {
    fn drop(&mut self) {
        self.0.pads.lock().unwrap().push(PadEvent::Dropped);
    }
}

struct Source(Arc<Recorder>);

impl WindowSource for Source {
    fn list_windows(&self) -> Vec<WindowInfo> {
        TestPatternSource.list_windows()
    }
    fn encoders(&self) -> Vec<VideoCodec> {
        TestPatternSource.encoders()
    }
    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        TestPatternSource.open(window, codec)
    }
    fn input(&self, event: &InputEvent, focus: Option<WindowId>) {
        self.0.inputs.lock().unwrap().push((event.clone(), focus));
    }
    fn gamepads(&self) -> Option<Result<Box<dyn GamepadSink>, String>> {
        self.0.pads.lock().unwrap().push(PadEvent::Made);
        Some(Ok(Box::new(Pads(Arc::clone(&self.0)))))
    }
    fn clipboard(&self) -> Option<(u64, String)> {
        Some(self.0.clipboard.lock().unwrap().clone())
    }
    fn set_clipboard(&self, text: &str) {
        self.0.set_clipboard.lock().unwrap().push(text.to_owned());
        let mut clipboard = self.0.clipboard.lock().unwrap();
        // Setting the clipboard changes it, as on a real host.
        *clipboard = (clipboard.0 + 1, text.to_owned());
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("windowcast-input-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_for(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn next_event(session: &ClientSession, matches: impl Fn(&Event) -> bool) -> Event {
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(Instant::now() < deadline, "event did not arrive");
        if let Some(event) = session.next_event(Duration::from_millis(200)) {
            if matches(&event) {
                return event;
            }
        }
    }
}

#[test]
fn input_clipboard_and_resize_go_through() {
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

    let recorder = Arc::new(Recorder::default());
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
        Arc::new(Source(Arc::clone(&recorder))),
    ));

    let client = Client::new(&client_dir).unwrap();
    let session = client.connect(&address, None).unwrap();

    // Before any stream, input has nowhere to go and is dropped.
    session
        .send_input(InputEvent::Key {
            keycode: 30,
            pressed: true,
        })
        .unwrap();

    session.start_window(WINDOW, &[VideoCodec::H264]).unwrap();
    next_event(&session, |e| matches!(e, Event::StreamStarted { .. }));
    // The size is announced with the first frame.
    let resized = next_event(&session, |e| matches!(e, Event::WindowResized { .. }));
    assert_eq!(
        resized,
        Event::WindowResized {
            window: WINDOW.0,
            width: WIDTH as u32,
            height: HEIGHT as u32
        }
    );
    assert!(matches!(
        session.next_frame(WINDOW, WAIT),
        FramePoll::Frame(_)
    ));

    let sent = vec![
        InputEvent::PointerMove {
            window: WINDOW,
            x: 0.25,
            y: 0.75,
        },
        InputEvent::PointerButton {
            window: WINDOW,
            button: PointerButton::Left,
            pressed: true,
        },
        InputEvent::Key {
            keycode: 30,
            pressed: true,
        },
        InputEvent::Text { text: "é".into() },
        InputEvent::Touch {
            window: WINDOW,
            id: 0,
            x: 0.5,
            y: 0.5,
            phase: TouchPhase::Start,
        },
    ];
    // A window this session does not stream: dropped.
    session
        .send_input(InputEvent::PointerMove {
            window: WindowId(999),
            x: 0.5,
            y: 0.5,
        })
        .unwrap();
    for event in &sent {
        session.send_input(event.clone()).unwrap();
    }
    wait_for("the input", || {
        recorder.inputs.lock().unwrap().len() >= sent.len()
    });
    std::thread::sleep(Duration::from_millis(200));
    let got = recorder.inputs.lock().unwrap().clone();
    assert_eq!(
        got.len(),
        sent.len(),
        "unexpected input reached the host: {got:?}"
    );
    for ((event, focus), want) in got.iter().zip(&sent) {
        assert_eq!(event, want);
        assert_eq!(*focus, Some(WINDOW));
    }

    // Gamepads: one sink for the session, made at its first pad event; a
    // pad out of range is dropped.
    let pad = GamepadState {
        buttons: GamepadButtons::A,
        left_x: 12000,
        ..Default::default()
    };
    for event in [
        InputEvent::Gamepad { pad: 1, state: pad },
        InputEvent::Gamepad { pad: 9, state: pad },
        InputEvent::Gamepad {
            pad: 1,
            state: GamepadState::default(),
        },
        InputEvent::GamepadGone { pad: 1 },
    ] {
        session.send_input(event).unwrap();
    }
    wait_for("the pads", || recorder.pads.lock().unwrap().len() >= 4);
    assert_eq!(
        *recorder.pads.lock().unwrap(),
        [
            PadEvent::Made,
            PadEvent::Set(1, pad),
            PadEvent::Set(1, GamepadState::default()),
            PadEvent::Removed(1),
        ]
    );
    assert_eq!(recorder.inputs.lock().unwrap().len(), sent.len());

    // Client to host.
    session.set_clipboard("from the client").unwrap();
    wait_for("the client's clipboard", || {
        recorder.set_clipboard.lock().unwrap().as_slice() == ["from the client"]
    });
    // Host to client; and the client's own text was not echoed back first.
    {
        let mut clipboard = recorder.clipboard.lock().unwrap();
        *clipboard = (clipboard.0 + 1, "from the host".into());
    }
    let event = next_event(&session, |e| matches!(e, Event::Clipboard { .. }));
    assert_eq!(
        event,
        Event::Clipboard {
            text: "from the host".into()
        }
    );

    session.stop_window(WINDOW).unwrap();
    drop(session);
    // The session's pads go with it.
    wait_for("the pads to go", || {
        recorder.pads.lock().unwrap().last() == Some(&PadEvent::Dropped)
    });
    drop(client);
    runtime.shutdown_timeout(Duration::from_secs(1));
    let _ = std::fs::remove_dir_all(host_dir);
    let _ = std::fs::remove_dir_all(client_dir);
}

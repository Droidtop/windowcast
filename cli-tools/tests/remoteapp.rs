//! RemoteApp launches, run by the host (docs/BACKENDS.md, "RemoteApp").
//! Everywhere: a launch the rules give RDP asks the host for a RemoteApp
//! with the Windows password the client has, and the host's answer (it
//! started, it needs the password, a refusal, or none) is what the client
//! gets. On a Windows host with Remote Desktop on
//! (`WINDOWCAST_TEST_REMOTEAPP`, set by CI on its Windows runner, which
//! runs as an administrator): Notepad runs as a RemoteApp under a user the
//! host makes, its window joins the host's window list, streams as video
//! like any window of the host's, and typing into it reaches it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::TestPatternSource;
use windowcast_client::{Client, ClientError, ClientSession, Event, FramePoll};
use windowcast_host::command::{Principal, RemoteAppRefusal, RemoteApps};
use windowcast_host::{HostConfig, HostControl, WindowSource};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{VideoCodec, WindowId};
use windowcast_rdp::remoteapp_windows::{RemoteAppWindows, WithRemoteApps, REMOTE_APP_WINDOW};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "windowcast-remoteapp-{name}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Rig {
    _runtime: tokio::runtime::Runtime,
    control: Arc<HostControl>,
    apps: Arc<RemoteAppWindows>,
    client: Client,
    address: String,
    host_dir: PathBuf,
}

/// A host (the test pattern plus its RemoteApps' windows) and a client
/// that trust each other, on loopback.
fn rig(name: &str) -> Rig {
    let host_dir = temp_dir(&format!("{name}-host"));
    let client_dir = temp_dir(&format!("{name}-client"));
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
    let control = HostControl::open(&HostConfig {
        listen: address.clone(),
        pairing: false,
        data_dir: host_dir.clone(),
    })
    .unwrap();
    let apps = Arc::new(RemoteAppWindows::default());
    let source: Arc<dyn WindowSource> = Arc::new(WithRemoteApps::new(
        Arc::new(TestPatternSource),
        Arc::clone(&apps),
    ));
    runtime.spawn(windowcast_host::serve_with(
        listener,
        Arc::clone(&control),
        source,
    ));
    Rig {
        _runtime: runtime,
        control,
        apps,
        client: Client::new(&client_dir).unwrap(),
        address,
        host_dir,
    }
}

/// Needs the password "pw" for the user "mech", or answers as `answer`
/// says; keeps what it was asked.
struct Answer {
    answer: Option<Result<(), RemoteAppRefusal>>,
    asked: Mutex<Vec<(Vec<String>, Option<String>)>>,
}

impl RemoteApps for Answer {
    fn launch(
        &self,
        _who: &Principal,
        argv: &[String],
        password: Option<&str>,
    ) -> Option<Result<(), RemoteAppRefusal>> {
        self.asked
            .lock()
            .unwrap()
            .push((argv.to_vec(), password.map(str::to_owned)));
        self.answer.clone().map(|answer| match (answer, password) {
            (Ok(()), Some("pw")) => Ok(()),
            (Ok(()), _) => Err(RemoteAppRefusal::PasswordNeeded("mech".into())),
            (refused, _) => refused,
        })
    }
}

fn answer(rig: &Rig, answer: Option<Result<(), RemoteAppRefusal>>) -> Arc<Answer> {
    let provider = Arc::new(Answer {
        answer,
        asked: Mutex::default(),
    });
    rig.control
        .set_remote_apps(Some(Arc::clone(&provider) as Arc<dyn RemoteApps>));
    provider
}

fn notepad() -> Vec<String> {
    vec![r"C:\Windows\System32\notepad.exe".into()]
}

#[test]
fn a_launch_the_rules_give_rdp_follows_the_hosts_remoteapp_answer() {
    let rig = rig("answers");

    // The host needs the Windows password: the user is asked, and the
    // launch with it starts the RemoteApp.
    let provider = answer(&rig, Some(Ok(())));
    let session = rig.client.connect(&rig.address, None).unwrap();
    let launched = session.launch(notepad());
    assert!(
        matches!(&launched, Err(ClientError::PasswordNeeded(user)) if user == "mech"),
        "{launched:?}"
    );
    let launched = session.launch_with_password(notepad(), Some("pw"));
    assert!(matches!(launched, Ok(None)), "{launched:?}");
    assert_eq!(
        provider.asked.lock().unwrap().last(),
        Some(&(notepad(), Some("pw".to_owned())))
    );

    // A program the rules leave native is never asked for as a RemoteApp.
    let _ = session.launch(vec!["no-such-game-binary".into()]);
    assert_eq!(provider.asked.lock().unwrap().len(), 2);
    drop(session);

    // The host refuses: the reason reaches the user.
    let _provider = answer(
        &rig,
        Some(Err(RemoteAppRefusal::Refused(
            "Remote Desktop is off".into(),
        ))),
    );
    let session = rig.client.connect(&rig.address, None).unwrap();
    let refused = session.launch(notepad());
    assert!(
        matches!(&refused, Err(ClientError::Refused(r)) if r.contains("Remote Desktop is off")),
        "{refused:?}"
    );
    drop(session);

    // The host does not serve it as a RemoteApp now: it is an ordinary
    // launch on the host (where this Notepad does not exist, except on
    // Windows).
    let provider = answer(&rig, None);
    let session = rig.client.connect(&rig.address, None).unwrap();
    let plain = session.launch(vec!["/no/such/notepad.exe".into()]);
    assert!(
        matches!(&plain, Err(ClientError::Refused(r)) if r.contains("could not start")),
        "{plain:?}"
    );
    assert_eq!(provider.asked.lock().unwrap().len(), 1);
    assert!(rig.apps.infos().is_empty());
}

fn next_event(session: &ClientSession, until: Instant) -> Event {
    loop {
        assert!(Instant::now() < until, "timed out");
        if let Some(event) = session.next_event(Duration::from_millis(500)) {
            return event;
        }
    }
}

/// The RemoteApp windows of a window list event, by id and title.
fn remote_windows(event: &Event) -> Vec<(WindowId, String, u32, u32)> {
    match event {
        Event::Windows { windows } => windows
            .iter()
            .filter(|w| w.id.0 & REMOTE_APP_WINDOW != 0)
            .map(|w| (w.id, w.title.clone(), w.width, w.height))
            .collect(),
        _ => Vec::new(),
    }
}

#[test]
fn notepad_runs_as_a_remoteapp_of_the_host_and_streams_like_its_windows() {
    if std::env::var("WINDOWCAST_TEST_REMOTEAPP").is_err() {
        println!("skipped: set WINDOWCAST_TEST_REMOTEAPP on a Windows host with Remote Desktop on");
        return;
    }
    let rig = rig("notepad");
    let remote_apps = windowcast_rdp::remoteapp_host::WindowsRemoteApps::new(
        rig.host_dir.clone(),
        windowcast_rdp::remoteapp_host::Setting {
            availability: windowcast_rdp::remoteapp_host::Availability::On,
            login: windowcast_rdp::remoteapp_host::Login::HostAccount,
        },
        Arc::clone(&rig.apps),
    );
    println!(
        "Remote Desktop here: {:?}",
        windowcast_rdp::remoteapp_host::WindowsRemoteApps::status()
    );
    rig.control.set_remote_apps(Some(Arc::new(remote_apps)));
    let session = rig.client.connect(&rig.address, None).unwrap();
    session.request_windows().unwrap();
    let started = Instant::now();
    let pid = session.launch(notepad()).unwrap();
    assert_eq!(pid, None, "a RemoteApp has no process id here");
    println!("Notepad started as a RemoteApp in {:?}", started.elapsed());

    // Its window joins the host's window list (with any other window
    // Windows shows in that session, such as a new user's first "System
    // Properties"), sent by the host as the list changes.
    let until = Instant::now() + Duration::from_secs(60);
    let (window, title, width, height) = loop {
        let event = next_event(&session, until);
        for listed in remote_windows(&event) {
            println!("listed: {:?} {}x{}", listed.1, listed.2, listed.3);
        }
        if let Some(found) = remote_windows(&event).into_iter().find(|(_, title, w, h)| {
            title.to_lowercase().contains("notepad") && *w > 100 && *h > 100
        }) {
            break found;
        }
    };
    println!("Notepad's window: {title:?} {width}x{height}");

    // It streams as video from the host, like the host's own windows.
    session.start_window(window, &[VideoCodec::H264]).unwrap();
    loop {
        match next_event(&session, until) {
            Event::StreamStarted {
                window: w, backend, ..
            } if w == window.0 => {
                println!("streaming over {backend:?}");
                break;
            }
            Event::StreamRefused { reason, .. } => panic!("refused: {reason}"),
            _ => {}
        }
    }
    let frame = loop {
        match session.next_frame(window, Duration::from_secs(1)) {
            FramePoll::Frame(frame) => break frame,
            FramePoll::Ended => panic!("the RemoteApp's stream ended"),
            FramePoll::Timeout => assert!(Instant::now() < until, "no frame"),
        }
    };
    assert!(frame.keyframe, "the first frame is a keyframe");
    println!("first frame: {} bytes", frame.data.len());

    // Typing reaches Notepad through the session: it marks its title as
    // changed.
    use windowcast_protocol::{InputEvent, PointerButton};
    for event in [
        InputEvent::PointerMove {
            window,
            x: 0.5,
            y: 0.5,
        },
        InputEvent::PointerButton {
            window,
            button: PointerButton::Left,
            pressed: true,
        },
        InputEvent::PointerButton {
            window,
            button: PointerButton::Left,
            pressed: false,
        },
        InputEvent::Text {
            text: "windowcast".into(),
        },
    ] {
        session.send_input(event).unwrap();
    }
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let event = next_event(&session, until);
        if let Some((_, title, _, _)) = remote_windows(&event)
            .into_iter()
            .find(|(w, ..)| *w == window)
        {
            println!("title now {title:?}");
            if title.starts_with('*') {
                break;
            }
        }
    }
}

//! RemoteApp launches through a windowcast session (docs/BACKENDS.md,
//! "RemoteApp"). Everywhere: a launch the rules give RDP asks the host for a
//! RemoteApp, and the host's answer (a login, a refusal, or none) is what
//! the client does. On a Windows host with Remote Desktop on
//! (`WINDOWCAST_TEST_REMOTEAPP`, set by CI on its Windows runner, which runs
//! as an administrator): Notepad runs as a RemoteApp under a user the host
//! makes, its window joins the session's window list, its picture comes,
//! and typing into it reaches it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::TestPatternSource;
use windowcast_client::{Client, ClientError, ClientSession, Event, REMOTE_APP_WINDOW};
use windowcast_host::command::{Principal, RemoteAppLogin, RemoteApps};
use windowcast_host::{HostConfig, HostControl};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{HandoffTarget, WindowId};

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
    client: Client,
    address: String,
    host_dir: PathBuf,
}

/// A host and a client that trust each other, on loopback.
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
    runtime.spawn(windowcast_host::serve_with(
        listener,
        Arc::clone(&control),
        Arc::new(TestPatternSource),
    ));
    Rig {
        _runtime: runtime,
        control,
        client: Client::new(&client_dir).unwrap(),
        address,
        host_dir,
    }
}

/// Answers every RemoteApp launch the same way, and counts them.
struct Answer {
    answer: Option<Result<RemoteAppLogin, String>>,
    asked: Mutex<Vec<Vec<String>>>,
}

impl RemoteApps for Answer {
    fn launch(&self, _who: &Principal, argv: &[String]) -> Option<Result<RemoteAppLogin, String>> {
        self.asked.lock().unwrap().push(argv.to_vec());
        self.answer.clone()
    }
}

fn answer(rig: &Rig, answer: Option<Result<RemoteAppLogin, String>>) -> Arc<Answer> {
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

    // A login with no password, for a user the client did not sign in as:
    // the user is asked for the Windows password.
    let provider = answer(
        &rig,
        Some(Ok(RemoteAppLogin {
            target: HandoffTarget {
                address: String::new(),
                port: 3389,
                username: "mech".into(),
                password: String::new(),
                certificate_sha256: Some([0; 32]),
            },
            sign_in_password: false,
        })),
    );
    let session = rig.client.connect(&rig.address, None).unwrap();
    session.accept_pictures(true);
    let launched = session.launch(notepad());
    assert!(
        matches!(&launched, Err(ClientError::PasswordNeeded(user)) if user == "mech"),
        "{launched:?}"
    );
    assert_eq!(provider.asked.lock().unwrap().len(), 1);

    // A program the rules leave native is never asked for as a RemoteApp,
    // and neither is anything from a client that does not show pictures.
    let _ = session.launch(vec!["no-such-game-binary".into()]);
    session.accept_pictures(false);
    let _ = session.launch(notepad());
    assert_eq!(provider.asked.lock().unwrap().len(), 1);
    drop(session);

    // The host refuses: the reason reaches the user.
    let _provider = answer(&rig, Some(Err("Remote Desktop is off".into())));
    let session = rig.client.connect(&rig.address, None).unwrap();
    session.accept_pictures(true);
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
    session.accept_pictures(true);
    let plain = session.launch(vec!["/no/such/notepad.exe".into()]);
    assert!(
        matches!(&plain, Err(ClientError::Refused(r)) if r.contains("could not start")),
        "{plain:?}"
    );
    assert_eq!(provider.asked.lock().unwrap().len(), 1);
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
fn notepad_runs_as_a_remoteapp_through_a_session() {
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
    );
    println!(
        "Remote Desktop here: {:?}",
        windowcast_rdp::remoteapp_host::WindowsRemoteApps::status()
    );
    rig.control.set_remote_apps(Some(Arc::new(remote_apps)));
    let session = rig.client.connect(&rig.address, None).unwrap();
    session.accept_pictures(true);
    let started = Instant::now();
    let pid = session.launch(notepad()).unwrap();
    assert_eq!(pid, None, "a RemoteApp has no process id here");
    println!("Notepad started as a RemoteApp in {:?}", started.elapsed());

    // Its window joins the session's window list (with any other window
    // Windows shows in that session, such as a new user's first "System
    // Properties").
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

    // Shown, it streams over RDP: its own picture, not the desktop's.
    session.start_window(window, &[]).unwrap();
    loop {
        match next_event(&session, until) {
            Event::StreamStarted {
                window: w, backend, ..
            } if w == window.0 => {
                assert_eq!(backend, windowcast_protocol::BackendKind::Rdp);
                break;
            }
            Event::StreamRefused { reason, .. } => panic!("refused: {reason}"),
            _ => {}
        }
    }
    let picture = loop {
        match session.next_picture(window, Duration::from_secs(1)) {
            windowcast_client::PicturePoll::Picture(picture) => break picture,
            windowcast_client::PicturePoll::Ended => panic!("the RemoteApp ended"),
            windowcast_client::PicturePoll::Timeout => {
                assert!(Instant::now() < until, "no picture");
                if let Some(picture) = session.latest_picture(window) {
                    break picture;
                }
            }
        }
    };
    println!("picture: {}x{}", picture.width, picture.height);
    assert_eq!((picture.width, picture.height), (width, height));
    assert!(
        picture.width < 1920 || picture.height < 1080,
        "the whole desktop came"
    );

    // Typing reaches Notepad: it marks its title as changed.
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

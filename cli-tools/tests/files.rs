//! File transfer through both ends of the library (docs/COMMAND-STREAM.md,
//! "Files"): the host application's shares, listing, download, upload and
//! resume, and every way the host refuses.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::TestPatternSource;
use windowcast_client::{Client, ClientError, ClientSession};
use windowcast_host::chat::{ChatHub, ChatSink};
use windowcast_host::command::{CommandAuthorizer, PairedDevices, Principal};
use windowcast_host::files::Shares;
use windowcast_host::{HostConfig, HostControl};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::command::ChannelKind;

fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("windowcast-command-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Rig {
    // Kept alive for the test's length.
    _runtime: tokio::runtime::Runtime,
    control: Arc<HostControl>,
    client: Client,
    address: String,
}

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
        data_dir: host_dir,
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
    }
}

fn connect(rig: &Rig) -> ClientSession {
    rig.client.connect(&rig.address, None).unwrap()
}

fn shares(root: &std::path::Path, writable: bool) -> Shares {
    let mut shares = Shares::default();
    shares.add("saves", root, writable).unwrap();
    shares
}

fn refused(error: ClientError) -> String {
    match error {
        ClientError::Refused(reason) => reason,
        other => panic!("expected a refusal, got {other}"),
    }
}

#[test]
fn a_client_lists_downloads_and_uploads_on_a_share_and_resumes() {
    let rig = rig("transfer");
    let root = temp_dir("transfer-share");
    std::fs::write(root.join("a.sav"), b"saved game").unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    rig.control.set_file_shares(shares(&root, true));
    let session = connect(&rig);
    let files = session.open_files().unwrap();

    let offered = files.shares().unwrap();
    assert_eq!(
        (offered[0].name.as_str(), offered[0].writable),
        ("saves", true)
    );
    let names: Vec<String> = files
        .list("saves", "")
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["a.sav", "sub"]);
    assert_eq!(files.stat("saves", "a.sav").unwrap().size, 10);

    // Download, and a resumed download after a partial one.
    let local = temp_dir("transfer-local");
    assert_eq!(
        files
            .download("saves", "a.sav", &local.join("a.sav"), false)
            .unwrap(),
        10
    );
    assert_eq!(std::fs::read(local.join("a.sav")).unwrap(), b"saved game");
    std::fs::write(local.join("b.sav.wcpart"), b"saved").unwrap();
    files
        .download("saves", "a.sav", &local.join("b.sav"), true)
        .unwrap();
    assert_eq!(std::fs::read(local.join("b.sav")).unwrap(), b"saved game");

    // A file of several windows' worth uploads whole and in place.
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(local.join("big.bin"), &big).unwrap();
    assert_eq!(
        files
            .upload("saves", "sub/big.bin", &local.join("big.bin"), false)
            .unwrap(),
        big.len() as u64
    );
    assert_eq!(std::fs::read(root.join("sub/big.bin")).unwrap(), big);
    assert!(!root.join("sub/big.bin.wcpart").exists());

    // An interrupted upload (a partial file on the host) continues.
    std::fs::write(root.join("r.bin.wcpart"), &big[..777_777]).unwrap();
    std::fs::write(local.join("r.bin"), &big).unwrap();
    files
        .upload("saves", "r.bin", &local.join("r.bin"), true)
        .unwrap();
    assert_eq!(std::fs::read(root.join("r.bin")).unwrap(), big);

    // Nothing escapes the share.
    for path in ["../outside.txt", "/etc/passwd", "sub/../../x"] {
        let reason = refused(
            files
                .download("saves", path, &local.join("x"), false)
                .unwrap_err(),
        );
        assert!(!reason.is_empty(), "{path}");
        assert!(!local.join("x").exists() || std::fs::read(local.join("x")).unwrap().is_empty());
    }
    assert!(refused(
        files
            .upload("saves", "../up.bin", &local.join("a.sav"), false)
            .unwrap_err()
    )
    .contains(".."));
    assert!(!root.parent().unwrap().join("up.bin").exists());
}

#[test]
fn a_read_only_share_refuses_uploads() {
    let rig = rig("readonly");
    let root = temp_dir("readonly-share");
    rig.control.set_file_shares(shares(&root, false));
    let session = connect(&rig);
    let files = session.open_files().unwrap();
    let src = root.join("src.bin");
    std::fs::write(&src, b"x").unwrap();
    let reason = refused(files.upload("saves", "x.bin", &src, false).unwrap_err());
    assert!(reason.contains("read-only"), "{reason}");
    assert!(!root.join("x.bin").exists());
}

#[test]
fn a_host_that_shares_nothing_refuses_the_channel() {
    let rig = rig("noshares");
    let reason = refused(connect(&rig).open_files().err().expect("refused"));
    assert!(reason.contains("shares no folders"), "{reason}");
}

/// Allows a shell but not files: kinds are authorized one by one.
struct NoFiles;

impl CommandAuthorizer for NoFiles {
    fn authorize(&self, _who: &Principal, kind: &ChannelKind) -> Result<(), String> {
        match kind {
            ChannelKind::Files => Err("files are not allowed for this device".into()),
            _ => Ok(()),
        }
    }
}

#[test]
fn the_authorizer_can_refuse_files_alone() {
    let rig = rig("authorizer");
    let root = temp_dir("authorizer-share");
    rig.control.set_file_shares(shares(&root, true));
    rig.control.set_command_authorizer(Arc::new(NoFiles));
    let session = connect(&rig);
    let reason = refused(session.open_files().err().expect("refused"));
    assert!(reason.contains("not allowed"), "{reason}");
    // Other kinds still open.
    let (output, code) = session
        .exec(vec!["echo".into(), "hi".into()], Duration::from_secs(10))
        .unwrap();
    assert_eq!(
        (String::from_utf8_lossy(&output).trim().to_owned(), code),
        ("hi".to_owned(), Some(0))
    );
    rig.control.set_command_authorizer(Arc::new(PairedDevices));
}

fn next_saved(watch: &windowcast_client::files::FolderWatch) -> (String, u64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        match watch.next(Duration::from_millis(250)) {
            Some(windowcast_client::files::WatchEvent::Saved { path, size }) => {
                return (path, size)
            }
            Some(windowcast_client::files::WatchEvent::Failed(reason)) => {
                panic!("watch failed: {reason}")
            }
            _ => {}
        }
    }
    panic!("no file arrived");
}

#[test]
fn a_watched_folder_sends_a_changed_file_once_it_has_settled() {
    use windowcast_client::files::WatchEvent;
    let rig = rig("watch");
    let root = temp_dir("watch-share");
    std::fs::create_dir_all(root.join("profile")).unwrap();
    std::fs::write(root.join("profile/old.sav"), b"already there").unwrap();
    rig.control.set_file_shares(shares(&root, false));
    let session = connect(&rig);
    let dest = temp_dir("watch-dest");

    let watch = session
        .watch_folder("saves", "profile", Some("*.sav"), false, &dest)
        .unwrap();
    assert_eq!(
        watch.next(Duration::from_secs(10)),
        Some(WatchEvent::Started { existing: 1 })
    );

    // A save written while we watch arrives, and not before it has settled.
    std::fs::write(root.join("profile/slot1.sav"), b"first save").unwrap();
    std::fs::write(root.join("profile/notes.txt"), b"not a save").unwrap();
    let started = Instant::now();
    assert_eq!(next_saved(&watch), ("slot1.sav".to_owned(), 10));
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "sent before it had settled"
    );
    assert_eq!(
        std::fs::read(dest.join("slot1.sav")).unwrap(),
        b"first save"
    );
    // The file that was already there, and the one that does not match, were not sent.
    assert!(!dest.join("old.sav").exists() && !dest.join("notes.txt").exists());

    // Saving again sends the new version.
    std::fs::write(root.join("profile/slot1.sav"), b"second, longer save").unwrap();
    assert_eq!(next_saved(&watch), ("slot1.sav".to_owned(), 19));
    assert_eq!(
        std::fs::read(dest.join("slot1.sav")).unwrap(),
        b"second, longer save"
    );

    watch.stop();
    assert_eq!(watch.next(Duration::from_secs(10)), Some(WatchEvent::Ended));
}

#[test]
fn a_watch_can_ask_for_the_files_already_there() {
    let rig = rig("watch-existing");
    let root = temp_dir("watch-existing-share");
    std::fs::write(root.join("a.sav"), b"a").unwrap();
    rig.control.set_file_shares(shares(&root, false));
    let session = connect(&rig);
    let dest = temp_dir("watch-existing-dest");
    let watch = session
        .watch_folder("saves", "", None, true, &dest)
        .unwrap();
    assert_eq!(next_saved(&watch), ("a.sav".to_owned(), 1));
    assert_eq!(std::fs::read(dest.join("a.sav")).unwrap(), b"a");
}

#[test]
fn a_watch_outside_the_share_or_without_shares_is_refused() {
    let rig = rig("watch-refused");
    let session = connect(&rig);
    let dest = temp_dir("watch-refused-dest");
    let reason = refused(
        session
            .watch_folder("saves", "", None, false, &dest)
            .err()
            .expect("no shares"),
    );
    assert!(reason.contains("shares no folders"), "{reason}");

    let root = temp_dir("watch-refused-share");
    rig.control.set_file_shares(shares(&root, false));
    for path in ["../elsewhere", "/etc"] {
        let reason = refused(
            session
                .watch_folder("saves", path, None, false, &dest)
                .err()
                .expect("outside"),
        );
        assert!(!reason.is_empty(), "{path}");
    }
}

#[test]
fn the_authorizer_can_refuse_watching_alone() {
    struct NoWatch;
    impl CommandAuthorizer for NoWatch {
        fn authorize(&self, _who: &Principal, kind: &ChannelKind) -> Result<(), String> {
            match kind {
                ChannelKind::Watch { .. } => Err("watching is not allowed here".into()),
                _ => Ok(()),
            }
        }
    }
    let rig = rig("watch-authorizer");
    let root = temp_dir("watch-authorizer-share");
    rig.control.set_file_shares(shares(&root, false));
    rig.control.set_command_authorizer(Arc::new(NoWatch));
    let session = connect(&rig);
    let dest = temp_dir("watch-authorizer-dest");
    let reason = refused(
        session
            .watch_folder("saves", "", None, false, &dest)
            .err()
            .expect("refused"),
    );
    assert!(reason.contains("not allowed"), "{reason}");
    // Transfers on the same share still open.
    assert!(session.open_files().is_ok());
}

struct Heard(std::sync::Mutex<Vec<String>>);

impl ChatSink for Heard {
    fn received(&self, _from: &Principal, text: &str) {
        self.0.lock().unwrap().push(text.to_owned());
    }
}

#[test]
fn chat_runs_both_ways_between_a_client_and_the_host_application() {
    use windowcast_client::files::ChatEvent;
    let rig = rig("chat");
    let heard = Arc::new(Heard(std::sync::Mutex::new(Vec::new())));
    let hub = ChatHub::new(heard.clone());
    rig.control.set_chat(Some(Arc::clone(&hub)));
    let session = connect(&rig);
    let chat = session.open_chat().unwrap();

    chat.say("hello from the handheld").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while heard.0.lock().unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "the host application heard nothing"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(*heard.0.lock().unwrap(), ["hello from the handheld"]);

    // A notice from the host application.
    assert_eq!(hub.notice("download finished"), Ok(1));
    assert_eq!(
        chat.next(Duration::from_secs(10)),
        Some(ChatEvent::Said("download finished".into()))
    );

    // Bad text is refused before it leaves the client.
    assert!(refused(chat.say("").unwrap_err()).contains("empty"));
    assert!(refused(chat.say(&"x".repeat(3000)).unwrap_err()).contains("at most"));
    assert!(refused(chat.say("\u{1b}[2J").unwrap_err()).contains("control"));

    // And the chat is gone from the hub when the client closes it.
    chat.close();
    let deadline = Instant::now() + Duration::from_secs(10);
    while hub.open_channels() != 0 {
        assert!(Instant::now() < deadline, "the channel stayed open");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_host_without_a_chat_hub_refuses_chat_and_the_authorizer_can_too() {
    let rig = rig("chat-refused");
    let session = connect(&rig);
    let reason = refused(session.open_chat().err().expect("refused"));
    assert!(reason.contains("no chat"), "{reason}");

    struct NoChat;
    impl CommandAuthorizer for NoChat {
        fn authorize(&self, _who: &Principal, kind: &ChannelKind) -> Result<(), String> {
            match kind {
                ChannelKind::Chat => Err("chat is not allowed here".into()),
                _ => Ok(()),
            }
        }
    }
    rig.control
        .set_chat(Some(ChatHub::new(Arc::new(Heard(std::sync::Mutex::new(
            Vec::new(),
        ))))));
    rig.control.set_command_authorizer(Arc::new(NoChat));
    let session = connect(&rig);
    let reason = refused(session.open_chat().err().expect("refused"));
    assert!(reason.contains("not allowed"), "{reason}");
}

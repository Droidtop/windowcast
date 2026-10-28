//! File transfer through both ends of the library (docs/COMMAND-STREAM.md,
//! "Files"): the host application's shares, listing, download, upload and
//! resume, and every way the host refuses.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use windowcast_cli_tools::testpattern::TestPatternSource;
use windowcast_client::{Client, ClientError, ClientSession};
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

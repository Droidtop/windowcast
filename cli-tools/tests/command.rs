//! The command stream through both ends of the library: a shell with a
//! real PTY on the host, drawn by the client's screen model; a command with
//! its output and exit code; an application launch; the authorizer hook
//! refusing; and the host cleaning up when the client goes.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windowcast_cli_tools::testpattern::TestPatternSource;
use windowcast_client::{Client, ClientError, ClientSession};
use windowcast_host::command::{NoCommands, PairedDevices};
use windowcast_host::{HostConfig, HostControl};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::command::TerminalSize;
use windowcast_terminal::Terminal;

const WAIT: Duration = Duration::from_secs(20);

fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("windowcast-command-{name}-{}", std::process::id()));
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

fn screen_text(terminal: &Terminal) -> String {
    terminal
        .snapshot()
        .lines
        .iter()
        .map(|line| line.iter().map(|r| r.text.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn wait_for_screen(terminal: &Terminal, needle: &str) {
    wait_for(&format!("{needle:?} on the screen"), || {
        screen_text(terminal).contains(needle)
    });
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

#[test]
fn a_shell_runs_on_the_host_and_is_drawn_by_the_client() {
    let rig = rig("shell");
    let session = connect(&rig);
    let terminal = session
        .open_terminal(TerminalSize {
            cols: 100,
            rows: 30,
        })
        .unwrap();

    wait_for("the host to list the shell", || {
        rig.control.commands().len() == 1
    });
    assert_eq!(rig.control.commands()[0].what, "shell");

    terminal.send_text("echo hi-$((20+22)); stty size\r");
    wait_for_screen(&terminal, "hi-42");
    wait_for_screen(&terminal, "30 100");

    terminal.resize(TerminalSize { cols: 61, rows: 17 });
    terminal.send_text("stty size\r");
    wait_for_screen(&terminal, "17 61");
    assert_eq!(terminal.snapshot().cols, 61);

    // A program sets the clipboard in the stream.
    terminal.send_text("printf '\\033]52;c;Y29waWVk\\007'\r");
    wait_for("the clipboard text", || {
        let text = terminal.take_clipboard();
        text == ["copied"]
    });

    terminal.send_text("exit 4\r");
    wait_for("the shell to end", || terminal.ended().is_some());
    assert_eq!(terminal.ended(), Some(Some(4)));
    wait_for("the host to forget the shell", || {
        rig.control.commands().is_empty()
    });
}

#[test]
fn commands_run_with_their_output_and_applications_launch() {
    let rig = rig("exec");
    let session = connect(&rig);

    let (output, code) = session
        .exec(
            vec![
                "sh".into(),
                "-c".into(),
                "echo out-$((6*7)); echo err >&2; exit 3".into(),
            ],
            WAIT,
        )
        .unwrap();
    let output = String::from_utf8_lossy(&output);
    assert!(
        output.contains("out-42") && output.contains("err"),
        "{output:?}"
    );
    assert_eq!(code, Some(3));

    let marker = std::env::temp_dir().join(format!("wc-launch-{}", std::process::id()));
    let pid = session
        .launch(vec!["touch".into(), marker.to_string_lossy().into_owned()])
        .unwrap();
    assert!(pid.is_some_and(|pid| pid > 1));
    wait_for("the application to run", || marker.exists());
    std::fs::remove_file(marker).unwrap();

    let missing = session.launch(vec!["/no/such/application".into()]);
    assert!(
        matches!(missing, Err(ClientError::Refused(_))),
        "{missing:?}"
    );
}

#[test]
fn the_authorizer_decides_and_the_host_cleans_up() {
    let rig = rig("authorize");
    rig.control.set_command_authorizer(Arc::new(NoCommands));
    let session = connect(&rig);
    let refused = session.open_terminal(TerminalSize { cols: 80, rows: 24 });
    match refused {
        Err(ClientError::Refused(reason)) => assert!(reason.contains("does not allow"), "{reason}"),
        other => panic!("expected a refusal, got {:?}", other.err()),
    }
    assert!(rig.control.commands().is_empty());

    // The check is read when a client connects: a new session sees the
    // paired-devices default again.
    rig.control.set_command_authorizer(Arc::new(PairedDevices));
    let second = connect(&rig);
    let terminal = second
        .open_terminal(TerminalSize { cols: 80, rows: 24 })
        .unwrap();
    wait_for("the shell", || rig.control.commands().len() == 1);

    // The client goes; the shell must not outlive its session.
    drop(terminal);
    drop(second);
    wait_for("the host to end the shell with the session", || {
        rig.control.commands().is_empty()
    });
}

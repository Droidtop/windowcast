//! What a client starts runs as the signed-in user, never as the host's
//! own SYSTEM account (docs/BACKENDS.md, "The secure screen";
//! `user_process`). The runner is not SYSTEM, so the console user's token
//! is refused here, which shows the token path is the one taken and that
//! nothing starts without it; the starting itself (CreateProcessAsUserW,
//! the user's environment, pipes, the terminal relay) runs with this
//! process's own token through the same code.

#![cfg(windows)]

use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windowcast_agent_windows::user_process::{self, UserProcesses, NOBODY_SIGNED_IN};
use windowcast_host::command::{ProcessStarter, Started};

fn relay() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_windowcast-agent-windows"))
}

fn strings(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|s| s.to_string()).collect()
}

/// Everything the command wrote, both outputs read at once.
fn output(started: Started) -> (String, Option<i32>) {
    let readers: Vec<_> = started
        .output
        .into_iter()
        .map(|mut reader| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = reader.read_to_end(&mut bytes);
                String::from_utf8_lossy(&bytes).into_owned()
            })
        })
        .collect();
    let code = started.process.wait();
    let text = readers.into_iter().map(|r| r.join().unwrap()).collect();
    (text, code)
}

#[test]
fn without_the_signed_in_users_token_nothing_starts() {
    assert!(
        !user_process::running_as_system(),
        "this test needs a runner that is not SYSTEM"
    );
    let marker = std::env::temp_dir().join("windowcast-user-process-marker");
    let _ = std::fs::remove_file(&marker);
    let write_marker = strings(&["cmd.exe", "/c", &format!("echo x> {}", marker.display())]);
    let starter = UserProcesses::console_user(relay());

    let refusals = [
        starter
            .piped(&write_marker, &[])
            .err()
            .expect("piped started"),
        starter
            .detached(&write_marker, &[])
            .expect_err("detached started"),
        starter
            .terminal(&windowcast_pty::Spawn {
                size: windowcast_pty::Size { cols: 80, rows: 24 },
                term: String::new(),
                program: Some(write_marker.clone()),
                env: Vec::new(),
            })
            .err()
            .expect("terminal started"),
    ];
    for refusal in &refusals {
        assert!(
            refusal == NOBODY_SIGNED_IN || refusal.contains("signed-in user's token"),
            "{refusal}"
        );
    }
    std::thread::sleep(Duration::from_millis(500));
    assert!(!marker.exists(), "something ran without the user's token");
}

#[test]
fn a_command_gets_its_input_the_users_environment_and_the_extra_variables() {
    let starter = UserProcesses::own_user(relay());
    let started = starter
        .piped(
            &strings(&[
                "cmd.exe",
                "/c",
                "echo %WINDOWCAST_TEST_VAR% %USERPROFILE%& exit /b 3",
            ]),
            &[("WINDOWCAST_TEST_VAR".into(), "from-the-host".into())],
        )
        .unwrap();
    assert!(started.pid.is_some());
    let (text, code) = output(started);
    assert!(text.contains("from-the-host"), "{text:?}");
    let profile = std::env::var("USERPROFILE").unwrap();
    assert!(text.contains(&profile), "{text:?}");
    assert_eq!(code, Some(3));

    let started = starter.piped(&strings(&["sort.exe"]), &[]).unwrap();
    started.process.write(b"pear\r\napple\r\n").unwrap();
    started.process.eof();
    let (text, code) = output(started);
    assert_eq!(
        text.split_whitespace().collect::<Vec<_>>(),
        ["apple", "pear"]
    );
    assert_eq!(code, Some(0));
}

#[test]
fn an_application_starts_detached() {
    let marker = std::env::temp_dir().join("windowcast-user-process-detached");
    let _ = std::fs::remove_file(&marker);
    let starter = UserProcesses::own_user(relay());
    let pid = starter
        .detached(
            &strings(&["cmd.exe", "/c", &format!("echo x> {}", marker.display())]),
            &[],
        )
        .unwrap();
    assert!(pid.is_some());
    let since = Instant::now();
    while !marker.exists() {
        assert!(since.elapsed() < Duration::from_secs(10), "it did not run");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_terminal_runs_through_the_relay_and_ends_with_its_code() {
    let starter = UserProcesses::own_user(relay());
    let started = starter
        .terminal(&windowcast_pty::Spawn {
            size: windowcast_pty::Size {
                cols: 100,
                rows: 30,
            },
            term: "xterm-256color".into(),
            program: Some(strings(&["cmd.exe"])),
            env: vec![("WINDOWCAST_TEST_VAR".into(), "relayed".into())],
        })
        .unwrap();
    let process = started.process;
    let (seen_tx, seen_rx) = std::sync::mpsc::channel::<String>();
    for mut reader in started.output {
        let seen_tx = seen_tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0
                    || seen_tx
                        .send(String::from_utf8_lossy(&buf[..n]).into_owned())
                        .is_err()
                {
                    return;
                }
            }
        });
    }
    process
        .write(b"echo terminal-%WINDOWCAST_TEST_VAR%\r\n")
        .unwrap();
    let mut seen = String::new();
    let mut answered = false;
    let since = Instant::now();
    while !seen.contains("terminal-relayed") {
        assert!(
            since.elapsed() < Duration::from_secs(20),
            "no echo in {seen:?}"
        );
        if let Ok(text) = seen_rx.recv_timeout(Duration::from_millis(200)) {
            seen.push_str(&text);
        }
        // ConPTY asks where the cursor is before it shows anything and
        // waits for the answer, which a client's terminal gives.
        if !answered && seen.contains("\x1b[6n") {
            process.write(b"\x1b[1;1R").unwrap();
            answered = true;
        }
    }
    process.resize(windowcast_protocol::command::TerminalSize { cols: 70, rows: 20 });
    process.write(b"exit 4\r\n").unwrap();
    assert_eq!(process.wait(), Some(4));
}

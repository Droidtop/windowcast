//! Account sign-in end to end through the library's two ends
//! (docs/ACCOUNTS.md): a host with sign-in on serving the test pattern,
//! clients signing in, resuming with the registration, policy narrowing
//! what they see and which commands they may run, and an SSH certificate
//! for a signed-in account. With WINDOWCAST_TEST_OIDC set to a Dex issuer
//! the CI runs on loopback (a public client `windowcast` and the mock
//! connector), the OpenID Connect flows too, the device flow through the C
//! interface.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use windowcast_accounts::{oidc::ProviderConfig, Config, Policy, Rule};
use windowcast_cli_tools::testpattern::{TestPatternSource, WINDOW};
use windowcast_client::{Client, ClientError, ClientSession, Event, SignIn};
use windowcast_host::command::{NoCommands, PairedDevices};
use windowcast_host::{HostConfig, HostControl};
use windowcast_identity::{Identity, TrustStore};
use windowcast_protocol::{SignInMethod, VideoCodec};

const WAIT: Duration = Duration::from_secs(20);

fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("windowcast-accounts-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A host on loopback with `accounts`, serving the test pattern.
fn host(
    runtime: &tokio::runtime::Runtime,
    name: &str,
    accounts: Config,
) -> (Arc<HostControl>, String, PathBuf) {
    host_in(runtime, temp_dir(name), accounts)
}

/// As [`host`], in a data folder the caller has prepared.
fn host_in(
    runtime: &tokio::runtime::Runtime,
    dir: PathBuf,
    accounts: Config,
) -> (Arc<HostControl>, String, PathBuf) {
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let control = HostControl::open(&HostConfig {
        listen: address.clone(),
        pairing: false,
        data_dir: dir.clone(),
    })
    .unwrap();
    control.set_accounts(Some(accounts)).unwrap();
    runtime.spawn(windowcast_host::serve_with(
        listener,
        Arc::clone(&control),
        Arc::new(TestPatternSource),
    ));
    (control, address, dir)
}

fn password(username: &str, password: &str) -> SignIn {
    SignIn::Password {
        username: username.into(),
        password: password.into(),
    }
}

fn windows(session: &windowcast_client::ClientSession) -> Vec<u64> {
    session.request_windows().unwrap();
    loop {
        match session.next_event(WAIT) {
            Some(Event::Windows { windows }) => return windows.iter().map(|w| w.id.0).collect(),
            Some(_) => continue,
            None => panic!("no window list"),
        }
    }
}

#[test]
fn local_accounts_sign_in_register_and_follow_policy() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let config = Config {
        ssh_ca: Some("ssh-user-ca".into()),
        policy: Policy {
            rules: vec![
                Rule {
                    who: vec!["user:alice".into()],
                    commands: Some(true),
                    ..Rule::default()
                },
                Rule {
                    who: vec!["group:viewers".into()],
                    windows: vec!["something else".into()],
                    ..Rule::default()
                },
            ],
        },
        ..Config::default()
    };
    let (control, address, host_dir) = host(&runtime, "local-host", config);
    let accounts = control.accounts().unwrap();
    accounts
        .local()
        .create("alice", "alice-password", &[])
        .unwrap();
    accounts
        .local()
        .create("bob", "bob-password", &["viewers".into()])
        .unwrap();
    accounts
        .local()
        .create("carol", "carol-password", &[])
        .unwrap();
    accounts.save_local().unwrap();

    let client_dir = temp_dir("local-client");
    let client = Client::new(&client_dir).unwrap();

    let options = client.sign_in_options(&address).unwrap();
    assert!(!options.trusted);
    assert_eq!(options.methods, vec![SignInMethod::Password]);
    assert_eq!(options.host_id, control.peer_id().to_hex());

    // A host the client does not know: nothing is sent.
    match client.connect_account(&address, &password("alice", "alice-password"), None) {
        Err(ClientError::HostNotTrusted(id)) => assert_eq!(id, options.host_id),
        Err(e) => panic!("expected HostNotTrusted, got {e}"),
        Ok(_) => panic!("expected HostNotTrusted, got a session"),
    }
    // Wrong password, and an account policy has no rule for.
    for (user, pass) in [("alice", "wrong"), ("carol", "carol-password")] {
        assert!(client
            .connect_account(
                &address,
                &password(user, pass),
                Some(options.host_id.as_str())
            )
            .is_err());
    }

    let session = client
        .connect_account(
            &address,
            &password("alice", "alice-password"),
            Some(options.host_id.as_str()),
        )
        .unwrap();
    assert_eq!(windows(&session), vec![WINDOW.0]);
    let registered = control.registered();
    assert_eq!(registered.len(), 1);
    assert_eq!(registered[0].1.account.name, "alice");
    assert_eq!(
        control.account_of(&registered[0].0).map(|a| a.name),
        Some("alice".to_owned())
    );

    // An SSH certificate for alice, for this client's own SSH key, which
    // then logs in with it (SshAuth::Certificate).
    let public_key = client.ssh_public_key().unwrap();
    assert!(public_key.starts_with("ssh-ed25519 "), "{public_key}");
    assert_eq!(client.ssh_public_key().unwrap(), public_key);
    session.request_ssh_certificate(&public_key).unwrap();
    loop {
        match session.next_event(WAIT) {
            Some(Event::SshCertificate { certificate, error }) => {
                let certificate = certificate.unwrap_or_else(|| panic!("{error:?}"));
                assert!(certificate.starts_with("ssh-ed25519-cert-v01@openssh.com "));
                assert!(client.ssh_certificate_auth(&certificate).is_ok());
                break;
            }
            Some(_) => continue,
            None => panic!("no SSH certificate"),
        }
    }
    assert!(client.ssh_certificate_auth("not a certificate").is_err());
    drop(session);

    // Later: the registration resumes with the device key alone, and the
    // host is trusted now.
    assert!(client.sign_in_options(&address).unwrap().trusted);
    let session = client.connect(&address, None).unwrap();
    assert_eq!(windows(&session), vec![WINDOW.0]);
    drop(session);

    // Bob's policy shows him no window and refuses the stream.
    let bob_dir = temp_dir("local-bob");
    let bob = Client::new(&bob_dir).unwrap();
    let session = bob
        .connect_account(
            &address,
            &password("bob", "bob-password"),
            Some(options.host_id.as_str()),
        )
        .unwrap();
    assert!(windows(&session).is_empty());
    session.start_window(WINDOW, &[VideoCodec::H264]).unwrap();
    loop {
        match session.next_event(WAIT) {
            Some(Event::StreamRefused { .. }) => break,
            Some(Event::StreamStarted { .. }) => panic!("bob may not stream the window"),
            Some(_) => continue,
            None => panic!("no answer to the stream request"),
        }
    }
    drop(session);

    // Removing alice's account stops her device at its next connection.
    accounts.local().remove("alice").unwrap();
    assert!(client.connect(&address, None).is_err());

    drop(client);
    drop(bob);
    runtime.shutdown_timeout(Duration::from_secs(1));
    for dir in [host_dir, client_dir, bob_dir] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Runs `true` on the host over the command stream: `Ok` when the host
/// let it run, the refusal otherwise.
fn run_command(session: &ClientSession) -> Result<(), String> {
    match session.exec(vec!["true".into()], WAIT) {
        Ok((_, code)) => {
            assert_eq!(code, Some(0));
            Ok(())
        }
        Err(ClientError::Refused(reason)) => Err(reason),
        Err(e) => panic!("the command did not run or get refused: {e}"),
    }
}

/// Writes trust files so that a client in `client_dir` is paired (as by
/// PIN) with a host in `host_dir`.
fn pair(host_dir: &Path, client_dir: &Path) {
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
}

#[test]
fn account_policy_decides_the_command_stream() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let config = Config {
        policy: Policy {
            rules: vec![
                Rule {
                    who: vec!["user:alice".into()],
                    commands: Some(true),
                    ..Rule::default()
                },
                Rule {
                    who: vec!["user:dave".into()],
                    commands: Some(false),
                    ..Rule::default()
                },
                Rule {
                    who: vec!["method:pin".into()],
                    commands: Some(false),
                    ..Rule::default()
                },
                // Staff: policy says nothing about commands.
                Rule {
                    who: vec!["group:staff".into()],
                    ..Rule::default()
                },
            ],
        },
        ..Config::default()
    };
    let host_dir = temp_dir("commands-host");
    let paired_dir = temp_dir("commands-paired");
    pair(&host_dir, &paired_dir);
    let (control, address, host_dir) = host_in(&runtime, host_dir, config);
    let accounts = control.accounts().unwrap();
    for (user, groups) in [
        ("alice", Vec::<String>::new()),
        ("dave", Vec::new()),
        ("erin", vec!["staff".to_owned()]),
    ] {
        accounts
            .local()
            .create(user, &format!("{user}-password"), &groups)
            .unwrap();
    }
    accounts.save_local().unwrap();
    let host_id = control.peer_id().to_hex();

    // The host's own check refuses everything; policy is asked first.
    control.set_command_authorizer(Arc::new(NoCommands));
    let mut dirs = vec![host_dir, paired_dir.clone()];
    let mut sign_in = |user: &str| {
        let dir = temp_dir(&format!("commands-{user}"));
        dirs.push(dir.clone());
        let client = Client::new(&dir).unwrap();
        let session = client
            .connect_account(
                &address,
                &password(user, &format!("{user}-password")),
                Some(host_id.as_str()),
            )
            .unwrap();
        (client, session)
    };
    let (_alice, alice_session) = sign_in("alice");
    let (_dave, dave_session) = sign_in("dave");
    let (erin, erin_session) = sign_in("erin");
    let paired = Client::new(&paired_dir).unwrap();
    let paired_session = paired.connect(&address, None).unwrap();

    // Policy says yes for alice, past the host's own refusal.
    assert_eq!(run_command(&alice_session), Ok(()));
    // Policy says no for dave and for a device paired by PIN.
    for session in [&dave_session, &paired_session] {
        let reason = run_command(session).unwrap_err();
        assert!(reason.contains("policy"), "{reason}");
    }
    // Policy leaves erin to the host's own check, which says no...
    let reason = run_command(&erin_session).unwrap_err();
    assert!(reason.contains("does not allow"), "{reason}");
    drop(erin_session);
    // ...until the host allows paired devices again (read per session).
    control.set_command_authorizer(Arc::new(PairedDevices));
    let erin_session = erin.connect(&address, None).unwrap();
    assert_eq!(run_command(&erin_session), Ok(()));
    drop((alice_session, dave_session, erin_session, paired_session));

    // Sign-in off: a paired device is under the host's own check alone,
    // as before accounts.
    control.set_accounts(None).unwrap();
    let paired_session = paired.connect(&address, None).unwrap();
    assert_eq!(run_command(&paired_session), Ok(()));
    drop(paired_session);

    drop(paired);
    drop(erin);
    runtime.shutdown_timeout(Duration::from_secs(1));
    for dir in dirs {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Follows the authorization page as a browser would: Dex's mock connector
/// signs in without a form and sends the browser back to the client's
/// loopback port. Each hop is logged, so a provider that stops somewhere
/// says where.
fn browse(url: &str) {
    follow(agent().get(url));
}

/// Sends `request`, then follows redirects by hand (logging each).
fn follow(request: ureq::Request) {
    follow_with(request, None)
}

fn follow_with(request: ureq::Request, form: Option<&[(&str, &str)]>) {
    let mut current = url::Url::parse(request.url()).unwrap();
    let mut response = match form {
        Some(form) => request.send_form(form),
        None => request.call(),
    };
    for _ in 0..12 {
        let answer = match response {
            Ok(answer) => answer,
            Err(ureq::Error::Status(_, answer)) => answer,
            Err(e) => {
                eprintln!("browser: {current}: {e}");
                return;
            }
        };
        let status = answer.status();
        match answer.header("location") {
            Some(location) if (300..400).contains(&status) => {
                current = current.join(location).unwrap();
                eprintln!("browser: {status} -> {current}");
                response = agent().get(current.as_str()).call();
            }
            _ => {
                // The page's text without its markup.
                let body = answer.into_string().unwrap_or_default();
                let mut text = String::new();
                let mut in_tag = false;
                for c in body.chars() {
                    match c {
                        '<' => in_tag = true,
                        '>' => {
                            in_tag = false;
                            text.push(' ');
                        }
                        _ if !in_tag => text.push(c),
                        _ => {}
                    }
                }
                let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                eprintln!("browser: {status} at {current}: {text}");
                return;
            }
        }
    }
}

/// Redirects are followed by hand, to log them.
fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_secs(20))
        .build()
}

#[test]
fn openid_connect_sign_in_against_a_local_dex() {
    let Ok(issuer) = std::env::var("WINDOWCAST_TEST_OIDC") else {
        eprintln!("WINDOWCAST_TEST_OIDC not set; skipping");
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let config = Config {
        password: Vec::new(),
        oidc: vec![ProviderConfig {
            name: "dex".into(),
            issuer,
            client_id: "windowcast".into(),
            scopes: vec!["email".into(), "profile".into(), "groups".into()],
            ..ProviderConfig::default()
        }],
        ..Config::default()
    };
    let (control, address, host_dir) = host(&runtime, "oidc-host", config);

    let client_dir = temp_dir("oidc-client");
    let client = Client::new(&client_dir).unwrap();
    let options = client.sign_in_options(&address).unwrap();
    assert_eq!(options.methods, vec![SignInMethod::Oidc]);
    let provider = options.providers[0].clone();

    // Authorization code with PKCE, through the loopback redirect.
    let browser = client.oidc_browser(&provider).unwrap();
    let url = browser.url().to_owned();
    let browsing = std::thread::spawn(move || browse(&url));
    let id_token = browser.finish(Duration::from_secs(30)).unwrap();
    browsing.join().unwrap();

    // The token's nonce is this client's: another device cannot use it.
    let other_dir = temp_dir("oidc-other");
    let other = Client::new(&other_dir).unwrap();
    let stolen = SignIn::Oidc {
        provider: provider.name.clone(),
        id_token: id_token.clone(),
    };
    assert!(other
        .connect_account(&address, &stolen, Some(options.host_id.as_str()))
        .is_err());

    let session = client
        .connect_account(&address, &stolen, Some(options.host_id.as_str()))
        .unwrap();
    assert_eq!(windows(&session), vec![WINDOW.0]);
    drop(session);
    let registered = control.registered();
    assert_eq!(registered.len(), 1);
    let account = &registered[0].1.account;
    assert_eq!(account.provider, "dex");
    assert!(!account.name.is_empty());
    eprintln!("signed in as {account:?}");

    // Each token registers one device: presenting it again is refused.
    assert!(client
        .connect_account(&address, &stolen, Some(options.host_id.as_str()))
        .is_err());

    // The device flow through the C interface, finished "on another
    // device".
    use std::ffi::{c_char, CStr, CString};
    use windowcast_client::ffi::{
        windowcast_oidc_device_free, windowcast_oidc_device_start, windowcast_oidc_device_wait,
        WINDOWCAST_TIMEOUT,
    };
    let provider_json = CString::new(serde_json::to_string(&provider).unwrap()).unwrap();
    let mut shown = vec![0 as c_char; 4096];
    let device = unsafe {
        windowcast_oidc_device_start(&other, provider_json.as_ptr(), shown.as_mut_ptr(), 4096)
    };
    let shown = unsafe { CStr::from_ptr(shown.as_ptr()) }
        .to_str()
        .unwrap()
        .to_owned();
    assert!(!device.is_null(), "{shown}");
    let shown: serde_json::Value = serde_json::from_str(&shown).unwrap();
    let user_code = shown["user_code"].as_str().unwrap().to_owned();
    let verify = shown["verification_uri_complete"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "{}?user_code={user_code}",
                shown["verification_uri"].as_str().unwrap()
            )
        });
    // Nobody has signed in yet.
    let mut token = vec![0 as c_char; 16 * 1024];
    assert_eq!(
        unsafe { windowcast_oidc_device_wait(device, 0, token.as_mut_ptr(), token.len()) },
        WINDOWCAST_TIMEOUT
    );
    let browsing = std::thread::spawn(move || {
        // Dex asks for the code on a form; post it the way the page does.
        browse(&verify);
        let action = verify
            .split('?')
            .next()
            .unwrap()
            .trim_end_matches('/')
            .to_owned()
            + "/auth/verify_code";
        follow_with(
            agent().post(&action),
            Some(&[("user_code", user_code.as_str())]),
        );
    });
    let id_token = loop {
        let n =
            unsafe { windowcast_oidc_device_wait(device, 1000, token.as_mut_ptr(), token.len()) };
        if n == WINDOWCAST_TIMEOUT {
            continue;
        }
        let text = unsafe { CStr::from_ptr(token.as_ptr()) }
            .to_str()
            .unwrap()
            .to_owned();
        assert!(n > 0, "device sign-in: {n} {text}");
        assert_eq!(n as usize, text.len());
        break text;
    };
    unsafe { windowcast_oidc_device_free(device) };
    browsing.join().unwrap();
    let session = other
        .connect_account(
            &address,
            &SignIn::Oidc {
                provider: provider.name.clone(),
                id_token,
            },
            Some(options.host_id.as_str()),
        )
        .unwrap();
    assert_eq!(windows(&session), vec![WINDOW.0]);
    drop(session);
    assert_eq!(control.registered().len(), 2);

    drop(client);
    drop(other);
    runtime.shutdown_timeout(Duration::from_secs(1));
    for dir in [host_dir, client_dir, other_dir] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

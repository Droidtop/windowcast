//! Account sign-in end to end through the library's two ends
//! (docs/ACCOUNTS.md): a host with sign-in on serving the test pattern,
//! clients signing in, resuming with the registration, policy narrowing
//! what they see, and an SSH certificate for a signed-in account. With
//! WINDOWCAST_TEST_OIDC set to a Dex issuer the CI runs on loopback (a
//! public client `windowcast` and the mock connector), the OpenID Connect
//! flows too.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use windowcast_accounts::{oidc::ProviderConfig, Config, Policy, Rule};
use windowcast_cli_tools::testpattern::{TestPatternSource, WINDOW};
use windowcast_client::{Client, ClientError, Event, SignIn};
use windowcast_host::{HostConfig, HostControl};
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
    let dir = temp_dir(name);
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

    // An SSH certificate for alice.
    session
        .request_ssh_certificate(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8g test",
        )
        .unwrap();
    loop {
        match session.next_event(WAIT) {
            Some(Event::SshCertificate { certificate, error }) => {
                let certificate = certificate.unwrap_or_else(|| panic!("{error:?}"));
                assert!(certificate.starts_with("ssh-ed25519-cert-v01@openssh.com "));
                break;
            }
            Some(_) => continue,
            None => panic!("no SSH certificate"),
        }
    }
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

    // The device flow, finished "on another device".
    let device = other.oidc_device(&provider).unwrap();
    let verify = device
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| format!("{}?user_code={}", device.verification_uri, device.user_code));
    let user_code = device.user_code.clone();
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
    let id_token = device.finish().unwrap();
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

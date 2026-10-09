//! Our GameStream client against our GameStream host over real HTTP and
//! HTTPS on loopback: serverinfo before and after, pairing with the PIN
//! the client shows typed on the host, the app list with the pinned
//! certificates, a wrong PIN, and a stranger's certificate refused.

use std::sync::Arc;
use std::time::Duration;

use windowcast_gamestream::client::{App, GameStreamClient, Host};
use windowcast_gamestream::crypto::Credentials;
use windowcast_gamestream::server::GameStreamServer;
use windowcast_gamestream::GameStreamError;

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("windowcast-gs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_client_pairs_and_lists_apps_and_strangers_are_refused() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let dir = temp_dir("host");
    let server = GameStreamServer::open(
        "test host",
        &dir,
        Box::new(|| {
            vec![
                App {
                    id: 1,
                    title: "Desktop".into(),
                    hdr: false,
                },
                App {
                    id: 7,
                    title: "A window & more".into(),
                    hdr: false,
                },
            ]
        }),
    )
    .unwrap();
    let (http, https) = runtime.block_on(async {
        (
            tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
            tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
        )
    });
    let (http_port, https_port) = (
        http.local_addr().unwrap().port(),
        https.local_addr().unwrap().port(),
    );
    runtime.spawn(Arc::clone(&server).serve(http, https));

    let client = GameStreamClient::new(Credentials::generate("NVIDIA GameStream Client").unwrap());
    let host = Host {
        address: "127.0.0.1".into(),
        http_port,
        https_port: 0,
        cert: None,
    };
    let info = client.server_info(&host).unwrap();
    assert_eq!(info.hostname, "test host");
    assert!(!info.paired);

    // The person reads the PIN off the client and types it on the host.
    let typist = {
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            for _ in 0..100 {
                if !server.pairing_requests().is_empty() {
                    return server.enter_pin("5309", None);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            false
        })
    };
    let paired = client.pair(&host, "5309", "test client").unwrap();
    assert!(typist.join().unwrap());
    assert_eq!(paired.https_port, https_port);
    assert_eq!(server.paired_clients(), 1);
    let apps = client.apps(&paired).unwrap();
    assert_eq!(apps.len(), 2);
    assert_eq!(apps[1].title, "A window & more");

    // A stranger with its own certificate gets no app list.
    let stranger =
        GameStreamClient::new(Credentials::generate("NVIDIA GameStream Client").unwrap());
    let refused = stranger.apps(&paired);
    assert!(
        matches!(refused, Err(GameStreamError::Status(401, _))),
        "{refused:?}"
    );

    // A wrong PIN pairs nobody.
    let typist = {
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            for _ in 0..100 {
                if !server.pairing_requests().is_empty() {
                    return server.enter_pin("1111", None);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            false
        })
    };
    let wrong = stranger.pair(&host, "2222", "stranger");
    assert!(typist.join().unwrap());
    assert!(matches!(wrong, Err(GameStreamError::WrongPin)), "{wrong:?}");
    assert_eq!(server.paired_clients(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

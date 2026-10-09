//! A host that streams a test pattern instead of real windows: for trying
//! a client (the Android viewer, droidtop) end to end on any machine,
//! Windows included, before that machine's capture exists. Pairing,
//! sessions and streams are the real host library.
//!
//! Usage: `windowcast-testhost [--listen ADDR:PORT] [--no-pairing]`

use std::sync::Arc;

use windowcast_cli_tools::testpattern::TestPatternSource;
use windowcast_host::{HostConfig, DEFAULT_LISTEN};

#[tokio::main]
async fn main() {
    let mut listen = DEFAULT_LISTEN.to_owned();
    let mut pairing = true;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next().expect("--listen needs ADDR:PORT"),
            "--no-pairing" => pairing = false,
            other => panic!("unknown argument {other}"),
        }
    }
    let config = HostConfig {
        listen,
        pairing,
        data_dir: windowcast_cli_tools::data_dir().join("testhost"),
    };
    if let Err(e) = windowcast_host::run(config, Arc::new(TestPatternSource)).await {
        eprintln!("host stopped: {e}");
        std::process::exit(1);
    }
}

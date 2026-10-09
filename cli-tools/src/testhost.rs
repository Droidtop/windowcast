//! A host that streams a test pattern instead of real windows: for trying
//! a client (the Android viewer, droidtop) end to end on any machine,
//! Windows included, before that machine's capture exists. Pairing,
//! sessions and streams are the real host library.
//!
//! Usage: `windowcast-testhost [--listen ADDR:PORT] [--no-pairing] [--tone]`
//! (`--tone`: the window also sounds a 440 Hz tone, for trying a client's
//! audio).

use std::sync::Arc;

use windowcast_cli_tools::testpattern::{TestPatternSource, TestPatternWithTone};
use windowcast_host::{HostConfig, WindowSource, DEFAULT_LISTEN};

#[tokio::main]
async fn main() {
    let mut listen = DEFAULT_LISTEN.to_owned();
    let mut pairing = true;
    let mut tone = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next().expect("--listen needs ADDR:PORT"),
            "--no-pairing" => pairing = false,
            "--tone" => tone = true,
            other => panic!("unknown argument {other}"),
        }
    }
    let config = HostConfig {
        listen,
        pairing,
        data_dir: windowcast_cli_tools::data_dir().join("testhost"),
    };
    let source: Arc<dyn WindowSource> = if tone {
        Arc::new(TestPatternWithTone)
    } else {
        Arc::new(TestPatternSource)
    };
    if let Err(e) = windowcast_host::run(config, source).await {
        eprintln!("host stopped: {e}");
        std::process::exit(1);
    }
}

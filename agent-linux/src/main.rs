//! windowcast Linux/Wayland host agent.
//!
//! Usage: `windowcast-agent-linux [--listen ADDR:PORT] [--no-pairing]
//! [--fps N] [--bitrate BPS_AT_1080P]` (`--listen` defaults to
//! 0.0.0.0:47100). Runs inside the Wayland session it streams from
//! (`WAYLAND_DISPLAY`; `SWAYSOCK` for input and the desktop backend).

use std::path::PathBuf;
use std::sync::Arc;

use windowcast_agent_linux::{LinuxSource, Options};
use windowcast_host::{HostConfig, DEFAULT_LISTEN};

#[tokio::main]
async fn main() {
    let mut listen = DEFAULT_LISTEN.to_owned();
    let mut pairing = true;
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .unwrap_or_else(|| panic!("{name} needs a value"))
        };
        match arg.as_str() {
            "--listen" => listen = value("--listen"),
            "--no-pairing" => pairing = false,
            "--fps" => options.fps = value("--fps").parse().expect("--fps needs a number"),
            "--bitrate" => {
                options.bitrate_1080p = value("--bitrate")
                    .parse()
                    .expect("--bitrate needs a number")
            }
            other => panic!("unknown argument {other}"),
        }
    }
    let config = HostConfig {
        listen,
        pairing,
        data_dir: data_dir(),
    };
    if let Err(e) = windowcast_host::run(config, Arc::new(LinuxSource::new(options))).await {
        eprintln!("host stopped: {e}");
        std::process::exit(1);
    }
}

fn data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").expect("HOME must be set")).join(".local/share")
        });
    base.join("windowcast")
}

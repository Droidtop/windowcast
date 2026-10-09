//! windowcast Linux/Wayland host agent: the Linux end of the windowcast
//! library. `windowcast-host` does the serving (pairing, sessions,
//! streams); this agent supplies the compositor's windows. It does not
//! capture windows yet — see `capture.rs` for exactly what's still missing
//! and why.
//!
//! Usage: `windowcast-agent-linux [--listen ADDR:PORT] [--no-pairing]`
//! (`--listen` defaults to 0.0.0.0:47100).

mod capture;
mod toplevels;

use std::path::PathBuf;
use std::sync::Arc;

use windowcast_host::{FrameSource, HostConfig, WindowSource, DEFAULT_LISTEN};
use windowcast_protocol::{VideoCodec, WindowId, WindowInfo};

struct LinuxWindows;

impl WindowSource for LinuxWindows {
    fn list_windows(&self) -> Vec<WindowInfo> {
        toplevels::list_windows().unwrap_or_else(|e| {
            eprintln!("failed to list windows: {e}");
            Vec::new()
        })
    }

    fn encoders(&self) -> Vec<VideoCodec> {
        vec![VideoCodec::H264]
    }

    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        capture::open(window, codec)
    }
}

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
        data_dir: data_dir(),
    };
    if let Err(e) = windowcast_host::run(config, Arc::new(LinuxWindows)).await {
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

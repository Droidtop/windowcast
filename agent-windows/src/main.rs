//! windowcast Windows host agent.
//!
//! Usage: `windowcast-agent-windows [--listen ADDR:PORT] [--no-pairing]
//! [--encoder auto|mf-hardware|nvenc|quicksync|amf|mf-software|openh264] [--fps N]
//! [--bitrate BPS_AT_1080P] [--microphone-device NAME] [--list-encoders]`
//! (`--microphone-device`: the virtual audio cable a client's microphone
//! plays into, part of its output's name; a known cable by default)
//! (`--listen` defaults to 0.0.0.0:47100).

#[cfg(windows)]
#[tokio::main]
async fn main() {
    use std::sync::Arc;

    use windowcast_agent_windows::encoder::{self, EncoderChoice};
    use windowcast_agent_windows::{Options, WindowsSource};
    use windowcast_host::{HostConfig, DEFAULT_LISTEN};

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
            "--encoder" => options.encoder = EncoderChoice::parse(&value("--encoder")).expect(
                "--encoder is auto, mf-hardware, nvenc, quicksync, amf, mf-software or openh264",
            ),
            "--fps" => options.fps = value("--fps").parse().expect("--fps needs a number"),
            "--bitrate" => {
                options.bitrate_1080p = value("--bitrate")
                    .parse()
                    .expect("--bitrate needs a number")
            }
            "--microphone-device" => options.microphone = Some(value("--microphone-device")),
            "--list-encoders" => {
                for line in encoder::list() {
                    println!("{line}");
                }
                return;
            }
            other => panic!("unknown argument {other}"),
        }
    }

    let source = match WindowsSource::new(options) {
        Ok(source) => source,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let data_dir = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .expect("APPDATA must be set")
        .join("windowcast")
        .join("agent");
    let config = HostConfig {
        listen,
        pairing,
        data_dir,
    };
    if let Err(e) = windowcast_host::run(config, Arc::new(source)).await {
        eprintln!("host stopped: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("windowcast-agent-windows runs on Windows only");
    std::process::exit(1);
}

//! The host agent for the platform the app is built for: agent-windows on
//! Windows, agent-linux on Linux (a Wayland session; input and the desktop
//! backend under sway). Elsewhere the host role says it is not available
//! yet and the client role works alone.

#[cfg(windows)]
mod imp {
    use windowcast_agent_windows::encoder::{self, EncoderChoice};
    use windowcast_agent_windows::{Options, WindowsSource};
    use windowcast_protocol::VideoCodec;

    use crate::config::HostSettings;

    pub type Agent = WindowsSource;
    pub const NAME: &str = "Windows";

    fn options(settings: &HostSettings) -> Result<Options, String> {
        let encoder = EncoderChoice::parse(&settings.encoder)
            .ok_or_else(|| format!("no encoder called {}", settings.encoder))?;
        let codec = match settings.codec.as_str() {
            "" => None,
            "H264" => Some(VideoCodec::H264),
            "H265" => Some(VideoCodec::H265),
            "Av1" => Some(VideoCodec::Av1),
            other => return Err(format!("no codec called {other}")),
        };
        Ok(Options {
            encoder,
            codec,
            fps: settings.fps.max(1),
            bitrate_1080p: (settings.bitrate_mbps * 1_000_000.0) as u32,
            microphone: (!settings.microphone_device.is_empty())
                .then(|| settings.microphone_device.clone()),
        })
    }

    pub fn open_agent(settings: &HostSettings) -> Result<Agent, String> {
        WindowsSource::new(options(settings)?)
    }

    pub fn configure(agent: &Agent, settings: &HostSettings) -> Result<(), String> {
        agent.set_options(options(settings)?)
    }

    pub fn encoders() -> Vec<(&'static str, Vec<VideoCodec>)> {
        EncoderChoice::ALL
            .into_iter()
            .map(|choice| (choice.name(), encoder::available_codecs(choice)))
            .collect()
    }

    /// Pointer positions and screen bounds in physical pixels on every
    /// screen, whatever its scaling, as capture and input expect.
    pub fn init() {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        // The main thread is the window's, and winit's drag and drop needs
        // it in OLE's single-threaded apartment: claim it first, so nothing
        // the host starts on this thread can put it in the multithreaded
        // one (OleInitialize then failed with RPC_E_CHANGED_MODE and the
        // host window could not open, Droidtop/tracker#464). winit's own
        // OleInitialize finds it done.
        unsafe {
            let _ = windows::Win32::System::Ole::OleInitialize(None);
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use windowcast_agent_linux::{LinuxSource, Options};
    use windowcast_protocol::VideoCodec;

    use crate::config::HostSettings;

    pub type Agent = LinuxSource;
    pub const NAME: &str = "Linux";

    fn options(settings: &HostSettings) -> Options {
        Options {
            fps: settings.fps.max(1),
            bitrate_1080p: (settings.bitrate_mbps * 1_000_000.0) as u32,
        }
    }

    pub fn open_agent(settings: &HostSettings) -> Result<Agent, String> {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Err("the Linux host needs a Wayland session (no WAYLAND_DISPLAY)".into());
        }
        Ok(LinuxSource::new(options(settings)))
    }

    pub fn configure(agent: &Agent, settings: &HostSettings) -> Result<(), String> {
        agent.set_options(options(settings));
        Ok(())
    }

    pub fn encoders() -> Vec<(&'static str, Vec<VideoCodec>)> {
        vec![("auto", vec![VideoCodec::H264])]
    }

    pub fn init() {}
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use windowcast_host::{FrameSource, WindowSource};
    use windowcast_protocol::{VideoCodec, WindowId, WindowInfo};

    use crate::config::HostSettings;

    /// Never made: [`open_agent`] refuses on this platform.
    pub enum Agent {}

    impl WindowSource for Agent {
        fn list_windows(&self) -> Vec<WindowInfo> {
            match *self {}
        }

        fn encoders(&self) -> Vec<VideoCodec> {
            match *self {}
        }

        fn open(&self, _: WindowId, _: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
            match *self {}
        }
    }

    pub const NAME: &str = std::env::consts::OS;

    pub fn open_agent(_: &HostSettings) -> Result<Agent, String> {
        Err(format!(
            "the host role is not available on {NAME} yet: this platform's agent cannot capture windows"
        ))
    }

    pub fn configure(agent: &Agent, _: &HostSettings) -> Result<(), String> {
        match *agent {}
    }

    pub fn encoders() -> Vec<(&'static str, Vec<VideoCodec>)> {
        Vec::new()
    }

    pub fn init() {}
}

pub use imp::{configure, encoders, init, open_agent, Agent, NAME};

//! One window served over RDP. The RDP desktop is the window: its size is
//! the window's when the client connects, its pictures are the window's
//! (from [`WindowSource::open_pictures`]), and the client's keys and
//! pointer go to the window through the host's own input delivery, as a
//! windowcast session's do. IronRDP's server does the protocol and picks
//! the bitmap codec the client supports (RemoteFX, or plain bitmaps).
//!
//! Every connection must log in with the [`Credentials`] the host set for
//! this window (NLA, over TLS with the host's [`HostIdentity`]).
//!
//! [`WithRdp`] adds this to any host as the `Rdp` backend of its sessions.

use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use ironrdp_server::{
    BitmapUpdate, Credentials, DesktopSize, DisplayUpdate, KeyboardEvent, MouseEvent, PixelFormat,
    RdpServer, RdpServerDisplay, RdpServerDisplayUpdates, RdpServerInputHandler,
};
use rand_core::RngCore;
use tokio::net::TcpListener;
use windowcast_host::{FrameSource, Handoff, Picture, PictureSource, WindowSource};
use windowcast_protocol::{
    BackendKind, HandoffTarget, InputEvent, PointerButton, VideoCodec, WindowId, WindowInfo,
};

use crate::tls::HostIdentity;
use crate::RdpError;

/// What a window's RDP server has done, for status and tests.
#[derive(Debug, Default)]
pub struct HostStats {
    /// Clients that logged in.
    pub sessions: AtomicU64,
    /// Pictures sent to clients.
    pub pictures: AtomicU64,
}

/// One window's RDP server: what it shows, who may log in, and how to
/// stop it.
pub struct WindowServer {
    pub source: Arc<dyn WindowSource>,
    pub window: WindowId,
    pub credentials: Credentials,
    pub identity: Arc<HostIdentity>,
    pub stats: Arc<HostStats>,
    /// Set to stop serving; a connected client is dropped.
    pub stop: Arc<AtomicBool>,
    /// Whether input from the RDP client drives the window: for a
    /// third-party client (compatibility mode, `windowcast-rdp serve`), not
    /// for a carrier of a windowcast session, whose input comes over the
    /// session under its rules (docs/BACKENDS.md, "One window, any
    /// carrier").
    pub input: bool,
}

/// Serves the window to RDP clients on `listener`, one connection at a
/// time, until stopped or the listener fails. Blocks: IronRDP's server runs
/// on a single-threaded runtime of its own, so give this a thread.
pub fn serve_window(listener: std::net::TcpListener, server: WindowServer) -> Result<(), RdpError> {
    listener.set_nonblocking(true)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        let listener = TcpListener::from_std(listener)?;
        serve(listener, server).await
    })
}

/// Resolves once `stop` is set.
async fn stopped(stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn serve(listener: TcpListener, config: WindowServer) -> Result<(), RdpError> {
    let WindowServer {
        source,
        window,
        credentials,
        identity,
        stats,
        stop,
        input,
    } = config;
    let size = source
        .list_windows()
        .into_iter()
        .find(|w| w.id == window)
        .map(|w| {
            (
                w.width.clamp(1, 8192) as u16,
                w.height.clamp(1, 8192) as u16,
            )
        })
        .ok_or_else(|| RdpError::Session("that window is gone".into()))?;
    let desktop = DesktopSize {
        width: size.0,
        height: size.1,
    };
    let builder = RdpServer::builder()
        .with_addr(listener.local_addr()?)
        .with_hybrid(identity.acceptor()?, identity.public_key()?);
    let builder = if input {
        builder.with_input_handler(WindowInput {
            window,
            deliver: windowcast_host::deliver_input(Arc::clone(&source)),
            size: desktop,
        })
    } else {
        builder.with_no_input()
    };
    let mut server = builder
        .with_display_handler(WindowDisplay {
            source,
            window,
            size: desktop,
            stats,
        })
        .build();
    server.set_credentials(Some(credentials));
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => accepted?,
            () = stopped(&stop) => return Ok(()),
        };
        let _ = stream.set_nodelay(true);
        eprintln!("rdp: {peer} connected to window {}", window.0);
        tokio::select! {
            result = server.run_connection(stream) => match result {
                Ok(()) => eprintln!("rdp: {peer} left"),
                Err(e) => eprintln!("rdp: {peer}: {e:#}"),
            },
            () = stopped(&stop) => return Ok(()),
        }
    }
}

/// Any host's windows, also served over RDP: a [`WindowSource`] that
/// passes everything to the host's own and adds the `Rdp` backend, starting
/// an RDP server for a window when a client's session asks for one (on a
/// port of its own, with a login made for that one stream, the certificate
/// pinned through the session).
pub struct WithRdp {
    inner: Arc<dyn WindowSource>,
    identity: Arc<HostIdentity>,
    /// Where the RDP servers listen; the port is picked per stream.
    bind: std::net::IpAddr,
    /// Whether RDP is offered (a host setting); on unless switched off.
    enabled: AtomicBool,
}

impl WithRdp {
    pub fn new(inner: Arc<dyn WindowSource>, bind: std::net::IpAddr) -> Result<Self, RdpError> {
        Ok(WithRdp {
            inner,
            identity: Arc::new(HostIdentity::generate("windowcast")?),
            bind,
            enabled: AtomicBool::new(true),
        })
    }

    /// Offers RDP to clients from now on, or stops offering it (streams
    /// already on RDP keep going until they stop).
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
    }

    fn start(&self, window: WindowId) -> Result<Handoff, String> {
        // Raw pictures first, so a host without them refuses here rather
        // than after the client has logged in.
        match self.inner.open_pictures(window) {
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(e),
            None => return Err("this host cannot hand out raw pictures for RDP".into()),
        }
        let listener = std::net::TcpListener::bind((self.bind, 0)).map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let mut secret = [0u8; 18];
        rand_core::OsRng.fill_bytes(&mut secret);
        let password: String = secret.iter().map(|b| format!("{b:02x}")).collect();
        let stop = Arc::new(AtomicBool::new(false));
        let server = WindowServer {
            source: Arc::clone(&self.inner),
            window,
            credentials: Credentials {
                username: "windowcast".into(),
                password: password.clone(),
                domain: None,
            },
            identity: Arc::clone(&self.identity),
            stats: Arc::new(HostStats::default()),
            stop: Arc::clone(&stop),
            input: false,
        };
        std::thread::spawn(move || {
            if let Err(e) = serve_window(listener, server) {
                eprintln!("rdp: window {}: {e}", window.0);
            }
        });
        Ok(Handoff {
            target: HandoffTarget {
                address: String::new(),
                port,
                username: "windowcast".into(),
                password,
                certificate_sha256: Some(self.identity.fingerprint()),
            },
            guard: Box::new(StopOnDrop(stop)),
        })
    }
}

/// Stops a handed-off window's RDP server when dropped.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl WindowSource for WithRdp {
    fn list_windows(&self) -> Vec<WindowInfo> {
        self.inner.list_windows()
    }
    fn encoders(&self) -> Vec<VideoCodec> {
        self.inner.encoders()
    }
    fn backends(&self) -> Vec<BackendKind> {
        let mut kinds = self.inner.backends();
        if self.enabled.load(Ordering::SeqCst) {
            kinds.push(BackendKind::Rdp);
        }
        kinds
    }
    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        self.inner.open(window, codec)
    }
    fn open_desktop(
        &self,
        window: WindowId,
        codec: VideoCodec,
    ) -> Result<Box<dyn FrameSource>, String> {
        self.inner.open_desktop(window, codec)
    }
    fn open_pictures(&self, window: WindowId) -> Option<Result<Box<dyn PictureSource>, String>> {
        self.inner.open_pictures(window)
    }
    fn open_audio(
        &self,
        window: WindowId,
    ) -> Option<Result<Box<dyn windowcast_host::audio::AudioSource>, String>> {
        self.inner.open_audio(window)
    }
    fn microphone(
        &self,
    ) -> Option<Result<Box<dyn windowcast_host::audio::MicrophoneSink>, String>> {
        self.inner.microphone()
    }
    fn input(&self, event: &InputEvent, focus: Option<WindowId>) {
        self.inner.input(event, focus)
    }
    fn gamepads(&self) -> Option<Result<Box<dyn windowcast_host::gamepad::GamepadSink>, String>> {
        self.inner.gamepads()
    }
    fn clipboard(&self) -> Option<(u64, String)> {
        self.inner.clipboard()
    }
    fn set_clipboard(&self, text: &str) {
        self.inner.set_clipboard(text)
    }
    fn handoff(&self, kind: BackendKind, window: WindowId) -> Option<Result<Handoff, String>> {
        if kind == BackendKind::Rdp {
            if !self.enabled.load(Ordering::SeqCst) {
                return Some(Err("this host does not offer RDP".into()));
            }
            Some(self.start(window))
        } else {
            self.inner.handoff(kind, window)
        }
    }
}

struct WindowDisplay {
    source: Arc<dyn WindowSource>,
    window: WindowId,
    size: DesktopSize,
    stats: Arc<HostStats>,
}

#[async_trait::async_trait]
impl RdpServerDisplay for WindowDisplay {
    async fn size(&mut self) -> DesktopSize {
        self.size
    }

    async fn updates(&mut self) -> anyhow::Result<Box<dyn RdpServerDisplayUpdates>> {
        let mut capture = match self.source.open_pictures(self.window) {
            Some(Ok(capture)) => capture,
            Some(Err(e)) => anyhow::bail!("window {}: {e}", self.window.0),
            None => anyhow::bail!("this host cannot hand out raw pictures"),
        };
        // Capture blocks; a thread feeds the async side, dropping pictures
        // the connection is not ready for.
        let (tx, rx) = tokio::sync::mpsc::channel::<Picture>(2);
        std::thread::spawn(move || {
            while let Some(picture) = capture.next_picture() {
                match tx.try_send(picture) {
                    Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
                }
            }
        });
        // The display is asked for once a client has logged in.
        self.stats.sessions.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(WindowUpdates {
            pictures: rx,
            size: self.size,
            stats: Arc::clone(&self.stats),
        }))
    }
}

struct WindowUpdates {
    pictures: tokio::sync::mpsc::Receiver<Picture>,
    size: DesktopSize,
    stats: Arc<HostStats>,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for WindowUpdates {
    async fn next_update(&mut self) -> anyhow::Result<Option<DisplayUpdate>> {
        let Some(picture) = self.pictures.recv().await else {
            return Ok(None);
        };
        let (width, height) = (usize::from(self.size.width), usize::from(self.size.height));
        let data = fit(&picture, width, height);
        self.stats.pictures.fetch_add(1, Ordering::SeqCst);
        Ok(Some(DisplayUpdate::Bitmap(BitmapUpdate {
            x: 0,
            y: 0,
            width: NonZeroU16::new(self.size.width).expect("a window has a width"),
            height: NonZeroU16::new(self.size.height).expect("a window has a height"),
            format: PixelFormat::BgrA32,
            data: data.into(),
            stride: NonZeroUsize::new(width * 4).expect("a window has a width"),
        })))
    }
}

/// The picture on a desktop of `width` by `height`: the window keeps the
/// size it had when the client connected, so a window that has since grown
/// is cut at the edge and one that shrank is padded with black.
fn fit(picture: &Picture, width: usize, height: usize) -> Vec<u8> {
    let (pw, ph) = (picture.width as usize, picture.height as usize);
    if pw == width && ph == height && picture.stride == width * 4 {
        return picture.data.clone();
    }
    let mut out = vec![0u8; width * height * 4];
    let row = pw.min(width) * 4;
    for y in 0..ph.min(height) {
        let from = &picture.data[y * picture.stride..][..row];
        out[y * width * 4..][..row].copy_from_slice(from);
    }
    out
}

/// The client's input, to the window.
struct WindowInput {
    window: WindowId,
    deliver: Sender<(InputEvent, Option<WindowId>)>,
    size: DesktopSize,
}

impl WindowInput {
    fn send(&self, event: InputEvent) {
        let _ = self.deliver.send((event, Some(self.window)));
    }

    fn button(&self, button: PointerButton, pressed: bool) {
        self.send(InputEvent::PointerButton {
            window: self.window,
            button,
            pressed,
        });
    }
}

impl RdpServerInputHandler for WindowInput {
    fn keyboard(&mut self, event: KeyboardEvent) {
        match event {
            KeyboardEvent::Pressed { code, extended }
            | KeyboardEvent::Released { code, extended } => {
                let pressed = matches!(event, KeyboardEvent::Pressed { .. });
                if let Some(keycode) =
                    windowcast_protocol::keys::evdev_from_scan(u16::from(code), extended)
                {
                    self.send(InputEvent::Key { keycode, pressed });
                }
            }
            KeyboardEvent::UnicodePressed(unit) => {
                if let Some(text) = char::from_u32(u32::from(unit)) {
                    self.send(InputEvent::Text {
                        text: text.to_string(),
                    });
                }
            }
            KeyboardEvent::UnicodeReleased(_) | KeyboardEvent::Synchronize(_) => {}
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        let window = self.window;
        match event {
            MouseEvent::Move { x, y } => {
                let w = f32::from(self.size.width.max(2) - 1);
                let h = f32::from(self.size.height.max(2) - 1);
                self.send(InputEvent::PointerMove {
                    window,
                    x: (f32::from(x) / w).clamp(0.0, 1.0),
                    y: (f32::from(y) / h).clamp(0.0, 1.0),
                });
            }
            MouseEvent::LeftPressed => self.button(PointerButton::Left, true),
            MouseEvent::LeftReleased => self.button(PointerButton::Left, false),
            MouseEvent::RightPressed => self.button(PointerButton::Right, true),
            MouseEvent::RightReleased => self.button(PointerButton::Right, false),
            MouseEvent::MiddlePressed => self.button(PointerButton::Middle, true),
            MouseEvent::MiddleReleased => self.button(PointerButton::Middle, false),
            MouseEvent::Button4Pressed => self.button(PointerButton::Back, true),
            MouseEvent::Button4Released => self.button(PointerButton::Back, false),
            MouseEvent::Button5Pressed => self.button(PointerButton::Forward, true),
            MouseEvent::Button5Released => self.button(PointerButton::Forward, false),
            // RDP wheel units are 120 to a notch, positive away from the
            // user, as windowcast's are in notches.
            MouseEvent::VerticalScroll { value } => self.send(InputEvent::PointerScroll {
                window,
                dx: 0.0,
                dy: f32::from(value) / 120.0,
            }),
            MouseEvent::Scroll { x, y } => self.send(InputEvent::PointerScroll {
                window,
                dx: x as f32 / 120.0,
                dy: y as f32 / 120.0,
            }),
            MouseEvent::RelMove { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grown_window_is_cut_and_a_shrunk_one_padded() {
        let picture = Picture {
            width: 3,
            height: 2,
            stride: 12,
            data: (0..24).collect(),
        };
        let cut = fit(&picture, 2, 1);
        assert_eq!(cut, (0..8).collect::<Vec<u8>>());
        let padded = fit(&picture, 4, 3);
        assert_eq!(&padded[..12], &(0..12).collect::<Vec<u8>>()[..]);
        assert_eq!(&padded[12..16], &[0, 0, 0, 0]);
        assert_eq!(&padded[32..48], &[0; 16]);
    }
}

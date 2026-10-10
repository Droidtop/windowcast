//! A client for RDP hosts: windowcast's own RDP face, Windows' Remote
//! Desktop, or any other. It logs in (TLS, then NLA with the user's name
//! and password), keeps the host's picture as RGBA, hands out a copy each
//! time the picture changes, and sends windowcast input as RDP input.
//!
//! Runs on its own thread with the blocking IronRDP driver; the socket
//! reads time out briefly so input goes out between the host's updates.

use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use ironrdp_blocking::Framed;
use ironrdp_connector::{ClientConnector, ConnectionResult, Credentials, DesktopSize};
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_input::{Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp_pdu::gcc::KeyboardType;
use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp_pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp_session::image::DecodedImage;
use ironrdp_session::{ActiveStageBuilder, ActiveStageOutput};
use windowcast_protocol::{InputEvent, PointerButton};

use crate::remoteapp::{
    window_orders, RailChannel, RailStatus, RailTap, RemoteApp, RemoteWindow, Windows,
};
use crate::tls::{self, Pinned};
use crate::RdpError;

/// Where and how to log in.
pub struct ClientConfig {
    pub address: SocketAddr,
    /// The host's name, for TLS and NLA (an address works).
    pub server_name: String,
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
    /// The desktop size to ask for (a windowcast host answers with its
    /// window's size instead).
    pub size: (u16, u16),
    /// The host certificate's SHA-256 to insist on; `None` takes any and
    /// reports it in [`RdpStream::fingerprint`].
    pub pinned: Option<[u8; 32]>,
    /// Run this program on the host and show only its window (RemoteApp)
    /// instead of the desktop. `size` is then the desktop the program's
    /// window lives on.
    pub remote_app: Option<RemoteApp>,
}

/// The host's whole picture after a change: RGBA, tightly packed.
#[derive(Debug, Clone)]
pub struct RgbaPicture {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// A connected RDP session.
pub struct RdpStream {
    pictures: std::sync::Mutex<Receiver<RgbaPicture>>,
    /// The newest picture, whoever took it: a host sends nothing while the
    /// window does not change, so a view that starts late begins here.
    latest: Arc<std::sync::Mutex<Option<RgbaPicture>>>,
    input: Sender<InputEvent>,
    stop: Arc<AtomicBool>,
    /// Set once the session has ended (the host closed it, or it failed).
    pub ended: Arc<AtomicBool>,
    /// The desktop the host gave.
    pub size: (u16, u16),
    /// SHA-256 of the host's TLS certificate.
    pub fingerprint: [u8; 32],
}

impl RdpStream {
    /// The newest picture so far, taken or not.
    pub fn latest_picture(&self) -> Option<RgbaPicture> {
        self.latest.lock().expect("latest").clone()
    }

    /// The host's picture after its next change, waiting up to `timeout`.
    pub fn next_picture(&self, timeout: Duration) -> Result<RgbaPicture, mpsc::RecvTimeoutError> {
        self.pictures
            .lock()
            .expect("pictures")
            .recv_timeout(timeout)
    }

    /// Sends input. Pointer positions are fractions of the desktop, as
    /// windowcast's are; the window an event names is ignored (the
    /// desktop is the one picture).
    pub fn input(&self, event: InputEvent) -> bool {
        self.input.send(event).is_ok()
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for RdpStream {
    fn drop(&mut self) {
        self.stop();
    }
}

type Tls = RailTap<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>;

/// Answers sspi's requests for a Kerberos KDC: there is none here, so NLA
/// uses NTLM (as it does with a local account or an address for a name).
struct NoKdc;

impl ironrdp_connector::sspi::network_client::NetworkClient for NoKdc {
    fn send(
        &self,
        _request: &ironrdp_connector::sspi::generator::NetworkRequest,
    ) -> ironrdp_connector::sspi::Result<Vec<u8>> {
        Err(ironrdp_connector::sspi::Error::new(
            ironrdp_connector::sspi::ErrorKind::NoAuthenticatingAuthority,
            "no Kerberos KDC is reachable from windowcast",
        ))
    }
}

fn connector_config(config: &ClientConfig) -> ironrdp_connector::Config {
    ironrdp_connector::Config {
        credentials: Credentials::UsernamePassword {
            username: config.username.clone(),
            password: config.password.clone(),
        },
        domain: config.domain.clone(),
        enable_tls: false,
        enable_credssp: true,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: DesktopSize {
            width: config.size.0,
            height: config.size.1,
        },
        desktop_scale_factor: 0,
        bitmap: None,
        client_build: 0,
        client_name: "windowcast".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        platform: if cfg!(windows) {
            MajorPlatformType::WINDOWS
        } else if cfg!(target_os = "android") {
            MajorPlatformType::ANDROID
        } else {
            MajorPlatformType::UNIX
        },
        enable_server_pointer: false,
        pointer_software_rendering: true,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        compression_type: None,
        multitransport_flags: None,
        performance_flags: PerformanceFlags::default(),
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
    }
}

/// The error and every cause under it: IronRDP's errors carry the useful
/// part (what the host or the login said) as their source.
fn connect_err(e: impl std::error::Error) -> RdpError {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    RdpError::Connect(text)
}

/// Logs in and starts the session.
pub fn connect(config: &ClientConfig) -> Result<RdpStream, RdpError> {
    let tcp = TcpStream::connect_timeout(&config.address, Duration::from_secs(10))?;
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(Some(Duration::from_secs(15)))?;
    let control = tcp.try_clone()?;
    let client_addr = tcp.local_addr()?;

    let mut framed = Framed::new(tcp);
    let mut connector = ClientConnector::new(connector_config(config), client_addr);
    let rail = Arc::new(std::sync::Mutex::new(RailStatus::default()));
    if let Some(app) = &config.remote_app {
        connector.attach_static_channel(RailChannel::new(
            app.clone(),
            config.size,
            Arc::clone(&rail),
        ));
    }
    let should_upgrade =
        ironrdp_blocking::connect_begin(&mut framed, &mut connector).map_err(connect_err)?;

    // TLS, the host's certificate pinned when we know it.
    let tcp = framed.into_inner_no_leftover();
    let provider = crate::crypto_provider();
    let tls_config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| RdpError::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned {
            pinned: config.pinned,
            provider,
        }))
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(config.server_name.clone())
        .map_err(|e| RdpError::Tls(e.to_string()))?;
    let session = rustls::ClientConnection::new(Arc::new(tls_config), name)
        .map_err(|e| RdpError::Tls(e.to_string()))?;
    let mut stream = rustls::StreamOwned::new(session, tcp);
    stream.flush()?;
    let cert = stream
        .conn
        .peer_certificates()
        .and_then(|certs| certs.first())
        .ok_or_else(|| RdpError::Tls("the host sent no certificate".into()))?
        .to_vec();
    let fingerprint = tls::fingerprint(&cert);
    let public_key = tls::public_key(&cert)?;

    let upgraded = ironrdp_blocking::mark_as_upgraded(should_upgrade, &mut connector);
    // A RemoteApp session asks for it in two PDUs IronRDP writes itself.
    let stream = if config.remote_app.is_some() {
        RailTap::new(stream)
    } else {
        RailTap::passthrough(stream)
    };
    let mut framed = Framed::new(stream);
    let result = ironrdp_blocking::connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut NoKdc,
        ironrdp_connector::ServerName::new(config.server_name.clone()),
        public_key,
        None,
    )
    .map_err(connect_err)?;

    let size = (result.desktop_size.width, result.desktop_size.height);
    let remote_app = config.remote_app.is_some();
    let (pictures_tx, pictures) = mpsc::sync_channel(2);
    let latest = Arc::new(std::sync::Mutex::new(None));
    let (input, input_rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let ended = Arc::new(AtomicBool::new(false));
    // Short reads from here on, so input is sent between updates.
    control.set_read_timeout(Some(Duration::from_millis(10)))?;
    {
        let (stop, ended, latest) = (Arc::clone(&stop), Arc::clone(&ended), Arc::clone(&latest));
        std::thread::spawn(move || {
            let pictures_tx = Pictures {
                tx: pictures_tx,
                latest,
            };
            if let Err(e) = run(result, framed, pictures_tx, input_rx, &stop, remote_app) {
                eprintln!("rdp: the session ended: {e}");
            }
            ended.store(true, Ordering::SeqCst);
        });
    }
    Ok(RdpStream {
        pictures: std::sync::Mutex::new(pictures),
        latest,
        input,
        stop,
        ended,
        size,
        fingerprint,
    })
}

fn run(
    result: ConnectionResult,
    mut framed: Framed<Tls>,
    pictures: Pictures,
    input: Receiver<InputEvent>,
    stop: &AtomicBool,
    remote_app: bool,
) -> Result<(), RdpError> {
    let size = result.desktop_size;
    // What the client sees: the desktop, or for RemoteApp the program's
    // window, once the server has described it.
    let desktop = Area {
        x: 0,
        y: 0,
        width: size.width,
        height: size.height,
    };
    let mut windows = Windows::default();
    let mut area = (!remote_app).then_some(desktop);
    let mut image = DecodedImage::new(PixelFormat::RgbA32, size.width, size.height);
    let mut stage = ActiveStageBuilder {
        static_channels: result.static_channels,
        user_channel_id: result.user_channel_id,
        io_channel_id: result.io_channel_id,
        message_channel_id: result.message_channel_id,
        share_id: result.share_id,
        compression_type: result.compression_type,
        enable_server_pointer: result.enable_server_pointer,
        pointer_software_rendering: result.pointer_software_rendering,
    }
    .build();
    let mut keys = Database::new();
    let session = |e: ironrdp_session::SessionError| RdpError::Session(e.to_string());

    while !stop.load(Ordering::SeqCst) {
        // Input first, so a busy host does not hold it up.
        let mut operations = Vec::new();
        while let Ok(event) = input.try_recv() {
            if let Some(area) = area {
                operations.extend(operations_for(&event, area));
            }
        }
        if !operations.is_empty() {
            let events = keys.apply(operations);
            let outputs = stage
                .process_fastpath_input(&mut image, &events)
                .map_err(session)?;
            if !handle(&mut framed, outputs, &image, &pictures, area)? {
                return Ok(());
            }
        }

        let (action, payload) = match framed.read_pdu() {
            Ok(pdu) => pdu,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let mut moved = false;
        if remote_app && action == ironrdp_pdu::Action::FastPath {
            for order in window_orders(&payload) {
                tracing::debug!(?order, "RemoteApp: window order");
                windows.apply(order);
                moved = true;
            }
            if moved {
                area = windows.main().map(|w| Area::of(w, desktop));
            }
        }
        let mut outputs = stage
            .process(&mut image, action, &payload)
            .map_err(session)?;
        if moved {
            // The window moved or appeared: hand out its picture again.
            outputs.push(ActiveStageOutput::GraphicsUpdate(
                ironrdp_pdu::geometry::InclusiveRectangle {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
            ));
        }
        if !handle(&mut framed, outputs, &image, &pictures, area)? {
            return Ok(());
        }
    }
    // Leave politely.
    if let Ok(frame) = stage.graceful_shutdown() {
        for output in frame {
            if let ActiveStageOutput::ResponseFrame(frame) = output {
                let _ = framed.write_all(&frame);
            }
        }
    }
    Ok(())
}

/// Sends what the session has to send and hands out the picture when it
/// changed. `false` when the host ended the session.
fn handle(
    framed: &mut Framed<Tls>,
    outputs: Vec<ActiveStageOutput>,
    image: &DecodedImage,
    pictures: &Pictures,
    area: Option<Area>,
) -> Result<bool, RdpError> {
    let mut changed = false;
    for output in outputs {
        match output {
            ActiveStageOutput::ResponseFrame(frame) => {
                if !frame.is_empty() {
                    framed.write_all(&frame)?;
                }
            }
            ActiveStageOutput::GraphicsUpdate(_) => changed = true,
            ActiveStageOutput::Terminate(reason) => {
                eprintln!("rdp: the host ended the session: {reason}");
                return Ok(false);
            }
            ActiveStageOutput::DeactivateAll => {
                return Err(RdpError::Session(
                    "the host restarted the session's display (reactivation is not handled yet)"
                        .into(),
                ));
            }
            _ => {}
        }
    }
    if let (true, Some(area)) = (changed, area) {
        pictures.send(area.cut(image));
    }
    Ok(true)
}

/// Where pictures go: the queue a consumer takes from (one that is behind
/// gets the newest next time), and the newest kept.
struct Pictures {
    tx: SyncSender<RgbaPicture>,
    latest: Arc<std::sync::Mutex<Option<RgbaPicture>>>,
}

impl Pictures {
    fn send(&self, picture: RgbaPicture) {
        *self.latest.lock().expect("latest") = Some(picture.clone());
        let _ = self.tx.try_send(picture);
    }
}

/// The part of the desktop the client shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Area {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

impl Area {
    /// A remote window's rectangle, kept inside the desktop.
    fn of(window: &RemoteWindow, desktop: Area) -> Area {
        let x = window.offset.0.clamp(0, i32::from(desktop.width) - 1) as u16;
        let y = window.offset.1.clamp(0, i32::from(desktop.height) - 1) as u16;
        let width = (window.size.0.min(u32::from(desktop.width - x)) as u16).max(1);
        let height = (window.size.1.min(u32::from(desktop.height - y)) as u16).max(1);
        Area {
            x,
            y,
            width,
            height,
        }
    }

    /// This part of the picture, as its own RGBA picture.
    fn cut(&self, image: &DecodedImage) -> RgbaPicture {
        let stride = usize::from(image.width()) * 4;
        let row = usize::from(self.width) * 4;
        let mut data = Vec::with_capacity(row * usize::from(self.height));
        for y in 0..usize::from(self.height) {
            let start = (usize::from(self.y) + y) * stride + usize::from(self.x) * 4;
            data.extend_from_slice(&image.data()[start..start + row]);
        }
        RgbaPicture {
            width: u32::from(self.width),
            height: u32::from(self.height),
            data,
        }
    }
}

/// RDP input for one windowcast event, positions within `area`.
fn operations_for(event: &InputEvent, area: Area) -> Vec<Operation> {
    let position = |x: f32, y: f32| MousePosition {
        x: area.x + (x.clamp(0.0, 1.0) * f32::from(area.width.max(1) - 1)).round() as u16,
        y: area.y + (y.clamp(0.0, 1.0) * f32::from(area.height.max(1) - 1)).round() as u16,
    };
    match event {
        InputEvent::PointerMove { x, y, .. } => vec![Operation::MouseMove(position(*x, *y))],
        InputEvent::PointerButton {
            button, pressed, ..
        } => {
            let button = match button {
                PointerButton::Left => MouseButton::Left,
                PointerButton::Right => MouseButton::Right,
                PointerButton::Middle => MouseButton::Middle,
                PointerButton::Back => MouseButton::X1,
                PointerButton::Forward => MouseButton::X2,
            };
            vec![if *pressed {
                Operation::MouseButtonPressed(button)
            } else {
                Operation::MouseButtonReleased(button)
            }]
        }
        InputEvent::PointerScroll { dx, dy, .. } => {
            let mut operations = Vec::new();
            if *dy != 0.0 {
                operations.push(Operation::WheelRotations(WheelRotations {
                    is_vertical: true,
                    rotation_units: (dy * 120.0) as i16,
                }));
            }
            if *dx != 0.0 {
                operations.push(Operation::WheelRotations(WheelRotations {
                    is_vertical: false,
                    rotation_units: (dx * 120.0) as i16,
                }));
            }
            operations
        }
        InputEvent::Key { keycode, pressed } => {
            match windowcast_protocol::keys::scan_from_evdev(*keycode) {
                Some((scan, extended)) => {
                    let code = Scancode::from_u8(extended, scan as u8);
                    vec![if *pressed {
                        Operation::KeyPressed(code)
                    } else {
                        Operation::KeyReleased(code)
                    }]
                }
                None => Vec::new(),
            }
        }
        InputEvent::Text { text } => text
            .chars()
            .flat_map(|c| {
                [
                    Operation::UnicodeKeyPressed(c),
                    Operation::UnicodeKeyReleased(c),
                ]
            })
            .collect(),
        InputEvent::Touch { x, y, phase, .. } => {
            // A touch is the pointer: down, drag, up.
            let at = Operation::MouseMove(position(*x, *y));
            match phase {
                windowcast_protocol::TouchPhase::Start => {
                    vec![at, Operation::MouseButtonPressed(MouseButton::Left)]
                }
                windowcast_protocol::TouchPhase::Move => vec![at],
                windowcast_protocol::TouchPhase::End | windowcast_protocol::TouchPhase::Cancel => {
                    vec![at, Operation::MouseButtonReleased(MouseButton::Left)]
                }
            }
        }
        InputEvent::Gamepad { .. } | InputEvent::GamepadGone { .. } => Vec::new(),
    }
}

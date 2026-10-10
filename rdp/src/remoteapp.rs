//! RemoteApp (RAIL, MS-RDPERP) on the client: windowcast asks the RDP
//! host to run one program and shows just that program's window, as mstsc
//! and FreeRDP do in their RemoteApp mode.
//!
//! IronRDP 0.10 has no option for this (docs/upstream/ironrdp-remoteapp.md
//! drafts one), so until it does:
//!
//! - [`RailTap`] sits between IronRDP and the TLS stream and edits two
//!   PDUs on their way out: the Client Info PDU gets INFO_RAIL, and the
//!   Confirm Active PDU gets the Remote Programs and Window List capability
//!   sets.
//! - [`RailChannel`] is the `rail` static virtual channel (IronRDP carries
//!   any static channel an application adds): it answers the server's
//!   handshake and asks for the program.
//! - [`window_orders`] reads the server's Windowing Alternate Secondary
//!   Drawing Orders out of fast-path Orders updates, which IronRDP skips,
//!   and [`Windows`] keeps the window list they describe.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use windowcast_protocol::WindowKind;

use ironrdp_pdu::gcc::{ChannelName, ChannelOptions, ConferenceCreateRequest};
use ironrdp_pdu::mcs::{ConnectInitial, SendDataRequest};
use ironrdp_pdu::rdp::capability_sets::CapabilitySet;
use ironrdp_pdu::rdp::headers::{ShareControlHeader, ShareControlPdu};
use ironrdp_pdu::x224::{X224Data, X224};
use ironrdp_svc::{SvcClientProcessor, SvcMessage, SvcProcessor};

/// The program to run on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteApp {
    /// An executable (a path, or a name the host resolves).
    pub program: String,
    pub arguments: String,
    pub working_dir: String,
}

// MS-RDPBCGR TS_INFO_PACKET flags.
const INFO_PKT: u16 = 0x0040;
const INFO_RAIL: u32 = 0x0000_8000;
// MS-RDPERP 2.2.1.1.1 and 2.2.1.1.2.
const TS_RAIL_LEVEL_SUPPORTED: u32 = 0x1;
const TS_RAIL_LEVEL_HANDSHAKE_EX_SUPPORTED: u32 = 0x80;
const TS_WINDOW_LEVEL_SUPPORTED: u32 = 0x1;

/// A stream that edits the Client Info and Confirm Active PDUs on their
/// way out, when the session is a RemoteApp one.
pub struct RailTap<S> {
    pub inner: S,
    channel_done: bool,
    info_done: bool,
    caps_done: bool,
}

impl<S> RailTap<S> {
    pub fn new(inner: S) -> Self {
        RailTap {
            inner,
            channel_done: false,
            info_done: false,
            caps_done: false,
        }
    }

    /// A tap that changes nothing (a desktop session).
    pub fn passthrough(inner: S) -> Self {
        RailTap {
            inner,
            channel_done: true,
            info_done: true,
            caps_done: true,
        }
    }

    /// The PDU in `buf` with RemoteApp asked for, or `None` to send it as
    /// it is.
    fn rewrite(&mut self, buf: &[u8]) -> Option<Vec<u8>> {
        if self.channel_done && self.info_done && self.caps_done {
            return None;
        }
        if !self.channel_done {
            if let Some(edited) = rail_channel_options(buf) {
                self.channel_done = true;
                return Some(edited);
            }
        }
        let X224(request) = ironrdp_core::decode::<X224<SendDataRequest<'_>>>(buf).ok()?;
        let data = request.user_data.as_ref();
        // The Client Info PDU: a basic security header with INFO_PKT, then
        // TS_INFO_PACKET (CodePage, then flags).
        if !self.info_done
            && data.len() >= 12
            && u16::from_le_bytes([data[0], data[1]]) & INFO_PKT != 0
        {
            let mut patched = data.to_vec();
            let flags = u32::from_le_bytes(patched[8..12].try_into().ok()?) | INFO_RAIL;
            patched[8..12].copy_from_slice(&flags.to_le_bytes());
            self.info_done = true;
            tracing::debug!(
                flags = format!("{flags:#x}"),
                "RemoteApp: INFO_RAIL set in the Client Info PDU"
            );
            return reencode(&request, patched);
        }
        if !self.caps_done {
            let mut header = ironrdp_core::decode::<ShareControlHeader>(data).ok()?;
            if let ShareControlPdu::ClientConfirmActive(confirm) = &mut header.share_control_pdu {
                let caps = &mut confirm.pdu.capability_sets;
                caps.retain(|c| {
                    !matches!(c, CapabilitySet::Rail(_) | CapabilitySet::WindowList(_))
                });
                caps.push(CapabilitySet::Rail(
                    (TS_RAIL_LEVEL_SUPPORTED | TS_RAIL_LEVEL_HANDSHAKE_EX_SUPPORTED)
                        .to_le_bytes()
                        .to_vec(),
                ));
                let mut window = TS_WINDOW_LEVEL_SUPPORTED.to_le_bytes().to_vec();
                window.push(3); // NumIconCaches
                window.extend_from_slice(&12u16.to_le_bytes()); // NumIconCacheEntries
                caps.push(CapabilitySet::WindowList(window));
                self.caps_done = true;
                tracing::debug!(
                    sets = caps.len(),
                    "RemoteApp: RAIL and Window List capability sets added"
                );
                let data = ironrdp_core::encode_vec(&header).ok()?;
                return reencode(&request, data);
            }
        }
        None
    }
}

/// The options FreeRDP opens the rail channel with
/// (channels/rail/client/rail_main.c, VirtualChannelEntryEx). With
/// SHOW_PROTOCOL the host's RemoteApp process reads each PDU with its
/// channel header, and the client's chunks carry CHANNEL_FLAG_SHOW_PROTOCOL
/// (libfreerdp/core/channels.c, freerdp_channel_send). IronRDP declares a
/// channel's options from its compression condition only, so they are set
/// here, in the Connect Initial it writes.
fn rail_options() -> ChannelOptions {
    ChannelOptions::INITIALIZED
        | ChannelOptions::ENCRYPT_RDP
        | ChannelOptions::COMPRESS_RDP
        | ChannelOptions::SHOW_PROTOCOL
}

/// The Connect Initial in `buf` with the rail channel's options set, or
/// `None` when `buf` is not one.
fn rail_channel_options(buf: &[u8]) -> Option<Vec<u8>> {
    let X224(data) = ironrdp_core::decode::<X224<X224Data<'_>>>(buf).ok()?;
    let mut initial = ironrdp_core::decode::<ConnectInitial>(data.data.as_ref()).ok()?;
    let mut blocks = initial.conference_create_request.gcc_blocks().clone();
    let rail = ChannelName::from_static(b"rail\0\0\0\0");
    let channel = blocks
        .network
        .as_mut()?
        .channels
        .iter_mut()
        .find(|c| c.name == rail)?;
    channel.options = rail_options();
    initial.conference_create_request = ConferenceCreateRequest::new(blocks).ok()?;
    let user_data = ironrdp_core::encode_vec(&initial).ok()?;
    tracing::debug!("RemoteApp: rail channel options set in the Connect Initial");
    ironrdp_core::encode_vec(&X224(X224Data {
        data: user_data.into(),
    }))
    .ok()
}

fn reencode(request: &SendDataRequest<'_>, user_data: Vec<u8>) -> Option<Vec<u8>> {
    ironrdp_core::encode_vec(&X224(SendDataRequest {
        initiator_id: request.initiator_id,
        channel_id: request.channel_id,
        user_data: user_data.into(),
    }))
    .ok()
}

impl<S: Read> Read for RailTap<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<S: Write> Write for RailTap<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.rewrite(buf) {
            Some(edited) => {
                self.inner.write_all(&edited)?;
                Ok(buf.len())
            }
            None => self.inner.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// MS-RDPERP 2.2.2.1 order types.
const RAIL_EXEC: u16 = 0x0001;
const RAIL_SYSPARAM: u16 = 0x0003;
const RAIL_HANDSHAKE: u16 = 0x0005;
const RAIL_CLIENTSTATUS: u16 = 0x000b;
const RAIL_EXEC_RESULT: u16 = 0x0080;
const RAIL_HANDSHAKE_EX: u16 = 0x0013;

fn rail_pdu(order: u16, body: &[u8]) -> Vec<u8> {
    let mut pdu = Vec::with_capacity(4 + body.len());
    pdu.extend_from_slice(&order.to_le_bytes());
    pdu.extend_from_slice(&((4 + body.len()) as u16).to_le_bytes());
    pdu.extend_from_slice(body);
    pdu
}

fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// What the `rail` channel has heard.
#[derive(Debug, Default)]
pub struct RailStatus {
    /// The server answered the handshake and the program was asked for.
    pub started: bool,
    /// The Server Execute Result, when it came: 0 is success.
    pub exec_result: Option<u16>,
    /// PDUs for the client to send on the channel. They go out through
    /// the session's own sender (ActiveStage::process_svc_processor_
    /// messages, with the client's MCS user ID): a reply returned from
    /// `process` is sent with the server's ID as the initiator instead
    /// (ironrdp-session x224 `process_svc_messages`).
    pub outgoing: Vec<Vec<u8>>,
}

/// The `rail` static virtual channel, client side.
#[derive(Debug)]
pub struct RailChannel {
    app: RemoteApp,
    /// The desktop size, for the work area the client reports.
    desktop: (u16, u16),
    status: Arc<Mutex<RailStatus>>,
}

impl RailChannel {
    pub fn new(app: RemoteApp, desktop: (u16, u16), status: Arc<Mutex<RailStatus>>) -> Self {
        RailChannel {
            app,
            desktop,
            status,
        }
    }

    /// The client's answer to the server's handshake: its own handshake,
    /// its status, the system parameters, then the program. FreeRDP sends
    /// these, in this order, when the handshake comes (2.x
    /// client/X11/xf_rail.c, xf_rail_server_handshake and
    /// xf_rail_server_start_cmd; client/Windows/wf_rail.c), and the
    /// parameters are its set (rail_main.c, rail_client_system_param).
    fn start_messages(&self) -> Vec<Vec<u8>> {
        let mut out = vec![
            rail_pdu(RAIL_HANDSHAKE, &7601u32.to_le_bytes()),
            rail_pdu(RAIL_CLIENTSTATUS, &0u32.to_le_bytes()),
        ];
        let rect = |param: u32| {
            let mut body = param.to_le_bytes().to_vec();
            for v in [0, 0, self.desktop.0, self.desktop.1] {
                body.extend_from_slice(&v.to_le_bytes());
            }
            rail_pdu(RAIL_SYSPARAM, &body)
        };
        let flag = |param: u32, on: u8| {
            let mut body = param.to_le_bytes().to_vec();
            body.push(on);
            rail_pdu(RAIL_SYSPARAM, &body)
        };
        // SPI_SETHIGHCONTRAST: flags (FreeRDP's 0x7E), then an empty colour
        // scheme (its length, 2, then a zero-length unicode string).
        let mut contrast = 0x0043u32.to_le_bytes().to_vec();
        contrast.extend_from_slice(&0x7Eu32.to_le_bytes());
        contrast.extend_from_slice(&2u32.to_le_bytes());
        contrast.extend_from_slice(&0u16.to_le_bytes());
        out.push(rail_pdu(RAIL_SYSPARAM, &contrast));
        out.push(flag(0x0021, 0)); // SPI_SETMOUSEBUTTONSWAP
        out.push(flag(0x0045, 0)); // SPI_SETKEYBOARDPREF
        out.push(flag(0x0025, 0)); // SPI_SETDRAGFULLWINDOWS
        out.push(flag(0x100B, 0)); // SPI_SETKEYBOARDCUES
        out.push(rect(0x002F)); // SPI_SETWORKAREA
        let (exe, dir, args) = (
            utf16(&self.app.program),
            utf16(&self.app.working_dir),
            utf16(&self.app.arguments),
        );
        let mut body = Vec::new();
        body.extend_from_slice(&0u16.to_le_bytes()); // Flags
        body.extend_from_slice(&(exe.len() as u16).to_le_bytes());
        body.extend_from_slice(&(dir.len() as u16).to_le_bytes());
        body.extend_from_slice(&(args.len() as u16).to_le_bytes());
        body.extend_from_slice(&exe);
        body.extend_from_slice(&dir);
        body.extend_from_slice(&args);
        out.push(rail_pdu(RAIL_EXEC, &body));
        out
    }
}

impl SvcProcessor for RailChannel {
    fn channel_name(&self) -> ChannelName {
        ChannelName::from_static(b"rail\0\0\0\0")
    }

    fn process(&mut self, payload: &[u8]) -> ironrdp_pdu::PduResult<Vec<SvcMessage>> {
        if payload.len() < 4 {
            return Ok(Vec::new());
        }
        let order = u16::from_le_bytes([payload[0], payload[1]]);
        tracing::debug!(
            order = format!("{order:#06x}"),
            length = payload.len(),
            "RemoteApp: rail PDU from the host"
        );
        match order {
            RAIL_HANDSHAKE | RAIL_HANDSHAKE_EX => {
                let mut status = self.status.lock().expect("rail status");
                if status.started {
                    return Ok(Vec::new());
                }
                status.started = true;
                status.outgoing.extend(self.start_messages());
                Ok(Vec::new())
            }
            RAIL_EXEC_RESULT if payload.len() >= 12 => {
                // Flags (2), ExecResult (2), RawResult (4), ...
                let result = u16::from_le_bytes([payload[6], payload[7]]);
                if result != 0 {
                    eprintln!("rdp: the host could not run the program (exec result {result})");
                }
                self.status.lock().expect("rail status").exec_result = Some(result);
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }
}

impl SvcClientProcessor for RailChannel {}

ironrdp_core::impl_as_any!(RailChannel);

/// One window the server described.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteWindow {
    pub id: u32,
    pub owner: u32,
    pub title: String,
    pub style: u32,
    pub ex_style: u32,
    pub show: u8,
    /// Its top-left corner on the desktop and its size.
    pub offset: (i32, i32),
    pub size: (u32, u32),
}

/// A window order: the window's new or changed fields, or its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowOrder {
    Window {
        new: bool,
        fields: u32,
        window: RemoteWindow,
    },
    Deleted(u32),
    /// An Actively Monitored or Non-Monitored Desktop order: its fields,
    /// and the window active now when it says.
    Desktop {
        fields: u32,
        active: Option<u32>,
    },
}

// MS-RDPERP 2.2.1.3.1 FieldsPresentFlags.
const ORDER_TYPE_WINDOW: u32 = 0x0100_0000;
const ORDER_TYPE_DESKTOP: u32 = 0x0400_0000;
/// MS-RDPERP 2.2.1.3.3.1: the server's desktop is being monitored and
/// described in full.
pub const DESKTOP_ARC_COMPLETED: u32 = 0x0000_0004;
/// The order names the active window.
const DESKTOP_ACTIVE_WND: u32 = 0x0000_0020;
// Window styles a listing tells windows apart by.
const WS_CAPTION: u32 = 0x00C0_0000;
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const STATE_NEW: u32 = 0x1000_0000;
const STATE_DELETED: u32 = 0x2000_0000;
const FIELD_APPBAR_EDGE: u32 = 0x0000_0001;
const FIELD_OWNER: u32 = 0x0000_0002;
const FIELD_TITLE: u32 = 0x0000_0004;
const FIELD_STYLE: u32 = 0x0000_0008;
const FIELD_SHOW: u32 = 0x0000_0010;
const FIELD_APPBAR_STATE: u32 = 0x0000_0040;
const FIELD_RESIZE_MARGIN_X: u32 = 0x0000_0080;
const FIELD_WNDRECTS: u32 = 0x0000_0100;
const FIELD_VISIBILITY: u32 = 0x0000_0200;
const FIELD_WNDSIZE: u32 = 0x0000_0400;
const FIELD_WNDOFFSET: u32 = 0x0000_0800;
const FIELD_VISOFFSET: u32 = 0x0000_1000;
const FIELD_CLIENTAREAOFFSET: u32 = 0x0000_4000;
const FIELD_CLIENTDELTA: u32 = 0x0000_8000;
const FIELD_CLIENTAREASIZE: u32 = 0x0001_0000;
const FIELD_RPCONTENT: u32 = 0x0002_0000;
const FIELD_ROOTPARENT: u32 = 0x0004_0000;
const FIELD_ENFORCE_SERVER_ZORDER: u32 = 0x0008_0000;
const FIELD_OVERLAY_DESCRIPTION: u32 = 0x0040_0000;
const FIELD_TASKBAR_BUTTON: u32 = 0x0080_0000;
const FIELD_RESIZE_MARGIN_Y: u32 = 0x0800_0000;
// Icon orders share the window type bit; they carry no window fields.
const ORDER_ICON: u32 = 0x4000_0000;
const ORDER_CACHED_ICON: u32 = 0x8000_0000;

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let bytes = self.data.get(self.at..self.at + n)?;
        self.at += n;
        Some(bytes)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn i32(&mut self) -> Option<i32> {
        Some(self.u32()? as i32)
    }
    fn unicode(&mut self) -> Option<String> {
        let len = usize::from(self.u16()?);
        let bytes = self.take(len)?;
        let units: Vec<u16> = bytes
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Some(String::from_utf16_lossy(&units))
    }
}

/// The window orders in one fast-path frame (a frame of other updates
/// gives none). Orders updates hold any kind of order; reading stops at
/// the first that is not a windowing order, whose size cannot be known.
pub fn window_orders(frame: &[u8]) -> Vec<WindowOrder> {
    use ironrdp_pdu::fast_path::{FastPathHeader, FastPathUpdatePdu, Fragmentation, UpdateCode};
    let mut out = Vec::new();
    let mut cursor = ironrdp_core::ReadCursor::new(frame);
    if ironrdp_core::decode_cursor::<FastPathHeader>(&mut cursor).is_err() {
        return out;
    }
    while !cursor.is_empty() {
        let Ok(update) = ironrdp_core::decode_cursor::<FastPathUpdatePdu<'_>>(&mut cursor) else {
            break;
        };
        if update.update_code == UpdateCode::Orders {
            tracing::trace!(
                fragmentation = ?update.fragmentation,
                compressed = update.compression_flags.is_some(),
                bytes = update.data.len(),
                "RemoteApp: fast-path orders update"
            );
        }
        if update.update_code != UpdateCode::Orders
            || update.fragmentation != Fragmentation::Single
            || update.compression_flags.is_some()
        {
            continue;
        }
        let mut reader = Reader {
            data: update.data,
            at: 0,
        };
        let Some(count) = reader.u16() else { continue };
        orders(&mut reader, count, &mut out);
    }
    out
}

/// The window orders in a slow-path frame, when it is an Orders update
/// (TS_UPDATE_ORDERS: a Share Data PDU of type Update, update type 0),
/// which IronRDP cannot decode; `None` for any other frame.
pub fn slow_path_window_orders(frame: &[u8]) -> Option<Vec<WindowOrder>> {
    let X224(indication) =
        ironrdp_core::decode::<X224<ironrdp_pdu::mcs::SendDataIndication<'_>>>(frame).ok()?;
    let mut r = Reader {
        data: indication.user_data.as_ref(),
        at: 0,
    };
    // Share Control Header: total length, PDU type (data is 7), source.
    r.u16()?;
    if r.u16()? & 0x0f != 0x07 {
        return None;
    }
    r.u16()?;
    // Share Data Header: share ID, pad, stream, length, type (update is
    // 2), compression type, compressed length.
    r.take(8)?;
    let pdu_type = r.u8()?;
    let compression = r.u8()?;
    r.u16()?;
    if pdu_type != 0x02 || compression & 0x20 != 0 {
        return None;
    }
    // TS_UPDATE_ORDERS: update type 0, pad, count, pad, orders.
    if r.u16()? != 0x0000 {
        return None;
    }
    r.u16()?;
    let count = r.u16()?;
    r.u16()?;
    let mut out = Vec::new();
    orders(&mut r, count, &mut out);
    Some(out)
}

/// Reads `count` orders, keeping the window ones, until one that is not a
/// windowing order (its size cannot be known).
fn orders(reader: &mut Reader<'_>, count: u16, out: &mut Vec<WindowOrder>) {
    for _ in 0..count {
        let start = reader.at;
        let Some(control) = reader.u8() else { break };
        // An alternate secondary order (class 0b10) of type window (0x0B).
        if control & 0x03 != 0x02 || control >> 2 != 0x0B {
            break;
        }
        let Some(size) = reader.u16() else { break };
        let end = start + usize::from(size);
        if let Some(order) = window_order(reader) {
            out.push(order);
        }
        if end > reader.data.len() {
            break;
        }
        reader.at = end;
    }
}

fn window_order(r: &mut Reader<'_>) -> Option<WindowOrder> {
    let fields = r.u32()?;
    if fields & ORDER_TYPE_DESKTOP != 0 {
        // ActiveWindowId comes first, when present (2.2.1.3.3.2.1).
        let active = (fields & DESKTOP_ACTIVE_WND != 0)
            .then(|| r.u32())
            .flatten();
        return Some(WindowOrder::Desktop { fields, active });
    }
    if fields & ORDER_TYPE_WINDOW == 0 {
        return None; // notification icon orders
    }
    let id = r.u32()?;
    if fields & STATE_DELETED != 0 {
        return Some(WindowOrder::Deleted(id));
    }
    if fields & (ORDER_ICON | ORDER_CACHED_ICON) != 0 {
        return None;
    }
    let mut window = RemoteWindow {
        id,
        ..RemoteWindow::default()
    };
    if fields & FIELD_OWNER != 0 {
        window.owner = r.u32()?;
    }
    if fields & FIELD_STYLE != 0 {
        window.style = r.u32()?;
        window.ex_style = r.u32()?;
    }
    if fields & FIELD_SHOW != 0 {
        window.show = r.u8()?;
    }
    if fields & FIELD_TITLE != 0 {
        window.title = r.unicode()?;
    }
    if fields & FIELD_CLIENTAREAOFFSET != 0 {
        r.take(8)?;
    }
    if fields & FIELD_CLIENTAREASIZE != 0 {
        r.take(8)?;
    }
    if fields & FIELD_RESIZE_MARGIN_X != 0 {
        r.take(8)?;
    }
    if fields & FIELD_RESIZE_MARGIN_Y != 0 {
        r.take(8)?;
    }
    if fields & FIELD_RPCONTENT != 0 {
        r.take(1)?;
    }
    if fields & FIELD_ROOTPARENT != 0 {
        r.take(4)?;
    }
    if fields & FIELD_WNDOFFSET != 0 {
        window.offset = (r.i32()?, r.i32()?);
    }
    if fields & FIELD_CLIENTDELTA != 0 {
        r.take(8)?;
    }
    if fields & FIELD_WNDSIZE != 0 {
        window.size = (r.u32()?, r.u32()?);
    }
    if fields & FIELD_WNDRECTS != 0 {
        let n = usize::from(r.u16()?);
        r.take(n * 8)?;
    }
    if fields & FIELD_VISOFFSET != 0 {
        r.take(8)?;
    }
    if fields & FIELD_VISIBILITY != 0 {
        let n = usize::from(r.u16()?);
        r.take(n * 8)?;
    }
    if fields & FIELD_OVERLAY_DESCRIPTION != 0 {
        r.unicode()?;
    }
    for flag in [
        FIELD_TASKBAR_BUTTON,
        FIELD_ENFORCE_SERVER_ZORDER,
        FIELD_APPBAR_STATE,
        FIELD_APPBAR_EDGE,
    ] {
        if fields & flag != 0 {
            r.take(1)?;
        }
    }
    Some(WindowOrder::Window {
        new: fields & STATE_NEW != 0,
        fields,
        window,
    })
}

/// The server's windows, kept from its orders.
#[derive(Debug, Default)]
pub struct Windows {
    windows: BTreeMap<u32, RemoteWindow>,
    /// The order windows were first seen in.
    order: Vec<u32>,
    /// The window the server last said is active.
    active: Option<u32>,
}

impl Windows {
    pub fn apply(&mut self, order: WindowOrder) {
        match order {
            WindowOrder::Desktop { active, .. } => {
                if active.is_some() {
                    self.active = active;
                }
            }
            WindowOrder::Deleted(id) => {
                self.windows.remove(&id);
                self.order.retain(|w| *w != id);
            }
            WindowOrder::Window { fields, window, .. } => {
                let entry = self.windows.entry(window.id).or_insert_with(|| {
                    self.order.push(window.id);
                    RemoteWindow {
                        id: window.id,
                        ..RemoteWindow::default()
                    }
                });
                if fields & FIELD_OWNER != 0 {
                    entry.owner = window.owner;
                }
                if fields & FIELD_STYLE != 0 {
                    entry.style = window.style;
                    entry.ex_style = window.ex_style;
                }
                if fields & FIELD_SHOW != 0 {
                    entry.show = window.show;
                }
                if fields & FIELD_TITLE != 0 {
                    entry.title = window.title;
                }
                if fields & FIELD_WNDOFFSET != 0 {
                    entry.offset = window.offset;
                }
                if fields & FIELD_WNDSIZE != 0 {
                    entry.size = window.size;
                }
            }
        }
    }

    /// The windows a client lists, in the order they appeared: every shown
    /// window with a size is its own entry (docs/BACKENDS.md, "Window by
    /// window, always"), naming its owner. A menu, tooltip or drop-down
    /// (an untitled tool window) that Windows describes without an owner
    /// is owned by the window active when it opened.
    pub fn listed(&self) -> Vec<ListedWindow> {
        let shown = |w: &RemoteWindow| w.show != 0 && w.size.0 > 0 && w.size.1 > 0;
        self.order
            .iter()
            .filter_map(|id| self.windows.get(id))
            .filter(|w| shown(w))
            .map(|w| {
                let popup = w.title.is_empty() && w.ex_style & WS_EX_TOOLWINDOW != 0;
                let owner = if self.windows.contains_key(&w.owner) {
                    Some(w.owner)
                } else if popup {
                    self.active
                        .filter(|a| *a != w.id && self.windows.contains_key(a))
                } else {
                    None
                };
                let kind = match owner {
                    None => WindowKind::Normal,
                    Some(_) if !popup && w.style & WS_CAPTION == WS_CAPTION => WindowKind::Dialog,
                    Some(_) => WindowKind::Popup,
                };
                ListedWindow {
                    id: w.id,
                    title: w.title.clone(),
                    rect: Rect::of(w),
                    owner,
                    kind,
                }
            })
            .collect()
    }
}

/// A rectangle on the RemoteApp session's desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    fn of(window: &RemoteWindow) -> Rect {
        Rect {
            x: window.offset.0,
            y: window.offset.1,
            width: window.size.0,
            height: window.size.1,
        }
    }
}

/// One window of a RemoteApp session as a client lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedWindow {
    /// The server's window id.
    pub id: u32,
    pub title: String,
    /// The window on the session's desktop.
    pub rect: Rect,
    /// The server's id of the window that owns it.
    pub owner: Option<u32>,
    pub kind: WindowKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A New Window order as a server writes it: owner, style, show,
    /// title, window offset and size.
    fn new_window_order(id: u32, title: &str, offset: (i32, i32), size: (u32, u32)) -> Vec<u8> {
        let fields = ORDER_TYPE_WINDOW
            | STATE_NEW
            | FIELD_OWNER
            | FIELD_STYLE
            | FIELD_SHOW
            | FIELD_TITLE
            | FIELD_WNDOFFSET
            | FIELD_WNDSIZE
            | FIELD_VISIBILITY;
        let mut body = Vec::new();
        body.extend_from_slice(&fields.to_le_bytes());
        body.extend_from_slice(&id.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // owner
        body.extend_from_slice(&0x16CF_0000u32.to_le_bytes()); // style
        body.extend_from_slice(&0u32.to_le_bytes()); // extended style
        body.push(5); // shown
        let title = utf16(title);
        body.extend_from_slice(&(title.len() as u16).to_le_bytes());
        body.extend_from_slice(&title);
        body.extend_from_slice(&offset.0.to_le_bytes());
        body.extend_from_slice(&offset.1.to_le_bytes());
        body.extend_from_slice(&size.0.to_le_bytes());
        body.extend_from_slice(&size.1.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes()); // one visibility rect
        body.extend_from_slice(&[0; 8]);
        let mut order = vec![0x2E];
        order.extend_from_slice(&((3 + body.len()) as u16).to_le_bytes());
        order.extend_from_slice(&body);
        order
    }

    fn fast_path_orders(orders: &[Vec<u8>]) -> Vec<u8> {
        let mut data = (orders.len() as u16).to_le_bytes().to_vec();
        for order in orders {
            data.extend_from_slice(order);
        }
        // TS_FP_UPDATE: updateHeader (code 0, single), size, data.
        let mut update = vec![0x00];
        update.extend_from_slice(&(data.len() as u16).to_le_bytes());
        update.extend_from_slice(&data);
        // Fast-path header: action 0, then a two-byte length.
        let total = 3 + update.len();
        let mut frame = vec![0x00, 0x80 | (total >> 8) as u8, total as u8];
        frame.extend_from_slice(&update);
        frame
    }

    #[test]
    fn every_window_is_listed_on_its_own_with_its_owner() {
        let window = |id, owner, title: &str, style, ex_style, offset, size| RemoteWindow {
            id,
            owner,
            title: title.into(),
            style,
            ex_style,
            show: 5,
            offset,
            size,
        };
        let mut windows = Windows::default();
        windows.apply(WindowOrder::Desktop {
            fields: ORDER_TYPE_DESKTOP | DESKTOP_ACTIVE_WND,
            active: Some(1),
        });
        for w in [
            window(1, 0, "Notepad", WS_CAPTION, 0, (100, 100), (400, 300)),
            // A menu: untitled, a tool window, no owner given.
            window(2, 0, "", 0, WS_EX_TOOLWINDOW, (90, 380), (500, 100)),
            // A dialog it owns.
            window(3, 1, "Save as", WS_CAPTION, 0, (150, 150), (200, 100)),
            // A shell helper with no size, and a hidden window.
            window(4, 0, "", 0, 0, (0, 0), (0, 0)),
            RemoteWindow {
                show: 0,
                ..window(5, 0, "Program Manager", 0, 0, (0, 0), (1280, 720))
            },
        ] {
            windows.apply(WindowOrder::Window {
                new: true,
                fields: FIELD_OWNER
                    | FIELD_TITLE
                    | FIELD_STYLE
                    | FIELD_SHOW
                    | FIELD_WNDOFFSET
                    | FIELD_WNDSIZE,
                window: w,
            });
        }
        let rect = |x, y, width, height| Rect {
            x,
            y,
            width,
            height,
        };
        assert_eq!(
            windows.listed(),
            vec![
                ListedWindow {
                    id: 1,
                    title: "Notepad".into(),
                    rect: rect(100, 100, 400, 300),
                    owner: None,
                    kind: WindowKind::Normal,
                },
                ListedWindow {
                    id: 2,
                    title: String::new(),
                    rect: rect(90, 380, 500, 100),
                    owner: Some(1),
                    kind: WindowKind::Popup,
                },
                ListedWindow {
                    id: 3,
                    title: "Save as".into(),
                    rect: rect(150, 150, 200, 100),
                    owner: Some(1),
                    kind: WindowKind::Dialog,
                },
            ]
        );
        windows.apply(WindowOrder::Deleted(3));
        windows.apply(WindowOrder::Deleted(2));
        assert_eq!(windows.listed().len(), 1);
    }

    #[test]
    fn the_desktop_order_says_when_the_desktop_is_ready() {
        // An Actively Monitored Desktop order with ARC_COMPLETED and an
        // active window (the order's own fields follow and are skipped).
        let fields = ORDER_TYPE_DESKTOP | DESKTOP_ARC_COMPLETED | 0x20;
        let mut body = fields.to_le_bytes().to_vec();
        body.extend_from_slice(&7u32.to_le_bytes());
        let mut order = vec![0x2E]; // alternate secondary order, window
        order.extend_from_slice(&((3 + body.len()) as u16).to_le_bytes());
        order.extend_from_slice(&body);
        let frame = fast_path_orders(&[order]);
        assert_eq!(
            window_orders(&frame),
            vec![WindowOrder::Desktop {
                fields,
                active: Some(7)
            }]
        );
    }

    #[test]
    fn a_new_window_and_its_deletion_are_read() {
        let mut deleted = vec![0x2E];
        let mut body = (ORDER_TYPE_WINDOW | STATE_DELETED).to_le_bytes().to_vec();
        body.extend_from_slice(&7u32.to_le_bytes());
        deleted.extend_from_slice(&((3 + body.len()) as u16).to_le_bytes());
        deleted.extend_from_slice(&body);
        let frame = fast_path_orders(&[
            new_window_order(7, "Untitled - Notepad", (100, 50), (640, 480)),
            deleted,
        ]);
        let orders = window_orders(&frame);
        assert_eq!(orders.len(), 2, "{orders:?}");
        let mut windows = Windows::default();
        windows.apply(orders[0].clone());
        let listed = windows.listed();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "Untitled - Notepad");
        assert_eq!(
            listed[0].rect,
            Rect {
                x: 100,
                y: 50,
                width: 640,
                height: 480
            }
        );
        windows.apply(orders[1].clone());
        assert!(windows.listed().is_empty());
    }

    #[test]
    fn the_client_info_pdu_asks_for_remoteapp() {
        let mut user_data = vec![0x40, 0x00, 0x00, 0x00]; // INFO_PKT
        user_data.extend_from_slice(&0u32.to_le_bytes()); // code page
        user_data.extend_from_slice(&0x0000_0003u32.to_le_bytes()); // flags
        user_data.extend_from_slice(&[0; 20]);
        let pdu = ironrdp_core::encode_vec(&X224(SendDataRequest {
            initiator_id: 1007,
            channel_id: 1003,
            user_data: user_data.into(),
        }))
        .unwrap();
        let mut tap = RailTap::new(Vec::new());
        tap.write_all(&pdu).unwrap();
        let X224(sent) = ironrdp_core::decode::<X224<SendDataRequest<'_>>>(&tap.inner).unwrap();
        let flags = u32::from_le_bytes(sent.user_data[8..12].try_into().unwrap());
        assert_eq!(flags, 0x0000_0003 | INFO_RAIL);
        // Later PDUs go out untouched.
        let mut again = RailTap::new(Vec::new());
        again.info_done = true;
        again.caps_done = true;
        again.write_all(&pdu).unwrap();
        assert_eq!(again.inner, pdu);
    }
}

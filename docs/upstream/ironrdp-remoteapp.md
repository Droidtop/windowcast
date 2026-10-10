# Draft upstream pull request: RemoteApp (RAIL) client support in IronRDP

**For:** IronRDP (Devolutions), against the published crates windowcast
uses (`ironrdp-connector` 0.10, `ironrdp-pdu` 0.9, `ironrdp-session`
0.11). **Status:** draft for the owner to review and file; not filed.
windowcast keeps its own RemoteApp handling (the `rail` static channel,
which windows to show, input) on top; this is only what the protocol layer
needs to let a client do RemoteApp at all.

## Title

connector, pdu, session: let a client ask for RemoteApp and see the
server's windows (INFO_RAIL, the RAIL and Window List capability sets,
Windowing Alternate Secondary Drawing Orders)

## Summary

A RemoteApp (RAIL) client needs three things from the protocol layer that
IronRDP has no way to do today:

1. **Ask for RemoteApp in the Client Info PDU.** `ClientInfoFlags::RAIL`
   (INFO_RAIL, 0x00008000) exists in `ironrdp-pdu`, but
   `ironrdp-connector` builds the flags itself
   (`connection.rs`, `create_client_info_pdu`: MOUSE, MOUSE_HAS_WHEEL,
   UNICODE, ...) and `Config` has no field that sets it.
2. **Advertise RemoteApp in the Confirm Active PDU.** The client must send
   the Remote Programs Capability Set (MS-RDPERP 2.2.1.1.1,
   CAPSTYPE_RAIL 0x0017: `RailSupportLevel`, at least
   TS_RAIL_LEVEL_SUPPORTED 0x1) and the Window List Capability Set
   (2.2.1.1.2, CAPSTYPE_WINDOW 0x0018: `WndSupportLevel`
   TS_WINDOW_LEVEL_SUPPORTED 0x1 or _EX 0x2, `NumIconCaches` u8,
   `NumIconCacheEntries` u16). The connector's capability list is fixed.
3. **Read the server's window list.** With RAIL the server describes its
   windows in Windowing Alternate Secondary Drawing Orders (MS-RDPERP
   2.2.1.3), which arrive in Orders updates: the fast-path
   TS_FP_UPDATE_ORDERS (update code 0x0, a `numberOrders` u16 then the
   orders) and the slow-path TS_UPDATE_ORDERS (update type 0x0000).
   `FastPathUpdate` has no Orders variant, so these are dropped, and the
   client never learns which windows exist, where they are or what they
   are called.

## Proposed change

- `ironrdp-connector`: `Config::remote_app: Option<RemoteAppConfig>`
  (`rail_support_level: u32`, `window_support_level: u32`,
  `icon_caches: u8`, `icon_cache_entries: u16`). When set, the Client Info
  PDU carries `ClientInfoFlags::RAIL` (and `HIDEF_RAIL_SUPPORTED` when
  asked), and the Confirm Active PDU carries the two capability sets.
  `None` keeps today's behaviour exactly.
- `ironrdp-pdu`: `CapabilitySet::Rail(RailCapability)` and
  `CapabilitySet::WindowList(WindowListCapability)` with encode/decode;
  `FastPathUpdate::Orders(Vec<Order>)` and the slow-path equivalent, where
  an `Order` is either an alternate secondary window order or left raw;
  and the window orders themselves:
  - the common header TS_WINDOW_ORDER_HEADER (2.2.1.3.1.1): the
    alternate secondary order header byte (class TS_SECONDARY 0x2,
    `orderType` 0x0B TS_ALTSEC_WINDOW in the upper six bits, so 0x2E),
    `OrderSize` u16, `FieldsPresentFlags` u32, `WindowId` u32;
  - New or Existing Window (2.2.1.3.1.2.1): the optional fields in the
    order the specification lists them, each behind its
    `FieldsPresentFlags` bit (owner 0x2, style 0x8, show 0x10, title 0x4,
    client area offset 0x4000, client area size 0x10000 (only with
    TS_WINDOW_LEVEL_SUPPORTED_EX), resize margins X 0x80 and Y 0x08000000,
    RP content 0x20000, root parent 0x40000, window offset 0x800, client
    delta 0x8000, window size 0x400, window rects 0x100, visible offset
    0x1000, visibility rects 0x200, overlay description 0x400000, taskbar
    button 0x800000, enforce server z-order 0x80000, app bar state 0x40,
    app bar edge 0x1), with WINDOW_ORDER_TYPE_WINDOW 0x01000000 and
    WINDOW_ORDER_STATE_NEW 0x10000000;
  - Deleted Window (2.2.1.3.1.2.4), Window Icon and Cached Icon
    (2.2.1.3.1.2.2, .3) at least skipped by `OrderSize` so the stream
    stays in step;
  - notification icon and desktop orders (2.2.1.3.2, 2.2.1.3.3) skipped
    the same way, or decoded if wanted.
- `ironrdp-session`: `ActiveStageOutput::WindowOrders(Vec<WindowOrder>)`
  when an Orders update carries window orders, so the application keeps
  its own window list; other orders keep being ignored.

- `ironrdp-svc`: a way for an `SvcProcessor` to declare its channel's
  options (`fn channel_options(&self) -> ChannelOptions`, defaulting to
  today's `make_channel_options`), and for `encode_svc_messages` to set
  CHANNEL_FLAG_SHOW_PROTOCOL on every chunk when SHOW_PROTOCOL is among
  them. `make_channel_options` derives the options from the compression
  condition alone, so `rail` goes out with options 0. A Windows host's
  RemoteApp process (rdpinit) then sends its handshake but ignores every
  client PDU: no Execute Result, no window orders. With the options
  FreeRDP and mstsc use (INITIALIZED | ENCRYPT_RDP | COMPRESS_RDP |
  SHOW_PROTOCOL, FreeRDP channels/rail/client/rail_main.c:715, and
  CHANNEL_FLAG_SHOW_PROTOCOL per chunk, libfreerdp/core/channels.c:99) the
  same session runs the program.

The `rail` static virtual channel itself (handshake, client status,
execute, system parameters, activate, ...) can already be added by an
application through `ClientConnector::with_static_channel`, so this
proposal leaves it to the application (a separate `ironrdp-rail` crate
could come later).

## Tests

- Round trips of both capability sets and of a New or Existing Window
  order with every optional field present, and with none.
- An Orders fast-path update with a window order followed by an unknown
  order: the window order decodes, the unknown one is skipped by its size.
- Connector: with `remote_app` set, the Client Info PDU has INFO_RAIL and
  the Confirm Active PDU both capability sets; with it unset, the bytes
  are as before.

## Why

Clients built on IronRDP can show a remote desktop but not a single
remote application (mstsc's and FreeRDP's RemoteApp mode), which is what a
client wants on a small screen or when it shows one application among its
own windows. windowcast uses IronRDP for RDP and shows single windows, so
it needs exactly this; until the change is merged it sets the flag, the
capability sets and the `rail` channel's options by rewriting those three
PDUs (Client Info, Confirm Active, Connect Initial) on their way out and
reads Orders updates before IronRDP sees them, which proper options would
make unnecessary.

## References

- MS-RDPERP 2.2.1.1.1 (Remote Programs Capability Set), 2.2.1.1.2 (Window
  List Capability Set), 2.2.1.3 (Windowing Alternate Secondary Drawing
  Orders).
- MS-RDPBCGR 2.2.1.11.1.1 (TS_INFO_PACKET, INFO_RAIL), 2.2.9.1.2.1.1
  (TS_FP_UPDATE_ORDERS), 2.2.9.1.1.3.1.1 (TS_UPDATE_ORDERS).
- MS-RDPEGDI 2.2.2.2.1.3.1.1 (Alternate Secondary Order Header).

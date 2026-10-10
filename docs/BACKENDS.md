# Backends: one library, a protocol per window

windowcast is **one library** we write: the protocol, pairing and
identity, the backends, codecs and input. The ends are thin
implementations of it: host agents (Linux, Windows, macOS) on one side,
clients (droidtop's Desktop mode, the reference CLI, anything else) on the
other. A client embeds `client-core` and talks to it only through its C
interface, which is the single surface every end implements against.

Every backend below lives inside this library, on both ends, and none
wraps another project's client or server program (no moonlight-common-c,
no FreeRDP, no libvncclient). Other projects' source is reference material
for how a protocol behaves on the wire. GameStream is written here from
those references.

Libraries are another matter (the owner's decision, 2026-10-09: "We can
wire in other crates, that's fine"): a backend may link a protocol crate
as its protocol layer, never an external program, with each crate's
licence kept straight in `NOTICE`. RDP is built that way: IronRDP's
crates (Devolutions, MIT or Apache-2.0) carry the RDP protocol itself
(connection sequence, TLS, CredSSP/NLA, the bitmap codecs, the static and
dynamic channels), and windowcast keeps its own window handling on top:
which window an RDP connection shows, RemoteApp, input mapping, the
streaming into windowcast's sessions and the backend seam.

## The window abstraction

A client sees windows (`WindowInfo`: id, title, app id, size, focus, and a
content hint) and asks to stream one (`StreamStartRequest` with
`StreamOptions`: the backend it would like, the codecs it can decode, and
its own ceilings for the stream's bitrate, frame rate and height).
The host answers with the backend it actually used (`StreamStartResponse`).
The user never needs to know which backend that was. Input (#108) goes
back on one channel whatever the backend, and pairing happens once per
device pair, not per backend.

The native session is always there: it is the control plane for every
other backend. Pairing, identity, the window list, signaling and input
ride it; a backend with its own connection (GameStream, RDP) is negotiated
over it and keyed from it.

## The backends

| Backend | For | Carried on | State |
|---|---|---|---|
| `Native` | everything by default | a video track on the session; the host captures and encodes the window | built: tracks, codecs, keyframe requests; capture and encode per host OS are the agents' work (#104, #105) |
| `Passthrough` | video players | a video track on the session, carrying the media as it was already encoded (no second encode) | seam only |
| `Desktop` | hosts that cannot capture one window; whole-desktop streams | a video track on the session, cut from a whole-output capture | built on Windows and on Linux under sway: the window cut from a capture of its screen, with whatever covers it; whole-desktop targets are #112 |
| `GameStream` | games | its own low-latency video, audio and controller channels | video, sound and input built at both ends (the `gamestream` crate) (#110) |
| `Rdp` | text-heavy windows: editors, terminals, documents | its own connection; sharp text at low bandwidth | built (#111): a session hands the window to an RDP server started for that stream (see below); clients get RGBA pictures |
| `Vnc` | anything else that only speaks VNC | its own connection | seam only (#111) |

Video tracks carry H.264, H.265 or AV1, chosen per track: the client lists
what it decodes in hardware, most preferred first, and the host picks the
first it can produce (for passthrough, the codec the media already is).

## Handing a window to RDP

A backend with its own connection is negotiated over the session and
keyed from it. For RDP:

1. The client's rules choose `Rdp` for a window and it asks for the
   stream as usual. It asks for `Native` instead when it has not said it
   shows pictures (`accept_pictures`; the Android viewer does, the app's
   own client not yet) and away from the LAN (a punched session): RDP is a
   TCP connection of its own to the host.
2. The host (any `WindowSource` wrapped in `rdp::host::WithRdp`, which the
   app, the agents and the test host do; the app's "Offer windows over RDP"
   setting switches it) starts an RDP server for that one window on a port
   of its own, with a login made for the stream (a random password) and
   the host's RDP certificate, and answers with a `HandoffTarget`: the
   port, the login and the certificate's SHA-256.
3. The client logs in with TLS and NLA, pinning that certificate, and
   only then reports the stream started; a failed login refuses it and
   tells the host to stop. Its pictures come out of `client-core` as RGBA
   (`next_picture`, `windowcast_session_next_picture`), and the input it
   sends for that window (pointer, keys, text) goes over RDP; gamepads stay
   on the session.
4. Stopping the stream (either side, or the session ending) stops the RDP
   server. The window's picture size is fixed when the client logs in.

The RDP desktop is the window, so any RDP client given that login sees
only that window, and the host's own input rules apply (the app's input
setting gates RDP input as it does session input).

## RemoteApp: programs and windows over RDP (Windows hosts)

Decided 2026-10-10 (Droidtop/tracker#111), and changed the same day at
the owner's word: "I don't even really necessarily want a different
session on Windows, since that's often locked behind licensing". A
RemoteApp of Windows' own Remote Desktop always runs in a session of its
own, and Windows licenses sessions: Windows 10 and 11 allow one (a Remote
Desktop login takes over the console), and a server more than two only
with Remote Desktop Services licences. So:

- **(a) The main RDP path: windows on the host's own desktop over
  windowcast's own RDP server.** A launched program runs in the user's
  existing session on the host's desktop, on every system, like any other
  window; a window the rules give RDP, launched or already open, is served
  by windowcast's own one-window RDP server (`WithRdp`, "Handing a window
  to RDP" above): its captured picture is the RDP desktop. (Speaking
  `rail` on that server for mstsc and FreeRDP is dropped: see "One
  window, any carrier" below.)
- **(b) Windows RemoteApps, opt-in.** A Windows host's owner may turn on
  RemoteApp launches: a launched program the client's rules give RDP then
  runs as a RemoteApp of Windows' own Remote Desktop, in a Remote Desktop
  session of its own and not on the host's screen. The client logs in to
  the host's Remote Desktop itself, asks for the program (the `rail`
  channel), and each window Windows describes in its window orders
  becomes a window in the client's window list, drawn from the RemoteApp
  desktop's picture, with input mapped to it. Off by default on every
  Windows edition.

Whole-desktop RDP stays what a client gets when it asks for the desktop
(`StreamTarget::Desktop`).

**Window by window, always.** The owner, verbatim: "No, we still want
separate windows. That's the whole point of windowcast. We should be able
to selectively stream whichever windows we want". Whatever runs a program
(the host's own desktop, or a RemoteApp session) and whichever backend
carries it, each of its top-level windows, dialogs, popups and menus is
its own entry in the window list, with its owner named, and a client
chooses and shows each one on its own. A client never gets a whole-desktop
picture unless it asks for the desktop.

How it works:

- `WindowInfo` names each window's `owner` and `kind` (normal, dialog,
  popup, menu).
- The Windows agent lists, besides the Alt+Tab windows, every shown
  window they own (dialogs, drop-downs) and the menus, tooltips and
  drop-downs their threads show (owned by the window active in that
  thread, or the one the menu is for). Windows.Graphics.Capture will not
  capture an owned window, a menu or a tooltip on its own ("Could not
  capture the given window", 0x80070057, for a dialog and a drop-down on
  the CI runner), so such a window is captured from its part of the
  screen, which shows it as it is drawn, on top.
- A RemoteApp connection lists every window Windows describes, a menu or
  tooltip owned by the window active when it opened.
- The host sends the window list again whenever it changes, once a client
  has asked for it, so a popup reaches the client while it is open.
- A client shows the dialogs, popups and menus a shown window owns, each
  as its own window, as they open, and stops them with their owner
  (`ClientSession::set_follow_popups`, on by default; the Windows app's
  "Open a streamed window's dialogs, popups and menus"; the Android viewer
  in floating windows). Off, they are only listed, for the user to pick.
- On Wayland (the Linux agent) a popup is part of its top-level window's
  surface: the compositor lists top-level windows only, and a popup or
  menu shows inside its window's picture.

### How a launch becomes a RemoteApp

1. The client evaluates its rules on the program as a window would be
   (`app_id` from the program's file name, content from
   `selection::classify`). If they give `Rdp`, the client shows pictures,
   and the host is on the same network, the `Launch` asks for a RemoteApp.
2. The host serves it as a RemoteApp only when its owner turned RemoteApp
   launches on (below) and Remote Desktop is on; otherwise, and by
   default, it starts the program in the user's own session as a plain
   launch, and the program's windows stream as usual, over (a) when the
   rules give them RDP.
3. The host answers with a `HandoffTarget` for its Remote Desktop: the
   address, port 3389 (or the configured port), the Windows user to log
   in as, a password only for a host-made account, and the SHA-256 of
   Remote Desktop's certificate, which the host reads by connecting to
   itself, so the client pins it.
4. The client logs in with TLS and NLA and runs the program; the windows
   it reports join the window list. When the program's last window
   closes, the client ends the RDP connection.

### Logging in: one host setting

- **As the signed-in user (default).** A client that signed in with the
  host's own Windows account (the `os` account source: PAM or
  LogonUserW) reuses that password: the client keeps it in memory for
  the session only and never stores it, and the host hands over only the
  user name. A client that signed in another way (PIN pairing, OIDC,
  LDAP, a windowcast account) is asked for the Windows password when it
  launches (`ClientError::PasswordNeeded`, which the app turns into a
  prompt); a PIN-paired device logs in as the user the host runs as.
  Kerberos with the user's ticket on a domain-joined host is planned and
  not built.
- **Host-made account.** The host creates one local Windows user per
  windowcast account (and one per PIN-paired device), with a random
  password the host keeps protected by DPAPI, adds it to "Remote Desktop
  Users", and hands that login over. Launches then need no Windows
  password from the user; it suits Windows Server and hosts many people
  use. Creating users needs the host to run with administrator rights;
  without them the host says so and refuses the RemoteApp.

### When it is on

Off by default on every Windows edition. The host's setting has three
values: off; on; and automatic, which is on for a Windows Server with a
session free (the Remote Desktop Session Host role is installed, or fewer
than the two sessions a server without it allows are in use) and off on
Windows 10 and 11.

- Windows 10 and 11 (client editions) allow one session: a Remote Desktop
  login takes over the console and locks the local screen. Turning
  RemoteApp launches on there shows a plain warning saying exactly that.
- Windows Server: more than two sessions need the Session Host role and
  Remote Desktop Services licences.
- Remote Desktop must be on, and Windows must let the program run as a
  RemoteApp (an allow-list entry, or the allow list switched off); the
  host's settings say what is missing.

## One window, any carrier: live switching (design, Droidtop/tracker#457)

Decided 2026-10-10 by the owner: "That design is wrong. We want the same
behavior for all protocols. That's kinda the point, seamless switching
between protocols and bitrates and stuff to optimize connections". This
section is the design; it replaces per-protocol window behaviour
(including the planned rail on windowcast's own RDP server, now dropped).
Not built yet: today (0.25.0) a window's backend is chosen once, when its
stream starts.

### The session owns the window

A window is a session object, the same whatever carries its picture:

| Owned by the session | Today | Under this design |
|---|---|---|
| Identity, title, owner and kind, popups | session (`WindowInfo`, 0.25.0) | unchanged |
| Geometry and z-order | size only (`WindowResized`) | position, size and stacking per window (`WindowGeometry`), so a client can place popups and menus where they belong |
| Focus | `WindowFocused`, input focus | unchanged |
| Input (pointer, touch, keys, text) | session, except RDP: client-core sends a window's input over its RDP connection and the RDP server injects it | always the session (`ControlMessage::Input`), one set of rules (input gate, allowed windows, focus); a carrier never carries a windowcast client's input |
| Gamepads | session (`GamepadSink`) | unchanged; a GameStream carrier's controller channel is not used by windowcast clients |
| Audio | session audio track per window | unchanged; carriers carry pictures only (GameStream's own audio only for Moonlight) |
| Clipboard | session | unchanged |
| Cursor | not sent: whatever the capture draws | session `Cursor { window, shape, position, visible }`; the host captures without the cursor and the client draws it, so it looks the same on every carrier and moves at input latency |
| Picture | the backend | the carrier: the only per-carrier part |

A carrier is then any way of getting a window's pictures to the client:
Native (video track on the session), Passthrough, Desktop cut-out,
GameStream video, RDP pictures, VNC. Each is interchangeable per window.
Bitrate, frame rate and resolution adapt continuously within a carrier
(host-core `quality`, as today).

On the host, one capture per window feeds every carrier of that window
(a fan-out), so two carriers during a switch do not capture twice and
both show the same picture.

### Switching a carrier: make-before-break (protocol 10)

A window's stream gets a generation number; two generations of one
window may run at once.

1. `CarrierSwitchRequest { window, generation: n+1, options }` (client to
   host; `options` as `StreamOptions`: the carrier, codecs, limits).
2. The host starts generation n+1 from the same capture and answers
   `CarrierStarted { window, generation, backend, handoff }` (a handoff
   for carriers with their own connection, such as RDP), or
   `CarrierRefused { window, generation, reason }`.
3. The client brings the new carrier up beside the old one, decodes it
   off screen, and when it has a picture at the window's current size
   swaps the view to it in one frame. Then it sends
   `CarrierStop { window, generation: n }`.
4. If generation n+1 shows no picture within 5 s, the client stops it
   and keeps n (the switch failed; see backoff below).

Input, audio, clipboard, cursor and the window's own events never move:
they are the session's. The existing `StreamStartRequest` and
`StreamStopRequest` become generation 1 and "every generation".

### When a carrier switches, and why it does not flap

The client's selector chooses (it knows its decoders, its screen and the
user's rules); the host reports what only it sees.

Signals, per window:
- Content, measured on the host from the capture: share of the picture
  changing per second (motion), video-like regions, text-like content
  (the classifier and app hints as today), sent in `StreamQuality`.
- Connection: round trip, loss, jitter and the bandwidth estimate (both
  ends), and whether the network allows a carrier's own connection (RDP
  and GameStream need a direct TCP/UDP path; away from the LAN only
  session carriers qualify).
- Cost: decoder and encoder availability and load on each end, battery
  (a handheld on battery prefers hardware video decode).
- The user: a per-window or per-app pinned carrier or quality turns
  automatic switching off for it.

Each candidate carrier gets a score from these; the current carrier is
replaced only when another scores better by a margin (25%) for a dwell
time (3 s), at most once per 15 s per window. After a failed or reverted
switch, that carrier waits twice as long before it is tried again for
that window (up to 5 min). Bitrate, frame rate and resolution adapt
within the current carrier first; a carrier switch is for when
adaptation alone cannot fit the content or the link (a text window on a
link too thin for legible video goes to RDP pictures; a window that
starts playing video goes back to video).

### What the client shows

The same window object throughout: the app's window or view, its title,
size, position, popups and input stay put. The client keeps two picture
paths per window (video decoding and RGBA pictures), each able to draw
the same view: the Windows client presents either into the same swap
chain; the Android viewer keeps two stacked surfaces and swaps which one
is visible on the new carrier's first picture. The window's size is the
session's (`WindowGeometry`), and each carrier's picture is scaled to it,
so a carrier sending a smaller picture does not resize the window. A
switch is an event for statistics (`CarrierChanged { window, backend,
codec }`), not something the user sees.

### Third-party clients and RemoteApp

- Plain RDP, GameStream (Moonlight) and VNC clients are not windowcast
  sessions: they have no session window model and cannot switch. The
  host keeps compatibility endpoints for them (an RDP server for one
  window as its desktop, the GameStream server's app list), each with its
  own input and audio, gated by the same host rules. Seamless windows for
  those clients (RDP rail) are not pursued: seamless windows are a
  property of windowcast sessions, the same on every carrier.
- Windows RemoteApp launches (opt-in, above) move to the host: the host
  logs in to its own Remote Desktop as the RemoteApp client and serves
  those windows as host windows, so they too are carried by any carrier
  and switch like any other, instead of reaching the client over RDP only.

## Choosing a backend

`windowcast_protocol::selection` holds the rules, so both ends agree:

1. The host classifies each window when it lists windows (`classify`):
   `Game` for Steam games (app id `steam_app_<id>`) and gamescope, `Text`
   for known terminals and editors, `Video` for known video players and
   for browsers whose title names a video site, `General` otherwise. The app lists are in `selection.rs`.
2. The client chooses (`choose_backend`): the user's own rules first, then
   the defaults. A per-app override is a user rule naming that app id; a
   rule can also match on title text or content hint.
3. The host serves the choice if it can, and falls back to `Native`
   otherwise (`serve`); the response says which one it used.

Default rules:

| Content | Backend |
|---|---|
| `Text` | `Rdp` |
| `Game` | `GameStream` |
| `Video` | `Passthrough` |
| `General` | `Native` |

Until a backend is built, hosts do not list it as available, so every
request falls back to `Native` and nothing breaks while the others arrive.
Clients expose the user rules as options with these defaults; droidtop's
Desktop mode is one such client.

## Commands and terminals

A shell, an application launch and other remote commands are not a
backend of a window: they are channels of the command stream
(docs/COMMAND-STREAM.md), on the session and to any SSH server. Selection
never chooses them and no default rule leads to them; a launched
application's windows appear in the window list and are then chosen a
backend like any other.

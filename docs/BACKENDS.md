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
   (`next_picture`, `windowcast_session_next_picture`). Its input (pointer,
   keys, text, gamepads), like every window's, goes over the session, and
   its sound comes on the session's audio track.
4. Stopping the stream (either side, or the session ending) stops the RDP
   server. The window's picture size is fixed when the client logs in.

The RDP desktop is the window, so any RDP client given that login sees
only that window. A carrier's RDP server takes no input from its RDP
connection (`WindowServer::input` off): a client could otherwise drive
the window past the session's input rules with the login it was handed.
Only an RDP server for third-party clients (compatibility mode,
`windowcast-rdp serve`) takes input, under the host's input rules.

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
  session of its own and not on the host's screen. The host logs in to
  its own Remote Desktop as that RemoteApp's client, asks for the program
  (the `rail` channel), and each window Windows describes in its window
  orders becomes a window in the host's window list, drawn from the
  RemoteApp desktop's picture, with input mapped to it: any client
  streams it on any carrier, as it would a window on the host's screen
  (`windowcast_rdp::remoteapp_windows`). Off by default on every Windows
  edition.

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
   `selection::classify`). If they give `Rdp`, the `Launch` asks for a
   RemoteApp, with the Windows password the client has (below).
2. The host serves it as a RemoteApp only when its owner turned RemoteApp
   launches on (below) and Remote Desktop is on; otherwise, and by
   default, it starts the program in the user's own session as a plain
   launch, and the program's windows stream as usual, over (a) when the
   rules give them RDP.
3. The host logs in to its own Remote Desktop (loopback, port 3389 or
   the configured one) with TLS and NLA, pinning the certificate it read
   by connecting to itself, as the Windows user below, and runs the
   program there. Without the password it needs it answers
   `PasswordNeeded` with the user's name, and the client asks the user.
4. The launch is answered once Windows started the program. The windows
   it reports join the host's window list (the host sends the list as it
   changes); each streams like the host's own windows, as video encoded
   from its pictures or as RDP pictures, and takes input over the
   session. When the program's last window closes, the host ends the
   RDP connection. RemoteApp windows have no sound yet.

### Logging in: one host setting

- **As the signed-in user (default).** A client that signed in with the
  host's own Windows account (the `os` account source: PAM or
  LogonUserW) reuses that password: the client keeps it in memory for
  the session only, never stores it, and sends it with the launch, which
  the host uses for that one login and keeps nowhere. A client that
  signed in another way (PIN pairing, OIDC, LDAP, a windowcast account)
  is asked for the Windows password when the host answers
  `PasswordNeeded` (`ClientError::PasswordNeeded`, which the app and the
  viewer turn into a prompt); a PIN-paired device logs in as the user
  the host runs as.
  Kerberos with the user's ticket on a domain-joined host is planned and
  not built.
- **Host-made account.** The host creates one local Windows user per
  windowcast account (and one per PIN-paired device), with a random
  password the host keeps protected by DPAPI, adds it to "Remote Desktop
  Users", and logs in with it. Launches then need no Windows
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
| Geometry and z-order | size only (`WindowResized`) | position, size and stacking per window, so a client can place popups and menus where they belong. Built: `WindowInfo::position` (protocol 11; Windows hosts and RemoteApp windows give it, Wayland does not), the list sent again when it changes, and the list in stacking order; the Windows client opens a popup's window borderless against its owner's stream window, the viewer its floating window against the watched one |
| Focus | `WindowFocused`, input focus | unchanged |
| Input (pointer, touch, keys, text) | session, except RDP: client-core sends a window's input over its RDP connection and the RDP server injects it | always the session (`ControlMessage::Input`), one set of rules (input gate, allowed windows, focus); a carrier never carries a windowcast client's input |
| Gamepads | session (`GamepadSink`) | unchanged; a GameStream carrier's controller channel is not used by windowcast clients |
| Audio | session audio track per window | unchanged; carriers carry pictures only (GameStream's own audio only for Moonlight) |
| Clipboard | session | unchanged |
| Cursor | not sent: whatever the capture draws | session `Cursor { window, shape, position, visible }`; the host captures without the cursor and the client draws it, so it looks the same on every carrier and moves at input latency. Built (protocol 13): host-core follows the pointer about 60 times a second while a window streams (`WindowSource::cursor`) and sends `Cursor { window, x, y, visible, shape }` in fractions of the topmost streamed window it is over (hidden where it was once it leaves them), each shape once before it is named (`CursorShape`, RGBA and hotspot). Windows hosts give it (`GetCursorInfo`, and Windows.Graphics.Capture with its cursor off, Windows 10 2004 on); Linux hosts do not yet, and their captures draw it. client-core keeps it (`Event::Cursor`, `cursor`, `cursor_image`; `windowcast_session_cursor_image` in C). The Windows client shows the host's shape as its own pointer over the stream window while it sends input (the pointer then moves with the mouse, at no latency); the viewer draws it over the watched window where the host says, scaled as the picture is. Third-party clients of compatibility mode see no pointer in the pictures |
| Picture | the backend | the carrier: the only per-carrier part |

A carrier is then any way of getting a window's pictures to the client:
Native (video track on the session), Passthrough, Desktop cut-out,
GameStream video, RDP pictures, VNC. Each is interchangeable per window.
Bitrate, frame rate and resolution adapt continuously within a carrier
(host-core `quality`, as today).

On the host, one capture per window should feed every carrier of that
window (a fan-out), so two carriers during a switch do not capture twice.
Not built yet: during the few seconds two carriers overlap, the window
is captured twice (Windows.Graphics.Capture and the Wayland capture both
allow it).

### Switching a carrier: make-before-break (protocol 10)

A window's stream gets a generation number; two generations of one
window may run at once.

1. `StreamStartRequest { target, options, generation: n+1 }` while
   generation n runs (`options` as for any start: the carrier, codecs,
   limits). Carriers running at once differ in backend; the host refuses
   a second carrier of the same backend.
2. The host starts generation n+1 and answers `StreamStartResponse` with
   that generation (a handoff for carriers with their own connection,
   such as RDP), or refuses it.
3. The client brings the new carrier up beside the old one, decodes it
   off screen, and when it has a picture at the window's current size
   swaps the view to it in one frame. Then it sends
   `CarrierStop { window, generation: n }`.
4. If generation n+1 shows no picture within 5 s, the client stops it
   and keeps n (the switch failed; see backoff below).

`StreamStopRequest` stops every carrier. The host sends `StreamStopped`
once, when the window's stream ends (stopped, its last carrier stopped,
or the window closed); the transport sends `TrackEnded` when a window's
video track on the session ends, which alone ends no stream. Input,
audio, clipboard, cursor and the window's own events never move: they
are the session's. The window's sound starts with its first carrier,
whichever it is, and stops with its stream.

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

Built (step 3, `client-core` `selector`): the client chooses between
video and RDP pictures for each shown window, twice a second, from:

- motion, measured on the client as the pictures or frames a window's
  carrier delivers per second against 24 (a still window's carrier sends
  about one a second), smoothed over about 2 s;
- the window's content as the host classifies it (`ContentHint`);
- whether RDP can reach the host (on this network, a client that shows
  pictures).

Scores: video 0.2 + motion; RDP pictures 1.2 - motion for text, 0.3 -
motion for other windows (so only text goes to RDP unless almost still),
nothing for games and video players. Margin 25%, dwell 3 s, at most one
switch per 15 s per window (counted from the stream's start), and after a
refused or failed switch that carrier waits 30 s, doubling each time up
to 5 min. A window whose app has a rule of the user's, or that the user
switched by hand, is pinned. On by default
(`ClientSession::set_auto_switch`; the Windows app's "Switch between video
and RDP by themselves"). Not used yet: the host's own motion measure,
loss and bandwidth, decoder and encoder load, and battery.

### What the client shows

The same window object throughout: the app's window or view, its title,
size, position, popups and input stay put. The client keeps two picture
paths per window (video decoding and RGBA pictures), each able to draw
the same view: the Windows client presents either into the same swap
chain; the Android viewer keeps two stacked surfaces and swaps which one
is visible on the new carrier's first picture. The window's size is the
session's (`WindowGeometry`), and each carrier's picture is scaled to it,
so a carrier sending a smaller picture does not resize the window. A
switch is not something the user sees.

Built (step 2): `ClientSession::switch_window` starts the new carrier and
gives it up after 5 s without a picture; `Event::CarrierStarted` hands it
to the app, which calls `carrier_shown` once it has shown its first
picture, and the old carrier stops; `Event::CarrierRefused` says a switch
did not happen. The Windows client and the Android viewer do this, by
hand for now (the app's "Switch to", the viewer's menu); the automatic
selector is the next step.

### Third-party clients and RemoteApp

- Plain RDP, GameStream (Moonlight) and VNC clients are not windowcast
  sessions: they have no session window model and cannot switch.
  Approved by the owner with: "Yep, go for it. We can have a
  compatibility mode for each stream, so we can still cast to other
  clienrs". So every stream (one window) can be put into **compatibility
  mode**, which also exposes that window to third-party clients: an RDP
  desktop of the window (mstsc, FreeRDP), a GameStream app (Moonlight)
  and a VNC endpoint. It is off by default and switched per stream, from
  the host or from the client streaming the window; the endpoints run
  only while it is on, with the host's rules and authentication (a login
  made for the stream, or the host's PIN pairing for Moonlight). A
  third-party client brings its own input and audio, under the same input
  rules. Seamless windows for those clients (RDP rail) are not pursued:
  seamless windows are a property of windowcast sessions, the same on
  every carrier. This replaces always-on compatibility endpoints.

  Built (protocol 16; `windowcast_host::compat`). A session turns it on
  for a window it streams (`CompatibilityRequest`; refused for a window
  it does not stream, or an account that may not drive windows), and it
  ends when that session stops streaming the window; the host's owner
  turns it on for any window in the host application and off again. The
  answer (`Compatibility`) says how a person points each third-party
  client at the window:
  - **RDP**: `WithRdp` serves the window as the whole desktop of an RDP
    server of its own, on a port of its own, TLS and NLA with a login made
    for it (user `windowcast`, a random password), taking the client's
    keys and pointer.
  - **GameStream**: the host's GameStream face (`windowcast_gamestream`,
    on the standard ports 47984, 47989 and 48010) starts the first time a
    window needs it and stays up; its app list is the windows in
    compatibility mode. Moonlight pairs by the PIN it shows, typed in the
    host application.
  - **VNC**: `windowcast_vnc` serves the window as an RFB 3.8 framebuffer
    (RFC 6143) on the first free port from 5900, with VNC Authentication
    by a password made for it (eight characters, what the protocol
    carries), Raw updates of what changed, DesktopSize when the window
    changes size, any true-colour pixel format, and the viewer's keys
    (characters as text, shortcuts and other keys as keys) and pointer.

  The client app shows "Compatibility mode" per stream with these
  details, the viewer a menu item and a dialog. Every endpoint's input
  goes through the host's "Clients may drive streamed windows" setting.
  Not built: sound for RDP and VNC clients (Moonlight gets the window's).
- Windows RemoteApp launches (opt-in, above) move to the host: the host
  logs in to its own Remote Desktop as the RemoteApp client and serves
  those windows as host windows, so they too are carried by any carrier
  and switch like any other, instead of reaching the client over RDP only.
  Built (protocol 15): `RemoteApps::launch` runs it on the host, and
  `WithRemoteApps` lists its windows and carries them.

## A locked host (Droidtop/tracker#467)

Decided by the owner, 2026-10-10: "A locked host should still allow
streaming. Otherwise, it's useless for remote access", and "Windowcast
doesn't have to follow a screen. We stream windows themselves". A host
never refuses a stream because it is locked (0.30.0 did; that is gone).
The work comes in pieces: windows keep streaming while locked (below,
built); a private session (a session of its own on Windows Pro and up,
a headless sway session on Linux; below, built), offered only where it
exists; unlocking through the secure screen ("We can detect windows pro, and
offer to use a separate session if it exists").

### What capture and input do while locked

Measured on CI (`agent-windows/tests/lock.rs`, `agent-linux/tests/lock.rs`,
which now assert it):

- **Windows** (LockWorkStation). The input desktop is out of reach about
  0.1 s after the call. A window's own capture (Windows.Graphics.Capture)
  keeps delivering changing pictures, and one started while locked does
  too; the program keeps painting. A capture of the whole screen started
  while locked gets one picture and then nothing. SendInput takes no
  events; messages posted to a window arrive.
- **Linux** (headless sway, swaylock, ext-session-lock-v1). The toplevel
  capture keeps showing the window, one started while locked too; the
  output shows the lock screen; input through the compositor goes to the
  lock surface. GNOME differs: its PipeWire window capture freezes on the
  last picture while locked (gnome-shell issue 6771).

### Windows keep streaming while locked (built)

- **The host says so.** `ControlMessage::HostState(HostState { locked })`
  (protocol 12) is sent when the state changes, and when a session
  starts on a locked host; an unlocked host says nothing. The Windows
  agent asks Remote Desktop Services (`WTSSessionInfoEx`) every 400 ms.
  client-core turns it into `Event::HostState { locked }`; the app and
  the viewer show "This PC is locked; its windows keep streaming", with
  the input limits below.
- **Capture.** Window streams keep their window capture. The desktop
  backend (the window cut out of the whole screen) would show the lock
  screen, so while the input desktop is out of reach it serves the
  window's own capture, and the screen's again once it is back (checked
  once a second). A window Windows.Graphics.Capture will not take on its
  own (menus, tooltips) is captured from the screen, so it shows the lock
  screen while locked.
- **Input on Windows** goes to the window as posted messages while the
  input desktop is out of reach (`agent-windows/src/posted.rs`): pointer
  moves, buttons and wheel to the child window under the point, in its
  client coordinates; keys as WM_KEYDOWN/WM_KEYUP with their real scan
  codes, to the window with the keyboard focus in that program's thread
  (or the child last clicked); text as WM_CHAR. The same rules decide
  which windows take input. This is best effort, and its limits are
  known: programs that read raw input, DirectInput or the key state
  (GetKeyState, GetAsyncKeyState) do not see it, most games and some
  Chromium and UWP controls among them; clicks on a window's frame do
  nothing; Shift does not change the character a posted key makes (text
  sent as text is exact). Gamepads are virtual devices (ViGEmBus), not on
  the desktop path. The full answer for those programs is the private
  session, or unlocking through the secure screen.
- **Linux while locked.** Window capture goes on under sway, so streams
  go on. The host cannot tell that sway is locked (ext-session-lock has
  no way to ask), so it sends no `HostState`, a desktop cut shows the
  lock screen, and input goes to the lock screen. The full answer there
  is the headless session.

### A private session

The owner, 2026-10-10: "We can detect windows pro, and offer to use a
separate session if it exists." A private session keeps a remote user's
work off the host's own screen and out of the reach of the lock screen:
the programs run where nobody at the PC sees them, input reaches them in
full (SendInput on Windows, the compositor's own input on Linux), and
locking or unlocking the PC does not touch them. It is a host option,
off by default ("Offer clients a private session", with what it does in
plain words); where the host can offer it and its owner turned it on, a
client is offered it and starts and ends it (the app's "Use a private
session", the viewer's menu). Built: protocol 14, host-core, both agents,
client-core (`ClientSession::private_session`, C
`windowcast_session_private_session`), the app and the viewer.

**Windows: the session moves to a Remote Desktop connection of the
host's own.** Windows 10 and 11 run one interactive session: a Remote
Desktop login of the signed-in user takes that session over, and the
PC's screen goes to the sign-in screen while the session goes on, now
drawn for the connection instead of the screen. That is the private
session. The host logs in to its own Remote Desktop (loopback, TLS and
NLA, the same RDP client as RemoteApp launches) as the user it runs as,
and keeps the connection open, discarding its pictures, at the size of
the client's screen. The host and its windows stay where they are, in the
same session, which is no longer the console: it is unlocked, so capture
and SendInput work as on an unlocked PC, and nobody at the PC sees it.

- **Offered only where it exists.** The host reads the edition
  (GetProductInfo): Home editions have no Remote Desktop server, so it is
  not offered there ("This PC runs Windows Home, which has no Remote
  Desktop to move the session to"). On Pro, Enterprise, Education, Pro for
  Workstations and Server it is offered when Remote Desktop is on; when it
  is off the host's setting says so and how to turn it on.
- **Logging in** follows RemoteApp's rule ("Logging in: one host
  setting" above): a client signed in with the host's Windows account
  hands the host that password for this one login, held in memory only;
  any other client is asked for the Windows password when it starts the
  private session. The login is the user the host runs as, never another
  account: a second user would sign this one out on a client edition.
- **Ending it.** The client ends it, or it ends when the client's last
  session closes. The host closes its connection; the session is then
  disconnected and stays locked until someone signs in at the PC, which
  brings it back to the screen as it was. The host never unlocks the PC's
  own screen.
- **Plain warning** in the host's setting: "While a client uses a private
  session, this PC's own screen shows the sign-in screen and your programs
  keep running out of sight. To use the PC here again, sign in as usual."
- **Limits to check on hardware**: a session drawn for Remote Desktop
  uses the Remote Desktop display driver, so a capture or encoder made
  on the GPU before the move may have to be made again (the stream
  restarts its capture when it fails); games that need the GPU may not
  run as they did. Gamepads are ViGEmBus devices and keep working.

**Linux: a headless sway of the user's own.** The host starts a second
sway with no screen (`WLR_BACKENDS=headless`, as CI runs it) for the
user, with its own Wayland and sway sockets, and the agent serves that
compositor's windows instead of the seat's for the clients of the
private session; programs a client launches start there. Nothing shows
on the PC's screen, and the seat's lock screen does not reach it. Offered
when sway is installed; the headless sway stays up while the host runs,
so ending the private session (the agent goes back to the seat's
windows) leaves its programs running for the next one. Programs started
there get its `WAYLAND_DISPLAY` and `SWAYSOCK`; an X11-only program still
finds the seat's `DISPLAY` and shows on the seat's screen.

**Protocol.** `HostState` gains `private_session`: unavailable (with
the plain reason), available, or active. A client asks with
`PrivateSessionRequest { start, password }` and is answered with the new
`HostState` or a `PrivateSessionRefused { reason, password_needed }`.

### The secure screen (built; hardware check pending)

The owner chose a smaller privilege boundary than a SYSTEM host: an
opt-in `windowcast-secure-helper.exe` performs only Desktop Duplication,
text/mouse `SendInput`, and an input-desktop state query. It cannot run a
command, open a caller-selected file, load a plug-in, serve a terminal or
handle gamepads. The ordinary host and all application code remain in the
user's process.

- The reusable client API installs, enables, disables and uninstalls the
  helper. The reference app exposes it as `windowcast-app secure-screen
  install|enable|disable|uninstall|status`; downstream applications choose
  how to present the same API. Install copies the helper only to
  `%ProgramFiles%\windowcast\secure-helper`. Enable explains service
  mode, asks for confirmation, elevates through UAC, records the granting
  account's SID in `HKLM\SOFTWARE\windowcast\SecureScreen`, and registers
  a fixed Task Scheduler definition for SYSTEM at boot, console connect
  and unlock through the Task Scheduler COM API, without a temporary XML
  file. The account SID is captured and displayed before elevation, then
  passed to the elevated command. `disable` clears the grant before task
  removal; `uninstall` also removes the binary. None of these operations is
  run by a normal host. Protected install DACLs let users read and execute
  clients while only administrators and SYSTEM can replace them.
- Task Scheduler begins SYSTEM tasks in Session 0. The fixed launcher
  duplicates only its own SYSTEM token, changes that token's session ID to
  `WTSGetActiveConsoleSessionId`, and starts only the installed helper on
  `winsta0\default`. The broker refuses to run as any account but
  LocalSystem or outside the active console session, and serves only when
  that console belongs to the granting SID or has no signed-in user.
- Before every operation the broker re-reads the grant. Its named pipe
  `\\.\pipe\windowcast-secure` has a protected DACL for SYSTEM and the
  granting SID. The peer PID comes from `GetNamedPipeClientProcessId`; its
  primary token SID and session must match the grant and active console,
  and its image must be exactly an
  installed `windowcast-agent-windows.exe` or `windowcast-app.exe` in the
  admin-only `%ProgramFiles%\windowcast` folder. The client independently
  checks `GetNamedPipeServerProcessId` and accepts only the LocalSystem
  helper at its exact installed path, using identification-only SQOS.
- The broker calls `OpenInputDesktop` without cross-account hook or desktop
  switching rights, then `SetThreadDesktop` before each request. Capture
  reuses DXGI Desktop Duplication, requires B8G8R8A8, and returns bounded
  packed BGRA. The helper is per-monitor-v2 DPI aware and maps pointer
  coordinates through the captured output's desktop rectangle. Persistent
  client connections serialize concurrent requests and idle clients time
  out. The desktop name from `GetUserObjectInformation(UOI_NAME)`
  distinguishes Winlogon from Default. Capture and input are refused on
  every desktop except Winlogon.
- When enabled and Winlogon is the active input desktop, the Windows host
  lists one reserved synthetic window:
  `secure-desktop` (`WindowId(u64::MAX)` on the numeric wire). It encodes
  helper frames through the existing encoder path and routes that entry's
  text, mouse input, and Enter, Backspace, Tab, Escape and arrow keys to
  the helper. Other keys and all modifiers are dropped. The normal desktop
  remains available only through the ordinary host capture path.

Unit tests cover fail-closed grant parsing, framed-message limits and the
operation allowlist, and client/server SID, session and image verification
with fakes. The
remaining check needs a Windows rig: CI is not a SYSTEM task in an
interactive session and cannot show Winlogon or a UAC secure desktop.

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

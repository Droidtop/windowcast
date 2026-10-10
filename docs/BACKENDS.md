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

Decided 2026-10-10 (Droidtop/tracker#111). Two RDP paths serve single
windows, and whole-desktop RDP stays what a client gets when it asks for
the desktop (`StreamTarget::Desktop`):

- **(b) Launched programs as RemoteApps.** When a client launches a
  program on a Windows host (the command stream's `Launch`) and its rules
  give that program RDP, the program runs as a RemoteApp of Windows' own
  Remote Desktop: the client logs in to the host's Remote Desktop itself,
  asks for the program (the `rail` channel), and each window Windows
  describes in its window orders becomes a window in the client's
  windowcast window list, drawn from the RemoteApp desktop's picture, with
  input mapped to it. The program runs in a Remote Desktop session of its
  own, not on the host's screen. Built first.
- **(a) Existing windows over windowcast's own RDP server.** A window
  already open on the host, given RDP by the rules, is served by
  windowcast's own RDP server (`WithRdp`, "Handing a window to RDP"
  above): its captured picture is the RDP desktop. Windows' RemoteApp
  cannot do this: it always starts a new copy of a program in a new
  session. That server will also speak `rail`, so RemoteApp clients such
  as mstsc and FreeRDP show the window seamlessly. Built after (b).

### How a launch becomes a RemoteApp

1. The client evaluates its rules on the program as a window would be
   (`app_id` from the program's file name, content from
   `selection::classify`). If they give `Rdp`, the client shows pictures,
   and the host is on the same network, the `Launch` asks for a RemoteApp.
2. The host serves it as a RemoteApp when its RemoteApp setting allows it
   (below) and Remote Desktop is on; otherwise it starts the program in
   its own session as a plain launch, and the program's windows stream as
   usual, (a) included.
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

- Windows 10 and 11 (client editions) allow one session: a Remote Desktop
  login takes over the console and locks the local screen. RemoteApp
  launches are off there by default; turning them on in the host's
  settings shows a plain warning saying exactly that.
- Windows Server: on by default when a session is free: the Remote
  Desktop Session Host role is installed, or fewer than the two sessions
  a server without it allows are in use. Otherwise the launch runs in the
  host's own session.
- Remote Desktop must be on, and Windows must let the program run as a
  RemoteApp (an allow-list entry, or the allow list switched off); the
  host's settings say what is missing.

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

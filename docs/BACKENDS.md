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
| `Rdp` | text-heavy windows: editors, terminals, documents | its own connection; sharp text at low bandwidth | host and client built in the `rdp` crate on IronRDP; not yet chosen by a session (#111) |
| `Vnc` | anything else that only speaks VNC | its own connection | seam only (#111) |

Video tracks carry H.264, H.265 or AV1, chosen per track: the client lists
what it decodes in hardware, most preferred first, and the host picks the
first it can produce (for passthrough, the codec the media already is).

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

# windowcast

A library for streaming application **windows** — not necessarily a
whole desktop — from a host to a client, each window over the protocol
that suits it: a text editor over RDP, a game over GameStream, a video
player by passing its already-encoded video through, everything else over
windowcast's own per-window WebRTC tracks. Several streams can be live at
once, each on its own backend. It has PAKE-bootstrapped device pairing,
directory-issued account credentials, and host agents per OS. GPL-3.0.

**The shape.** windowcast is one library we write: protocol, pairing and
identity, every backend, codecs and input. Host agents and clients are
thin implementations of it on each end, and clients use it only through
`client-core`'s C interface. The GameStream, RDP and media backends are
our own implementations of those protocols inside the library, not
wrappers around other projects' clients. See
[`docs/BACKENDS.md`](docs/BACKENDS.md) for the backends and how a window's
backend is chosen.

I built this as the reusable core behind
[droidtop](https://github.com/Droidtop/droidtop)'s remote-window
streaming feature, but kept it droidtop-agnostic on purpose — the goal is
a library other projects (VR streaming, general remote desktop) can embed
too, not a droidtop-only feature that happens to live in its own repo.

NoMachine's real NX protocol is closed-source, and the old open NX/X2Go
lineage only knows how to do this trick for X11. windowcast doesn't try to
be protocol-compatible with either — it's a new, from-scratch design built
on WebRTC, for the reasons under **Design** below.

## Status

Early — see the crate-by-crate breakdown. The security-critical pieces
(identity, pairing, protocol codec) are real and tested. Two peers now
connect for real: a client pairs with a host by PIN (or resumes with
pinned identities), the offer and answer are authenticated end to end
over an untrusted signaling stream, and the host can attach and detach
per-window video tracks in H.264, H.265 or AV1 that the client receives
as whole frames. Two peers on the same device connect over loopback even
with no network. Loopback tests run all of it between two real WebRTC
stacks. What's **not** done yet: any real capture or encoder feeding
those tracks (the Linux agent lists windows but cannot capture them — see
`agent-linux/src/capture.rs`; there is no Windows or macOS agent),
client-side decode, input, audio, and every backend except the native one
(their seam is in place: see docs/BACKENDS.md).

| Crate | Status |
|---|---|
| `protocol` | Real, tested (message schema + codec + version check; backend selection rules in `selection`) |
| `identity` | Real, tested (persistent Ed25519 identity, pinned-peer trust store) |
| `pairing` | Real, tested (SPAKE2 PAKE + HKDF + HMAC fingerprint authentication) — the *device* credential |
| `directory` | Real, tested (accounts, Argon2 password hashing, PASETO v4.public session certificates) — the *account* credential |
| `apollo-client` | Real, tested `serverinfo` client + `applist` XML parser for a local Sunshine/Apollo host; the authenticated fetch needs the GameStream backend's pairing (not started) |
| `transport` | Real, tested (webrtc-rs 0.21): authenticated offer/answer signaling over any byte stream (`signaling::connect`/`accept`), the control data channel, per-window H.264/H.265/AV1 tracks with renegotiation over the control channel, keyframe requests, loopback candidates for same-device sessions, TURN relay wiring (`Session::with_relay`) |
| `client-core` | FFI skeleton (session create/free); does not expose signaling or frames yet |
| `agent-linux` | Serves clients over TCP (PIN pairing, then pinned resume) and answers window lists from a real compositor (`zwlr_foreign_toplevel_manager_v1`); capture is an explicit `NotImplemented` (needs `ext-image-copy-capture-v1`, not vendored yet) |
| `agent-windows` | Not started |
| `agent-macos` | Not started |
| GameStream, RDP, VNC, passthrough, whole-desktop backends | Not started; the seam is in `protocol` (`StreamBackend`, `selection`). Each is our own implementation of its protocol; GameStream pairing follows the real protocol's salted-PIN AES challenge/response, read from reference sources, not guessed at |
| `cli-tools` | Reference client CLI: pairs with or resumes to a host agent and lists its windows |

## Design

See [`docs/SECURITY.md`](docs/SECURITY.md) for the full authentication
model. Short version: WebRTC gives fast, hardware-accelerated AEAD media
encryption (AES-128-GCM via DTLS-SRTP) for free, and handles NAT traversal
and congestion control — but its DTLS handshake is only as trustworthy as
whatever channel carries the SDP fingerprint exchange. windowcast closes
that gap two ways, for two distinct credential types:

- **Device credential** (`pairing` + `identity`) — a SPAKE2 PAKE seeded by
  a PIN shown on the host authenticates the fingerprint exchange itself,
  then a persistent pinned Ed25519 identity takes over for every later
  reconnect. One specific device is the identity (Moonlight-style) — no
  accounts involved.
- **Account credential** (`directory`) — a person logs into a directory
  server (password today, OIDC later); the directory mints a short-lived
  certificate, signed by its own CA key, binding that login to the
  session's ephemeral key. A host that trusts the directory's CA key
  accepts any account it vouches for, without individually pinning every
  user (RDP/RemoteApp-style) — the account, not the device, is the
  identity, so the same person can connect from anywhere.

Both feed the same fingerprint-authentication mechanism in `transport` —
they differ in trust root, not in mechanism.

One `PeerConnection` (one DTLS handshake) is shared per client<->host
*session*; each open window is a separate track/data-channel within it, so
opening or closing a window never repeats the expensive asymmetric
handshake.

Each session binds UDP on every interface and on 127.0.0.1, so a client
and host on the same device (droidtop and its own desktop container)
connect over loopback even with no network up.

## Building

```
cargo build --workspace
cargo test --workspace
```

`agent-linux` needs Wayland client headers (`libwayland-dev`,
`libxkbcommon-dev` on Debian/Ubuntu) to build.

## License

GPL-3.0-only — see [`LICENSE`](LICENSE).

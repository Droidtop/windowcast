# windowcast

A protocol + SDK for streaming a single application **window** — not a
whole desktop — from a host to a client, with per-window WebRTC channels,
PAKE-bootstrapped mutual authentication, and pluggable host-capture agents
per OS. GPL-3.0.

Built as the reusable core behind [droidtop](https://github.com/bi0shacker001/droidtop)'s
remote-window streaming feature, but deliberately kept droidtop-agnostic —
the goal is a library other projects (VR streaming, general remote desktop)
can embed too, not a droidtop-only feature that happens to live in its own
repo.

Real NoMachine's NX protocol is closed-source, and the old open NX/X2Go
lineage only knows how to do this trick for X11. windowcast doesn't try to
be protocol-compatible with either — it's a new, from-scratch design built
on WebRTC for the reasons in **Design** below.

## Status

Early — see the crate-by-crate breakdown. The security-critical pieces
(identity, pairing, protocol codec) are real and tested. The session
transport stands up a real WebRTC `PeerConnection` and extracts a real
local DTLS fingerprint. What's **not** done yet: SDP offer/answer signaling
between two live peers, per-window video track attach, and the Linux
agent's actual capture pipeline (window *listing* works today; window
*capture* does not — see `agent-linux/src/capture.rs`). Windows and macOS
agents don't exist yet at all.

| Crate | Status |
|---|---|
| `protocol` | Real, tested (message schema + codec + version check) |
| `identity` | Real, tested (persistent Ed25519 identity, pinned-peer trust store) |
| `pairing` | Real, tested (SPAKE2 PAKE + HKDF + HMAC fingerprint authentication) — the *device* credential |
| `directory` | Real, tested (accounts, Argon2 password hashing, CA-signed session certificates) — the *account* credential |
| `transport` | Real WebRTC session/fingerprint plumbing + TURN relay wiring (`Session::with_relay`); SDP signaling and per-window tracks not wired yet |
| `client-core` | FFI skeleton (session create/free, fingerprint extraction); frame delivery not wired yet |
| `agent-linux` | Toplevel listing works against a real compositor (`zwlr_foreign_toplevel_manager_v1`); capture is an explicit `NotImplemented` (needs `ext-image-copy-capture-v1`, not vendored yet) |
| `agent-windows` | Not started |
| `agent-macos` | Not started |
| `cli-tools` | Reference client CLI; demonstrates identity + transport end to end locally |

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

## Building

```
cargo build --workspace
cargo test --workspace
```

`agent-linux` needs Wayland client headers (`libwayland-dev`,
`libxkbcommon-dev` on Debian/Ubuntu) to build.

## License

GPL-3.0-only — see [`LICENSE`](LICENSE).

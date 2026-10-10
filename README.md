# windowcast

A library for streaming application **windows** — not necessarily a
whole desktop — from a host to a client, each window over the protocol
that suits it: a text editor over RDP, a game over GameStream, a video
player by passing its already-encoded video through, everything else over
windowcast's own per-window WebRTC tracks. Several streams can be live at
once, each on its own backend. It has PAKE-bootstrapped device pairing
and host agents per OS. GPL-3.0.

**The shape.** windowcast is one library we write: protocol, pairing and
identity, every backend, codecs and input. Host agents and clients are
thin implementations of it on each end, and clients use it only through
`client-core`'s C interface. The GameStream, RDP and media backends live
inside the library, not as wrappers around other projects' programs
(GameStream is written here; RDP links IronRDP's protocol crates). See
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
stacks. Real H.264 video streams end to end: the test-pattern host
(`windowcast-testhost`, OpenH264) to any client through `client-core`,
and the Android library decodes it with MediaCodec (not yet run on a
device). Real windows stream from Windows: the Windows agent captures a
window with Windows.Graphics.Capture and encodes it on the GPU (NVIDIA,
AMD or Intel through Media Foundation, H.264 or H.265) or in software.
Input goes back: mouse, keyboard, typed text, touch and gamepads from the
client, and the clipboard both ways. A client's gamepads (up to four)
become Xbox 360 pads on the host: through the ViGEmBus driver on Windows
(the one Parsec and others install; without it the host has no pads), and
through uinput on Linux, laid out as the kernel's xpad driver reports a
real pad (the host's user needs write access to /dev/uinput). The pads
are the session's and go when it ends. Real windows stream from Linux too: the Linux agent captures a
window under any compositor with ext-image-copy-capture (wlroots 0.19,
sway 1.11 and later), encodes it with OpenH264, and under sway delivers
the pointer and keys. A window's sound streams with it from Windows hosts:
WASAPI process loopback captures what the window's application plays (and
nothing else), sent as Opus on an audio track beside the picture, and
played by the Windows client (WASAPI) and the Android library (MediaCodec,
AudioTrack); Linux hosts send it too, recording the application's own
streams through the PulseAudio API (PipeWire's pulse server included).
A client's microphone goes back the same way (Opus on its own track) when
the host allows it: Linux hosts make a virtual microphone,
`windowcast_microphone`, that applications record from; Windows has no
virtual microphone of its own, so a Windows host plays the client's voice
into a virtual audio cable (VB-CABLE, VoiceMeeter, Virtual Audio Cable, or
one named with `--microphone-device`) and applications record from the
cable's other end. The Windows client and the Android viewer send their
microphone (Android 10 and later, which encode Opus).
Quality adapts to the network: from the client's receiver reports (packet
loss) and the host's own pings (round trip), the host holds each stream's
encoder to what gets through, steps the frame rate and then the picture
size down when the rate left is thin, and climbs back when the network
clears; the host's settings and the client's per-window limits (bitrate,
frame rate, height; per app in the reference app) are ceilings it never
goes above. Both ends show what a stream is sent at, its loss and the round
trip.
What's **not** done yet: macOS, rumble back to the client's pads,
sending gamepads from the Windows client, the clipboard and GPU encoding
on Linux, the microphone from the Linux client, and every backend
except the native and desktop ones (their seam is in place: see
docs/BACKENDS.md).

| Crate | Status |
|---|---|
| `pty` | Real, tested: a shell on a pseudo-terminal (ConPTY, openpty); the Windows half is built by CI but not run |
| `terminal` | Real, tested against an SSH server the tests run: channels, the screen model, the SSH client with pinned host keys |
| `protocol` | Real, tested (message schema + codec + version check; backend selection rules in `selection`) |
| `identity` | Real, tested (persistent Ed25519 identity, pinned-peer trust store) |
| `pairing` | Real, tested (SPAKE2 PAKE + HKDF + HMAC fingerprint authentication) — the *device* credential |
| `accounts` | The *account* credential beside it ([`docs/ACCOUNTS.md`](docs/ACCOUNTS.md)): a sign-in registers the client's device key to an account on the host. Local accounts (Argon2id), the host OS's (PAM, `LogonUserW`), LDAP and Active Directory, OpenID Connect (authorization code with PKCE in the browser, device code; SAML through an OIDC broker), Kerberos (GSS-API, SSPI); the credential sealed to the host with HPKE; per-host policy (who connects, which windows, input, commands); SSH user certificates from a sign-in. Tested in CI against a local Dex, slapd, MIT KDC and PAM |
| `gamestream` | Our own GameStream (#110): pairing at both ends (PIN-salted AES key, RSA certificates, the five-step challenge), a client for Sunshine/Apollo (`serverinfo`, pairing, app list, launch, quit over HTTPS with the host's certificate pinned), and a host stock Moonlight pairs with, lists windows from and streams them from (RTSP, H.264 video packets, the AES-GCM ENet control stream; tested in CI with Arch's moonlight-qt). Our client streams from a GameStream host; input (keys, mouse, touch, controllers) and the window's sound (Opus with Reed-Solomon parity) cross in both directions; control, audio and video are encrypted as the client asks |
| `rdp` | RDP (#111) on IronRDP's protocol crates: one window of a host served over RDP (the desktop is the window; TLS with a per-host certificate and NLA with a login the host sets; RemoteFX or plain bitmaps as the client supports; keys and pointer to the window), and a client for RDP hosts (ours, Windows' own Remote Desktop) giving the picture as RGBA and taking windowcast input. `WithRdp` makes it a session backend: a window the client's rules send to RDP gets its own RDP server and one-time login, handed over the session (docs/BACKENDS.md). `windowcast-rdp connect` and `serve` try it. Tested in CI: through a session end to end, our client against our host, stock FreeRDP against our host, our client against a Windows runner's own Remote Desktop. The Android viewer shows RDP windows. RemoteApp against Windows hosts (one program's window, `--app`) is started but not yet working. The Android viewer and the app's own client show RDP windows. Not yet: a window that changes size mid-session |
| `rendezvous` | Real, tested: finding a paired device away from the LAN the way Syncthing does (its global discovery, STUN, hole punching; addresses only, never data), shared with droidtop-agent |
| `transport` | Real, tested (webrtc-rs 0.21): authenticated offer/answer signaling over any byte stream (`signaling::connect`/`accept`), the control data channel, per-window H.264/H.265/AV1 tracks with renegotiation over the control channel, keyframe requests, loopback candidates for same-device sessions; away from the LAN, signaling over a punched UDP stream (`punched`, `remote`) and ICE through the NATs (`Session::away`), with no relay |
| `host-core` | Real, tested: the host side of the library (listening, PIN pairing with lockout and re-issue, trusted clients, window lists, backend and codec choice, feeding window tracks from an agent's encoder, keyframe requests). Agents implement `WindowSource` |
| `client-core` | Real, tested: the client surface, a Rust API and the C interface `include/windowcast.h` (connect/pair/resume, window list, streams, events as JSON, whole frames per window) |
| `android/` | The Android library (JNI over the C interface, MediaCodec decoding onto a Surface) and a viewer app; built by CI for arm64-v8a and x86_64; not yet run on a device |
| `agent-linux` | Real, tested: windows from `ext_foreign_toplevel_list_v1` (falling back to `zwlr_foreign_toplevel_manager_v1` for listing), per-window capture with `ext-image-copy-capture-v1` (toplevel source) into shared memory, the desktop backend (the window's output, cut by sway's geometry), OpenH264. Input under sway: a virtual pointer at absolute layout positions mapped onto the window (sway IPC), a virtual keyboard with an xkbcommon US keymap and its modifier state, text typed through it. CI runs it under a headless sway: captures a test window both ways, decodes the colour, and checks the click position and keys arrive |
| `agent-windows` | Real, tested: the desktop's windows, per-window capture (Windows.Graphics.Capture), BGRA to NV12/I420, encoders behind one interface chosen with `--encoder` (Media Foundation hardware, i.e. the GPU vendor's NVENC/AMF/Quick Sync MFT, or one vendor's on a machine with several, with H.265 and AV1 where offered; Microsoft's software H.264 MFT; OpenH264). CI captures a real window, encodes it with each encoder, streams it over loopback and decodes it. Input with SendInput (pointer mapped from the picture to the window, keys as scan codes, text as Unicode, touch as the pointer), clipboard text both ways; CI clicks and types into a real window through a client |
| `agent-macos` | Not started |
| RDP, VNC, passthrough backends | In progress (RDP, #111) and not started; the seam is in `protocol` (`StreamBackend`, `selection`) |
| `client-windows` | The Windows client end: hardware decoding with Media Foundation on a Direct3D 11 device (H.264; H.265 and AV1 with Microsoft's Video Extensions), presented through the GPU's video processor into a flip-model swap chain in a native window per stream; pointer and keys back to the host |
| `app` | The reference application `windowcast-app`: host, client or both by configuration, a native window per role (egui) and one per streamed window (`client-windows`); `--connect`/`--stream` for a stream straight from the command line. Host role on Windows and Linux (Wayland); stream windows on Windows only so far |
| `cli-tools` | `windowcast-client` (pairs or resumes, lists windows, `--watch` streams one and decodes it) and `windowcast-testhost` (a host whose one window is an OpenH264 test pattern, Windows included) |

## Design

See [`docs/SECURITY.md`](docs/SECURITY.md) for the full authentication
model. Short version: WebRTC gives fast, hardware-accelerated AEAD media
encryption (AES-128-GCM via DTLS-SRTP) for free, and handles NAT traversal
and congestion control — but its DTLS handshake is only as trustworthy as
whatever channel carries the SDP fingerprint exchange. windowcast closes
that gap with its device credential (`pairing` + `identity`): a SPAKE2 PAKE
seeded by a PIN shown on the host authenticates the fingerprint exchange
itself, then a persistent pinned Ed25519 identity takes over for every
later reconnect. One specific device is the identity (Moonlight-style).

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

The Android library and viewer: build client-core with
`cargo ndk -t arm64-v8a -t x86_64 -P 26 -o android/windowcast/src/main/jniLibs build --release -p windowcast-client-core`,
then `./gradlew :windowcast:assembleRelease :viewer:assembleDebug` in
`android/` (CI does both, `.github/workflows/android.yml`).

Debug builds of the viewer are signed with `android/viewer/debug.keystore`,
committed on purpose: it only identifies debug builds, so each new debug
APK installs over the last one (its passwords are Android's usual
`android`, alias `androiddebugkey`). It is not a release key and grants
nothing; release builds keep their own signing. Debug installs from 0.23.0
or earlier were signed with each build machine's own key and need one
uninstall first.

Trying it: run `windowcast-app --role host` on one machine and
`windowcast-app --role client` on another (or both roles on one), enter
the host's PIN in the client window and pick a window to stream; or
`windowcast-app --role client --connect HOST:47100 --pin PIN --stream APP`. `windowcast-testhost` (a test
pattern, any OS) with `windowcast-client HOST:47100 --pin PIN --watch 1`
or the Android viewer works too.

`agent-linux` needs Wayland client headers (`libwayland-dev`,
`libxkbcommon-dev`, `libpulse-dev` on Debian/Ubuntu) to build, and runs inside the
Wayland session it streams (`WAYLAND_DISPLAY`; `SWAYSOCK` for input and
the desktop backend).

## License

GPL-3.0-only — see [`LICENSE`](LICENSE).

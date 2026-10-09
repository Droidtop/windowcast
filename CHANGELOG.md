# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.6.0] - 2026-10-09

### Added
- Input from the client: mouse, keyboard, typed text, touch and gamepads. Windows hosts click, type and scroll in the streamed window (a touch moves and clicks the pointer); gamepads reach the host but are not delivered on Windows yet.
- The clipboard's text is shared both ways while a session is open.
- The Android viewer sends touches, keyboard keys and gamepads, and shares the clipboard.
- The client is told a window's picture size before its first frame and whenever it changes.

### Security
- Input reaches only the windows the session is streaming; a client cannot click or type into any other window on the host.

### Changed
- The protocol version is now 4; older peers are refused.

## [0.5.0] - 2026-10-09

### Added
- A Windows host agent (`windowcast-agent-windows`). It lists the windows you would see in Alt+Tab, captures any one of them on its own (even when other windows cover it), and encodes it on the graphics card with NVIDIA, AMD or Intel's encoder, in H.265 when the card offers it, or in software. Choose the encoder with `--encoder` and see what the machine has with `--list-encoders`.

## [0.4.0] - 2026-10-09

### Added
- Real video streams end to end. A test-pattern host (`windowcast-testhost`, also for Windows) streams H.264 to any client, and the reference client can watch a window and checks every frame decodes.
- The client side is one surface for every client: a C interface (`client-core/include/windowcast.h`) to connect, pair or reconnect, list windows, start and stop streams, and pull events and whole frames ready for a hardware decoder.
- An Android library over that interface that decodes windows with the device's hardware decoders (AV1, H.265 or H.264, whichever it has), and a small viewer app to try it. Built for both arm64 and x86_64 Android.
- The host side is a library too: host agents now only supply their windows and encoders; pairing, sessions and streams are shared.

## [0.3.0] - 2026-10-09

### Added
- Each window can use the protocol that suits it. Windows carry a content hint (text, game, video, general), clients choose a backend from the user's own per-app rules and then a default table (text over RDP, games over GameStream, video by passthrough, everything else native), and hosts fall back to the native backend for anything they cannot serve yet. Only the native backend exists so far; the others are named and documented in docs/BACKENDS.md.
- Window video in H.265 and AV1 as well as H.264, chosen per window from what the client can decode.
- A client and host on the same device now connect even with no network up, over loopback.
- A host whose PIN was withdrawn after three wrong guesses shows a new one a minute later instead of needing a restart.

### Changed
- The protocol version is now 3; older peers are refused.
- Closing a session now tells the other side at once instead of leaving it to notice half a minute later.
- Moved to the current webrtc-rs release line (0.21).

## [0.2.0] - 2026-10-08

### Added
- Two devices now actually connect: the first time by the PIN the host shows, afterwards with the identities they pinned then. Each side's connection details are signed end to end, so whatever carries them (a LAN socket today) cannot swap them.
- Per-window video: the host can start and stop sending any number of windows as separate video streams on one connection, and the client receives each as whole frames ready for a hardware decoder, asking the host for a fresh keyframe after a loss.
- The Linux host agent now accepts clients over the network and answers their window-list requests, and the reference client pairs with it, reconnects to it and lists its windows.

### Changed
- The protocol version is now 2; version 1 peers are refused.

### Security
- A host stops accepting a PIN after three failed pairing attempts.

## [0.1.0] - 2026-09-28

### Added
- Core streaming stack: a versioned message protocol, persistent device identities with a pinned-peer trust store, and PIN pairing that authenticates the WebRTC fingerprint exchange, so an untrusted signaling channel cannot silently substitute keys.
- Multiple concurrent streams per session: each window or game stream is started and stopped on its own and carries its own backend, so the built-in WebRTC capture can run alongside a stream handed off to another protocol library.
- Account credentials through a directory server: password-based accounts and short-lived signed session certificates, so one host can serve many users without pairing every device individually.
- TURN relay support for sessions that need a relay to connect.
- A client for a local Sunshine/Apollo host that reads server information and parses the application list.
- A Linux host agent that lists the compositor's open windows, and a reference command-line client.

### Changed
- Session certificates now use the standardized PASETO v4.public token format instead of a custom envelope, with expiry enforced by the token format itself.
- CI cancels a run made obsolete by a newer push instead of letting both finish in parallel and report the same failure repeatedly.
- Rewrote the README introduction in a plainer, first-person voice.

### Fixed
- CI no longer leaves a write-capable repository token inside the checkout where build steps can read it, and pins the third-party steps it runs to exact revisions instead of moving tags.

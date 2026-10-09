# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- A client's microphone reaches the host, as Opus on its own track of the session, when the host allows it (off by default; "Clients may use their microphone here" in the reference app). Linux hosts make a virtual microphone through the PulseAudio API (a null sink and a source remapped from its monitor, `windowcast_microphone`), removed again when the client stops. Windows hosts play the voice into a virtual audio cable that applications record from (VB-CABLE, VoiceMeeter, Virtual Audio Cable or the Steam streaming microphone are picked up; `--microphone-device` names another).
- The Windows client sends the default recording device ("Send my microphone to the host" in the reference app), and the Android library (`Microphone`) and viewer send the device's microphone on Android 10 and later. client-core gains `start_microphone`, `send_microphone` and `stop_microphone` (and the same in the C interface).

## [0.11.0] - 2026-10-09

### Added
- Linux hosts send a window's sound too: through the PulseAudio API (PulseAudio, or PipeWire's pulse server) the window's application's own playback streams are recorded, and nothing else (found by its process, and the processes it started, from sway's tree). An application that starts playing later is picked up.
- `windowcast-testhost --tone`: the test pattern with a 440 Hz tone, for trying a client's sound; `windowcast-client` reports a window's sound (packets, level, pitch).
- The Android viewer shows how many sound packets it has played, and logs it every second.

## [0.10.0] - 2026-10-09

### Added
- A window's sound streams with its picture. Windows hosts capture what the window's application plays, and only that (WASAPI process loopback, Windows 10 2004 and later; a browser's sound from its child processes too), and send it as Opus on an audio track of the session. The Windows client plays it (with a mute switch in the reference app), and the Android library and viewer play it with MediaCodec and AudioTrack. Clients get each window's Opus packets from client-core (`next_audio`, and `windowcast_session_next_audio` in the C interface).

## [0.9.0] - 2026-10-09

### Added
- Linux hosts stream real windows: the Linux agent captures a window under any Wayland compositor with ext-image-copy-capture (wlroots 0.19 and sway 1.11 and later), encodes it with OpenH264, and serves the desktop backend under sway. Window ids stay the same across connections.
- Input on Linux under sway: pointer clicks land where they were made on the picture, and keys, typed text and Shift reach the window through a virtual pointer and keyboard.
- The reference app runs as a host on Linux in a Wayland session.

### Changed
- The software encoder (OpenH264) and the BGRA conversions are part of the library (host-core), shared by the Windows and Linux agents.

## [0.8.0] - 2026-10-09

### Changed
- Windows hosts convert each captured picture to the encoder's colour format on the graphics card and read back only that, instead of converting on the processor: about 8 ms less work per 1440p frame (NVENC H.265 at 60 fps: 30 to 37 frames a second on the owner's PC).
- When the encoder is on the same graphics card as the capture, pictures never leave the card: the encoder takes the converted texture as it is (NVENC H.265 at 60 fps: 37 to about 50 frames a second, 14 to 8 ms a frame). An encoder on another card (Quick Sync on the Intel side) still gets them through memory.

### Added
- `windowcast-client --codec h264|h265|av1` asks for a codec other than H.264 (only H.264 frames are decoded to check them).

### Fixed
- NVIDIA's encoders ignored the keyframe interval and sent one every second; they now keep the two-second interval the others keep, and send bitrate as set.
- `windowcast-app --no-window` exits when the host cannot start instead of running with nothing to do.

## [0.7.0] - 2026-10-09

### Added
- The reference application, `windowcast-app`: the minimal, complete demonstration of the library, a host, a client or both by its configuration, with a native window for each role. Pair by PIN, see the host's windows with what each shows and the backend the rules pick, set a backend per app, choose codec, encoder, frame rate and bitrate (changes reach running streams), switch input and clipboard sharing, watch live statistics, and keep paired hosts. Each streamed window opens in a window of its own, normal or fullscreen on a chosen display (F11 switches). `--connect HOST --stream APP` streams one window straight from the command line, and `--no-window` runs a host without its window. The host role is Windows-only for now.
- A Windows client end (`client-windows`): each streamed window is decoded on the graphics card by Windows' own decoder (Media Foundation with Direct3D 11) and shown through a flip-model swap chain, with no copy through the processor; pointer and keys go back when input is on. It decodes the codecs Windows has decoders for: H.264 always, H.265 and AV1 with Microsoft's HEVC and AV1 Video Extensions.
- The whole-desktop backend on Windows: a window cut out of a capture of its whole screen, with whatever covers it.
- Windows hosts can pick one graphics card's encoder (`--encoder nvenc`, `quicksync` or `amf`) on machines with more than one, and AV1 where the card encodes it.
- A host application can watch and steer a running host (`HostControl`: the PIN, a new PIN, the trusted clients, who is connected and every stream's counters), and clients can measure the round trip to the host and ask for a keyframe.
- Browser windows on video sites are detected as video.

### Fixed
- A window streamed a second time in one session never arrived at the client.
- Windows hosts sent pictures faster than the frame rate they were set to.

### Changed
- A host listening on a loopback address, and a client connecting to one, open no network ports at all: the session uses loopback only.

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

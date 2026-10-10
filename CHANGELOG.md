# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.20.0] - 2026-10-09

### Added
- RDP (Droidtop/tracker#111), on IronRDP's protocol crates (Devolutions, MIT or Apache-2.0; listed in the new `NOTICE`), as the owner decided: crates may carry a protocol, never an external program. The new `windowcast-rdp` crate serves one window of a host over RDP, so any RDP client shows that window and types into it (the desktop is the window; TLS with a certificate made per host, NLA with a login the host sets; RemoteFX or plain bitmaps as the client supports; keys and pointer to the window through the host's own input delivery), and is a client for RDP hosts, windowcast's or Windows' own Remote Desktop, with the picture as RGBA and windowcast's input as RDP input. `windowcast-rdp connect HOST USER` and `windowcast-rdp serve` try it. Tested in CI: our client against our host (the pattern moves, keys and pointer arrive, a wrong password is refused), stock FreeRDP (`wlfreerdp3`) against our host, and our client logging in to a Windows runner's own Remote Desktop.
- RemoteApp in our RDP client, not yet working end to end (`ClientConfig::remote_app`, `windowcast-rdp connect ... --app PROGRAM`): the client asks for RemoteApp and runs the `rail` channel, and is meant to show only the program's window, cut from the session's picture by the window orders the server sends. IronRDP has no option for this yet, so `rdp::remoteapp` adds INFO_RAIL and the Remote Programs and Window List capability sets to the two PDUs IronRDP writes, sends the rail PDUs with the client's own MCS ID (IronRDP's channel replies carry the server's), reads window orders fast-path and slow-path, and skips slow-path PDUs IronRDP cannot read; `docs/upstream/ironrdp-remoteapp.md` drafts the upstream change. Against a Windows Server runner the server takes the RemoteApp session and sends its rail handshake in some runs, but no window has appeared yet (it logs the session off), so the CI test is off until that is solved.
- Hosts can hand out a window's raw pictures (`WindowSource::open_pictures`), for backends that encode their own way; the Linux and Windows agents do (Windows reads them back as BGRA), sending a picture when the window changes and once a second regardless.
- RDP as a session backend: a window the client's rules send to RDP is handed to an RDP server started for that one stream (`rdp::host::WithRdp`, in the app, the agents and the test host; the app's "Offer windows over RDP" setting, on by default), with a one-time login and the host's certificate fingerprint passed over the paired session (`HandoffTarget`, protocol version 6). client-core logs in, gives the window's pictures as RGBA (`ClientSession::next_picture`, `windowcast_session_next_picture`) and sends that window's input over RDP. A client asks for RDP only once it says it shows pictures (`accept_pictures`, `windowcast_session_accept_pictures`), and never away from the LAN. The Android viewer shows RDP windows (`WindowPictures`, drawing each picture onto the surface). Tested end to end in CI (`cli-tools/tests/rdp_backend.rs`).

### Changed
- One table for PC scan codes and evdev key codes (`windowcast_protocol::keys`), used by the Windows agent, the Windows client and RDP, in place of the two copies the Windows crates had.

## [0.19.1] - 2026-10-09

### Fixed
- The stock-Moonlight check pairs again when moonlight-qt says "Incorrect PIN" for a correct one. moonlight-qt cuts its PIN key at the first zero byte (`QByteArray(hash.constData())` in `nvpairingmanager.cpp`), so about one salt in sixteen gives it a key no host can match; this failed main's CI for 0.19.0, which was tagged but not released. A person pairing hits the same moonlight-qt bug and pairs again.

## [0.19.0] - 2026-10-09

### Added
- Parity for GameStream sound: every four audio packets are followed by two Reed-Solomon parity packets (Sunshine's and Moonlight's fixed 4+2 matrix over GF(2^8)), so a client rebuilds up to two lost packets of each four; the host encodes the sound at a constant rate so each block's packets are one size. Our client rebuilds lost packets and plays the rest in order, skipping a block only once a later one can play.
- Encrypted GameStream video, both ends: when the client asks (Sunshine's `SS_ENC_VIDEO`), the host seals every video packet with AES-128-GCM under the launch's input key (Sunshine's 32-byte prefix: counter IV, frame index, tag), and our client opens it, asking for it when the host offers it and `StreamRequest::encrypt_video` is set or the host requires it. A host can ask clients for it (`GameStreamServer::request_video_encryption`, off by default). Checked in CI: stock moonlight-qt, asked by our host, negotiates encrypted control, video and audio and decodes 30 fps; our client decodes an encrypted stream.

## [0.18.0] - 2026-10-09

### Added
- Sound over GameStream, both ends: the host sends the window's sound as Opus in RTP packets (5 or 10 ms, as the client asks) to where the client's audio pings come from, AES-CBC encrypted under the launch's input key when the client wants it (Moonlight does), and our client receives, decrypts and hands on the packets. Its sound is stereo; a client set to 5.1 or 7.1 is offered one coupled stream mapped over its speakers, so it still plays. Checked in CI: our client hears the test window's 440 Hz tone (a second of it, level 0.18, 440 cycles), and stock moonlight-qt receives and decrypts it.

## [0.17.0] - 2026-10-09

### Added
- Stock Moonlight streams from a windowcast host (Droidtop/tracker#110): our own GameStream host answers `/launch`, sets the stream up over RTSP (Sunshine's attributes, ping payloads and connect data), sends the window as H.264 GameStream video packets to the address the client pings from, and keeps the AES-GCM-encrypted ENet control stream (keyframe requests, the end of the stream). The host's windows are the apps Moonlight lists. Tested in CI: Arch's moonlight-qt 6.2.0 pairs, lists and streams the test pattern window, decoding 30 fps with no frames lost.
- Our own GameStream client streams too: it sets the stream up over RTSP, pings, reassembles the video packets into frames and keeps the control stream (`windowcast-gamestream stream HOST APP_ID`). In CI it decodes 30 of 30 frames from a windowcast host.
- Input over GameStream, both ends: the host reads Moonlight's input packets (keys as Windows virtual keys, absolute and relative mouse, buttons, both scroll axes, text, touch and pen, and up to four controllers, unplugged when the client drops them) and hands them to the streamed window and the host's virtual gamepads through the same input delivery a windowcast session uses; our client writes the same packets. The host now offers touch and pen to Moonlight.

### Fixed
- The Android viewer loads its native library again: the client library has a SONAME, so the JNI library no longer records a build-machine path for it.
- The Android microphone works on devices without an Opus encoder (BlueStacks has none): client-core encodes the microphone with libopus, once, for every client. Checked on BlueStacks through the PC's microphone: the viewer asks for the permission and the host hears a non-silent level.

## [0.16.0] - 2026-10-09

### Added
- GameStream, our own implementation (Droidtop/tracker#110), first piece: pairing at both ends, a client for Sunshine and Apollo hosts (server info, pairing, app list, launch, quit), and a host face that stock Moonlight pairs with and lists apps from (`windowcast-gamestream`). Streaming over GameStream comes next.

### Removed
- `windowcast-apollo`, the Sunshine `serverinfo` and app-list reader: `windowcast-gamestream`'s client does both, with pairing.
- The account-login crate (`windowcast-directory`: password accounts and directory-issued session certificates). Nothing used it; windowcast's credential is the paired device.

## [0.15.0] - 2026-10-09

### Added
- Away from the LAN (Droidtop/tracker#113): a host can be reachable by the clients it trusts from anywhere ("Reachable away from home", off by default; UDP 47101). Both sides find each other the way Syncthing's devices do, with the code droidtop-agent uses, now shared as the `windowcast-rendezvous` crate: STUN for the address the NAT gives the rendezvous socket, Syncthing's global discovery for announcing and looking it up, and hole punching. Signaling runs over a small reliable stream on the punched socket (`transport::punched`) and the session crosses the NATs with ICE (`Session::away`). Only addresses go to discovery and STUN; nothing is ever relayed. The two sides learn each other's discovery IDs on a trusted session (`ControlMessage::Rendezvous`); a client then looks for a paired host away when it does not answer on the LAN (the reference app, `Client::connect_away`, `windowcast_connect_away`). A host takes no pairing from away.

### Removed
- The TURN relay wiring (`RelayConfig`, `Session::with_relay`): windowcast never sends a session through a relay.

## [0.14.0] - 2026-10-09

### Added
- Adaptive quality (Droidtop/tracker#114): the host holds each stream's encoder to what the network carries, cutting the bitrate on packet loss (the client's RTCP receiver reports) or a growing round trip (the host now pings the client too, and clients answer), and growing it back on a clean network until the settings rule again; when the rate is thin for the picture it lowers the frame rate to 30, then the picture to three quarters and half size, then 20 and 15 fps, and climbs back in reverse. Encoders change bitrate mid-stream where they can (NVENC, OpenH264; others are reopened), and the picture is scaled on the GPU (Windows) or by a box filter (Linux, read-back pictures).
- Per-stream ceilings from the client (`StreamOptions::limits`, `ControlMessage::StreamLimits`, `set_stream_limits` and `windowcast_session_set_stream_limits`): bitrate, frame rate and height, which the host never goes above. The reference app keeps them per app and shows each stream's quality, loss and round trip on both sides; the host reports them as `StreamQuality` (`stream_quality` events).
- `windowcast-testhost --microphone-level`: takes a client's microphone and prints its level once a second, keeping nothing.

### Changed
- Protocol version 5: stream options carry the client's limits. Hosts and clients from before do not connect to these.

## [0.13.0] - 2026-10-09

### Added
- Gamepads reach the host: each of a client's pads (up to four) becomes a virtual Xbox 360 pad, through ViGEmBus on Windows hosts and uinput on Linux hosts (laid out as the kernel's xpad driver reports one, with the same USB ids, so SDL, Steam and games map it as one). A session's pads are made at its first gamepad event and unplugged when the client removes them or the session ends. In the reference app they follow "Clients may drive streamed windows and use gamepads"; switched off, the pads are unplugged rather than left holding buttons. Hosts supply them through `WindowSource::gamepads` (a `GamepadSink` per session).

### Fixed
- A client that leaves while the host is renegotiating (for example right after stopping a stream) ends its session at once; the host used to wait out the 15 s negotiation timeout first.

## [0.12.1] - 2026-10-09

### Fixed
- Linux hosts no longer let the virtual microphone take over the speakers: where the sound server hands the default output, and the streams on it, to every new sink (module-switch-on-connect), the desktop's sound went into the microphone. The default and its streams are put back. (0.12.0 was tagged with this fault and not released.)

## [0.12.0] - 2026-10-09

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

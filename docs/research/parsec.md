# Parsec's protocol: a feasibility study

Droidtop/tracker#441. Written 2026-10-09 from Parsec's published material
and from what the official Parsec app logs on the owner's own PC. Nothing
here comes from disassembling Parsec, and nothing was sent to Parsec's
servers.

## The goal

The owner, verbatim: "To be clear: we want to implement the parsec
protocol as a peer-to-peer one. No routing, our own standard crypto".

So the goal is **not** to join Parsec's service. It is a windowcast
transport built on Parsec's *design*: a low-latency UDP transport like BUD,
with its pacing, loss handling, congestion control and frame and input
framing. It runs directly between our own peers. It uses no Parsec servers,
relays or accounts, and it uses windowcast's own identity, pairing and
standard crypto, not Parsec's keys. Working with stock Parsec is not a goal.
The last section explains why it is not possible anyway.

## Sources

Reference material:

- Parsec engineering blog, "A networking protocol built for the lowest
  latency interactive game streaming" (BUD).
- parsec.app/technology (BUD, NAT traversal, crypto, pipeline).
- Parsec blog, "Description of Parsec technology" (capture, encode,
  decode, render, V-sync and frame dropping).
- Parsec blog, "Parsec game streaming total latency at 240 frames per
  second" (end-to-end latency measured with a 1000 fps camera).
- Parsec support, "Parsec Connectivity Requirements" and "How to port
  forward Parsec" (ports, the backend, STUN, relays).
- The Parsec SDK header `parsec.h` (an archived public copy of Parsec's
  SDK; the official repository is gone). It covers the session model,
  config, input messages, frames, audio and status codes.

Observations on the owner's PC, from the logs and config written by the
installed app (Parsec release15, build 150-105c; `C:\ProgramData\Parsec\`
`log.txt`, `log_cl.txt`, `config.json`, `metrics_*.json`). The account token
file `user.bin` was not read.

## What is documented or observable

### Session setup and signaling

- **Backend.** The app keeps an HTTPS API (`kessel-api.parsec.app`) and a
  WebSocket (`kessel-ws.parsec.app`) open, both on TCP 443. The log shows
  a `/me` poll roughly every 30 to 40 minutes, a `signal_thread`, and
  WebSocket reconnects (`Websocket upgrade request returned status code
  101`). This is where peers find each other, are authorised, and swap
  connection candidates.
- **SDK model.** `ParsecClientConnect(ps, cfg, sessionID, peerID)`: the
  session ID and the host's peer ID come from Parsec's API. The header says
  the call "performs authentication, peer-to-peer negotiation, and NAT
  traversal". A host starts with only its session ID. A guest goes through
  `WAITING → CONNECTING → CONNECTED` and carries a 56-byte `attemptID`.
- **NAT traversal.** STUN on UDP 3478 to Parsec's servers. Each session
  start in the log shows `STUN reply from <parsec>:3478`, once over IPv6
  and once over IPv4. UPnP and NAT-PMP mapping are on by default
  (`upnp = 1`). The status codes `NAT_ERR_STUN_PHASE`, `NAT_ERR_PEER_PHASE`,
  `NAT_ERR_NO_CANDIDATES` and `NAT_ERR_JSON_ACTION` show an ICE-like
  process: gather candidates with STUN, swap them as JSON over the
  signalling WebSocket, then check them pairwise. Parsec gives a 97%
  traversal success rate. Symmetric NAT, double NAT and CGNAT are its
  documented failures. Relays (Parsec's "High Performance Relay") are a
  paid Teams feature; the default path is direct.
- **Ports.** The host's start port is random by default; port-forwarding
  guidance sets it to 8000. The SDK binds `hostPort` or `clientPort` and
  tries the next port up to 50 times. On this PC the log shows the host on
  UDP 30268 and the client on UDP 21268 (`net = BUD|<addr>|<port>`).

### Transport: BUD ("Better User Datagrams")

What Parsec has published:

- Runs on UDP. Peer to peer by default.
- Adds TCP-like reliability, with a heavily tuned congestion control of
  their own.
- The blog's priority order is **latency first, then frame rate, then
  quality**.
- Parsec tried TCP first: it "freaks out" under loss and adds latency.
  They also tried WebRTC and plain UDP; none gave TCP's reliability with
  UDP's latency.
- Video has no buffer on the client. So congestion has to be predicted
  before it happens, from network metrics. The aim is to fill the link to
  its capacity without loss or added delay. Under heavy loss, the response
  is to lower the encoder bitrate.
- BUD feeds network state to the capture, encode, decode and render stages,
  and takes state back from them.
- A short outage shows "your network is bad" and pauses the stream. The
  session ends after 60 seconds. One guest's trouble does not affect other
  guests.

From the SDK header and logs:

- `protocol`: `PROTO_MODE_BUD` (1, "low-latency optimized") or
  `PROTO_MODE_SCTP` (2, "compatible with WebRTC data channels"; this is
  what the browser client uses). `mediaContainer`: native, or MP4 for
  browser MSE.
- Network status codes include `NETWORK_ERR_BG_TIMEOUT -12007`, `BAD_PACKET`,
  `BUFFER` and `INTERRUPTED`. The log has a real case: on 2026-10-07,
  `bud_write_packet[470] = -710022` repeats every 2 s for 60 s, then `Client
  disconnected with status code: -12007`. That matches the documented
  60-second grace period, with a keepalive or retry about every 2 s.
- The SDK reports `networkLatency` as a round-trip time. The client's
  metrics file gives per-stream network latency, queued frames, decode
  latency and bitrate, as mean, minimum, maximum, deviation and variance.
- The host logs one line per second per stream:
  `FPS:<sent>/<n>, L:<mean>/<max> ms, B:<used>/<cap> Mbps, N:<a>/<b>/<c>`.
  On the LAN this reads, for example, `FPS:59.8/0, L:8.2/12.6, B:6.3/7.9`.
  `B` is the bitrate in use against the cap the congestion controller
  allows, and that cap moves second by second (4.9, 7.9, 10.0). This is the
  "lightning fast" bitrate adaptation in action. `N` is all zero on a clean
  LAN, probably network loss and retransmit counters (inferred).

### Encryption and key exchange

- Published: every BUD packet is encrypted with **DTLS 1.2 (OpenSSL)**,
  AES-128 or AES-256.
- Observed today: every session start logs `BUD AES_GCM = 256`, and the SDK
  has `AES_GCM_ERR_*` codes as well as `DTLS_ERR_*`. Current builds
  therefore use AES-256-GCM on the data path. Where the key comes from is
  not documented. Since peers are only introduced through the
  authenticated signalling WebSocket, the key or the peers' certificates
  most likely travel through Parsec's backend (inferred). Either way, the
  trust anchor is Parsec's service, not anything a third party holds.

### Video, audio and input framing

- **Pipeline.** Capture with Desktop Duplication straight into GPU memory,
  then the vendor hardware encoder called directly (NVENC, AMF, QSV, with no
  wrapper), then network, then hardware decode to NV12 in video memory, then
  a pixel-shader NV12→RGB conversion into the back buffer. The frame never
  touches system memory. Codecs are H.264 and H.265; this PC negotiates
  `codec = h265` with `decoder = nvidia`, `format = NV12`, `fullrange =
  true`. 4:4:4 is probed (`mfx_caps … 444`). Several displays travel as
  separate video streams, `client_video_0` to `4` in the metrics.
- **Presentation.** V-sync is on by default with the flip-sequential swap
  effect. When frames pile up because the host runs slightly faster than
  the client's display, the extra frames are **dropped**; there is no
  smoothing buffer. Parsec measured 4 to 8 ms end to end on a LAN at
  240 fps. Its usual LAN figure is about 7 ms added to the game's own
  latency.
- **Audio.** The client receives 48 kHz stereo signed 16-bit PCM. The host
  submits float or int16. The codec on the wire is not stated (it is
  probably Opus, inferred). The client can also send microphone audio
  (`Host's virtual microphone is enabled`).
- **Input.** Separate small messages, not a state snapshot: keyboard
  (`code`, `mod`, `pressed`), mouse button, mouse wheel (`x`, `y`), mouse
  motion (`x`, `y`, `relative`), gamepad button (`id`, `button`, `pressed`),
  gamepad axis (`id`, `axis`, int16 `value`) and gamepad unplug. The host
  sends cursor updates separately: position, hotspot, a mode flag
  (relative or absolute) and an image as PNG or RGBA. Hosts get virtual
  gamepads and USB through their own drivers (`vusb`, `vdd` for virtual
  displays).
- **User data.** Arbitrary UTF-8 messages in both directions, on a numbered
  channel.

## What makes it low-latency

Taken together, the published material comes down to six choices:

1. **No buffering anywhere.** Each frame is decoded and shown as soon as it
   is complete. Late frames are dropped, not queued. There is no jitter
   buffer.
2. **UDP with selective reliability.** Lost packets are recovered by
   resending them while the frame can still make its display deadline,
   instead of blocking the whole stream as TCP does. Control and input
   stay reliable and in order.
3. **Congestion control that predicts.** The bitrate cap is held just under
   what the link can carry, adjusted every second or faster. Rising delay
   is read as congestion before loss happens. Under heavy loss the encoder
   bitrate drops; frames do not queue.
4. **A transport that talks to the codec.** Network state goes back to the
   encoder (bitrate, a keyframe or refresh after a loss) and to the
   renderer.
5. **A zero-copy GPU pipeline** at both ends, with vendor encoder and
   decoder APIs called directly.
6. **One direct path.** Peer to peer, with no relay in the default path.

## How windowcast would build the same thing

windowcast already has most of the parts around such a transport:

- **Finding the peer and crossing NATs without a relay.**
  `windowcast-rendezvous` uses STUN, Syncthing global discovery and UDP
  hole punching. `transport::punched` already carries a reliable stream
  over the punched socket. This is the same job as Parsec's STUN plus
  signalling WebSocket, without a central account service.
- **Authenticated peers.** Pinned Ed25519 identities and SPAKE2 PIN pairing
  (`identity`, `pairing`, docs/SECURITY.md) take the place of Parsec's
  account and session IDs.
- **Parity.** `gamestream::fec` already implements Reed-Solomon parity for
  GameStream audio and video.
- **Hardware encode** on the Windows agent and MediaCodec decode on Android
  already exist.

The new piece is a datagram transport, call it `Direct`. Its parts:

| Layer | Design (standard crypto, our own peers only) |
|---|---|
| Keys | A Noise `IK` handshake (or `KK` once both are pinned) over the punched socket or the LAN socket, using the pinned Ed25519 identities converted to X25519. The alternative is an HKDF export from the session's existing authenticated DTLS. Either way, one asymmetric handshake per session, as SECURITY.md already requires. |
| Datagram AEAD | AES-128-GCM, or ChaCha20-Poly1305 on cores without AES hardware, matching SECURITY.md. The nonce is a 64-bit packet number, with a replay window. No Parsec keys and nothing from their backend. |
| Packet header | Stream id (video per window, audio, control, input), frame number, fragment index and count, packet number, and send timestamp. The header is authenticated as associated data. Fragments sized to the path MTU (about 1200 bytes by default, probed upward on the LAN). |
| Loss recovery | Reed-Solomon parity per frame, with the parity ratio driven by the measured loss (reuse `gamestream::fec`), plus a NACK for a missing fragment when the frame can still make its deadline (RTT < remaining budget). After that, ask for a refresh: intra refresh or a long-term reference rather than a full IDR, where the encoder supports it. |
| Reliable channels | Control and input travel as a small ordered, acknowledged stream: sequence numbers, cumulative and selective ACKs, resends at about RTT × 1.5. Gamepad and mouse messages may be marked "latest wins", so a stale resend is dropped. |
| Congestion control | Send-side estimation as in WebRTC's GCC or a BBR variant. The receiver sends feedback every 10 to 20 ms (packet number and arrival time). The sender tracks the one-way delay gradient and loss, and sets the target bitrate that is passed to the encoder each frame. It is predictive: rising delay cuts the bitrate before loss. |
| Pacing | Each frame's packets go out spread across part of the frame interval, not as one burst, so switch and Wi-Fi queues stay short. |
| Presentation | No jitter buffer. A frame is decoded as soon as all its fragments are in. A frame that cannot be shown before the next one arrives is dropped (Parsec's frame dropping). |
| Liveness | A keepalive about every 1 to 2 s. After a few seconds without packets the stream pauses and shows "network is bad"; after 60 s it ends (Parsec's documented behaviour). |

### Its place among windowcast's backends (needs the owner's decision)

GameStream already gives windowcast a non-WebRTC, low-latency UDP path
with parity. That path is tied to what Moonlight and Sunshine expect: its
crypto, ENet control and RTSP setup. `Direct` would do the same job for
windowcast-to-windowcast sessions, with windowcast's own keys and a
congestion controller we own. That breaks the "one mechanism per job" rule
unless the jobs are split clearly. The proposal:

- **`Direct`** (Parsec-style): the default backend for `Game` content
  between windowcast peers, and later for low-latency `General` streams.
- **`GameStream`**: kept only to work with stock Moonlight, Sunshine and
  Apollo.
- **`Native`** (WebRTC): stays the control plane and the default for
  general windows, where WebRTC's jitter buffer and generic congestion
  control are fine.
- The parity code moves out of `gamestream` into a shared module, so both
  backends use the same implementation.

Splitting or merging backends is the owner's call (standing rule: ask
before structural changes). This study proposes it and does not decide it.

## Working with stock Parsec

It is not possible, and it is not a goal. The reasons:

- **Signalling and authorisation run through Parsec's backend.** Session
  IDs, peer IDs and candidate exchange go over `kessel-ws` with a logged-in
  account. A stock Parsec host only answers peers introduced that way.
  Getting around that would mean defeating their access control, which is
  ruled out.
- **The keys come from that same path.** The data path is AES-256-GCM, with
  keys or certificates arranged through the authenticated signalling
  (inferred, see above). An outside client cannot hold them without Parsec's
  service.
- **The framing is proprietary and undocumented** beyond the summary above.
  Working it out would mean disassembling `parsecd-*.dll`, which is not
  approved.
- **The binaries are signed and update themselves** (the loader picks a
  verified, hashed `parsecd-<build>.dll`; see `appdata.json`). Any
  compatibility we managed would break silently with each release.

Even a client using Parsec's real SDK with the owner's account would route
signalling through Parsec, which is exactly what the owner ruled out ("No
routing").

## Legal and terms of service

Plain notes, not legal advice.

- The plan uses Parsec's published *ideas*: UDP with selective
  reliability, predictive congestion control, no buffering, a zero-copy
  pipeline. Ideas and protocol designs are not protected by copyright, and
  every one of these is standard practice (WebRTC GCC, BBR, QUIC loss
  recovery, Moonlight's FEC). No Parsec code, binary, key or wire format is
  copied, and no Parsec service is used. Parsec's terms (Unity's Terms of
  Service plus the "Parsec Additional Terms") govern using Parsec's software
  and service. A from-scratch transport that never touches either is
  outside them.
- Calling the backend "Parsec" or using Parsec's marks would raise
  trademark questions. Name it for what it is (`Direct`) and cite Parsec's
  blog only as inspiration in the docs.
- No patent search was done. Parsec (Unity) may hold patents on parts of
  its streaming stack. The techniques above have long, well-documented
  prior art in public standards, but if this is ever distributed
  commercially, a patent search is the owner's call.
- The captures and logs used here are the owner's own traffic on his own
  machines. That is within the 2026-10-09 approval.

## Recommendation and effort

**Verdict: feasible, and worth building as windowcast's own low-latency
transport. Working with stock Parsec is not feasible and is not pursued.**

Effort, in agent-weeks of focused work, before tuning on the rig and the
console:

1. Noise handshake from the pinned identities, the datagram AEAD and replay
   window, and the packet header: about 1 week.
2. Fragmenting, parity (shared with GameStream), deadline-aware NACKs, and
   the reliable control and input channel: about 2 weeks.
3. Congestion control (delay gradient plus loss, receiver feedback) and
   pacing, wired to the encoder bitrate on the Windows agent: 2 to 3 weeks.
4. Unbuffered presentation with frame dropping, refresh requests on loss,
   wiring into `host-core`, `client-core` and the Android viewer, and the
   backend choice: 1 to 2 weeks.

That is about **6 to 8 weeks** in total, plus tuning on the BlueStacks and
emulator rigs and then the owner's console.

### An optional capture session (owner steps)

None of the above needs it. One capture would add measured BUD timing
(packets per frame, burst spacing, how often feedback is sent, packet
sizes) as tuning targets for steps 2 and 3. Packet contents are encrypted;
only sizes and timing would be read. The steps:

1. On this PC, as administrator in PowerShell:
   `pktmon filter add -t UDP` then
   `pktmon start --capture --pkt-size 128 -f <capture-dir>\parsec.etl`
   (128 bytes per packet is enough for sizes and timing).
2. From another device on the LAN, connect with the official Parsec client
   to this PC as host. Play something with motion for about 60 seconds,
   then leave it still for about 20 seconds, then disconnect.
3. `pktmon stop`, then
   `pktmon etl2pcap <capture-dir>\parsec.etl -o <capture-dir>\parsec.pcapng`,
   then `pktmon filter remove`.
4. Put `<capture-dir>` where the study agent can read it (the coordinator names the folder) and say so. The study agent then reads it
   with `tshark` in the container: packet sizes, inter-arrival times,
   ratio between directions. No packet is replayed or sent anywhere.

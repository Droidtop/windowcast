# NoMachine NX4: a feasibility study

Droidtop/tracker#442. Written 2026-10-09 from NoMachine's published
material and from the NoMachine install on the owner's PC (NoMachine
8.27.1, `C:\Program Files\NoMachine`): its configuration files and their
built-in documentation, its logs, its EULA, and the readable text strings in
its libraries. Nothing was disassembled or decompiled, and no connection
was made to any NoMachine server. The server's private key
(`etc\keys\host\nx_host_rsa_key`) was not read.

## The goal

The owner asked us to "TRY ... NX4". This study asks whether a windowcast
client could reach a NoMachine server (version 4 and later, today 8.x) with
a local account on that server, on the owner's own machines.

## Sources

- NoMachine's own `etc/server.cfg` and `etc/node.cfg`. Each setting is
  documented in comments: ports, connection methods, authentication methods,
  codecs and encoder modes.
- Logs: `C:\ProgramData\NoMachine\var\log\nxd.log`, `nxserver.log`,
  `node\*\session`, and the client's `%USERPROFILE%\.nx\*\session` and
  `options`.
- Readable strings in `bin\libnx.dll`, `libnxc.dll`, `libnxd.dll`,
  `libnxn.dll` and `libnxh.dll`: log messages, configuration names and the
  OpenSSL functions they call. This is the "static reading" the approval
  allows; see the legal section for where it stops.
- Public material: Wikipedia's "NX technology" article, NoMachine 4's 2013
  launch announcement (as quoted by secondary sources), and the GPL nx-libs
  sources (nxcomp and nxproxy, the NX 3 lineage, also Droidtop/tracker#439).
  NoMachine's own knowledge base has no public protocol specification.

## What is documented or observable

### The layers

1. **The `nxd` service** listens on **TCP 4000 and UDP 4000** (`nxd.log`:
   `Listening for connections on any interface on port 4000`, `Accepting
   connections from any host with encryption enabled`, `Listening for UDP
   packets on port 4000`). The other connection methods are SSH (here
   `SSHPort 4022`, NoMachine's own `nxssh`) and HTTP or HTTPS (4443, the
   web player). `ClientConnectionMethods` defaults to `NX`.
2. **TLS on the TCP connection.** `libnx.dll` and `libnxd.dll` carry the
   OpenSSL cipher list `ECDHE-RSA-AES128-GCM-SHA256:ECDHE-RSA-RC4-SHA`. That
   is TLS 1.2 with ECDHE key exchange and RSA authentication from the
   server's host key (`nx_host_rsa_key`), and an RC4 fallback. The server
   certificate is self-made from that host key. The client checks it by
   **fingerprint on first use**: `NXShellAcceptCertificate(certHost,
   certHash, certData)`, and "Encryptable: ERROR! Fingerprint for ...".
   Optionally the server also checks a client certificate
   (`EnableNXClientAuthentication`).
3. **Login.** The `AcceptedAuthenticationMethods` are `NX-password`,
   `NX-private-key` (key-based login), `NX-kerberos` and, over SSH,
   `SSH-system`. On Windows, `NX-password` checks the Windows account. The
   log has an example: a 2024 attempt failed with "Username is not in the
   expected format".
4. **Session negotiation** uses a line-based text protocol whose replies
   start `NX> <code>`. Codes seen in `libnxd.dll` include `NX> 250
   Properties`, `NX> 667/668 TCP|UDP handle pid=… socket=… cookie=…`, `NX>
   669 UDP communication`, `NX> 671 UDP forwarder` and `NX> 672 UDP
   communication close`; the updater logs `NX> 162 Disabled service`.
   Public sources say version 4 kept this text protocol compatible with
   version 3. The NX 3 commands (`hello`, `login`, `listsession`,
   `startsession`, `attachsession`) appear in the GPL FreeNX and nxclient
   sources. Each session gets a 128-bit cookie (`options`:
   `nx/nx,cookie=…,type=physical-desktop,id=…`).
5. **The proxy layer.** Once the session is set up, the connection becomes
   an **nxproxy link**. `libnxc.dll` still has nxproxy's usage text (`-C`
   / `-S` modes) and the version handshake `NXPROXY-%i.%i.%i`, with a
   fallback `NXPROXY-3.0.0-…`. It multiplexes channels, as in nxcomp:
   display, audio, HTTP, file system, printing, USB, smart card and network
   (`NXProxyAddDisplayChannel`, `NXProxyAddAudioChannel`, and so on).
   Messages such as `CodeBeginCongestion` and `CodeEndCongestion` and the
   pack-method names `16m-h264` and `16m-vp8` come from nxcomp's framing,
   extended by NoMachine.
6. **The "RT" real-time channel over UDP**, for multimedia. The TCP
   session hands out the UDP endpoint, port (`UDPPort` defaults to
   4011–4999) and a key (`No RT encryption key was specified`, `No RT
   encryption iv was specified`). `libnx.dll` imports `BF_set_key` and
   `BF_cfb64_encrypt`: the RT datagrams are encrypted with **Blowfish in
   CFB64 mode**. Wikipedia describes the same: the UDP port and Blowfish
   key are agreed over the secure TCP link. The channel counts "RT messages
   in, out, lost, corrected", so it has some loss correction. It falls back
   to TCP when UDP is blocked, and it is turned off when the session goes
   through SSH. UPnP and NAT-PMP mapping (`EnableUPnP`) are available.
7. **Media.** Video uses H.264 (`libx264` for software, hardware through
   Intel `libmfx` and `libvpl`, AMD `libvce`, and NVIDIA) or VP8, with
   MJPEG as a fallback (`DisplayServerVideoCodec h264|vp8|mjpeg`). The frame
   rate defaults to 30 and is chosen automatically. The rate-control
   `EncoderMode` is `auto|bitrate|quality`. Audio uses Opus, Speex or Vorbis
   (`libopus`, `libspeex`, `libvorbis`). Input travels on the display
   channel, as X11-style events in the nxcomp tradition (the Windows build
   ships an xkb tree and `nxkb.exe`).

### What is not observable without going further

The framing inside the display and RT channels, the version-4 extensions
to nxcomp's message set, the input event encoding, how the encoders are
negotiated, and how the RT keys are derived. None of this is published.
Getting it would take disassembly of `libnx*.dll`, or long trial and error
against the server, and neither is approved.

## What a windowcast client could realistically do

| Step | Feasible? | Why |
|---|---|---|
| TLS to `nxd` on TCP 4000, accepting the host key by fingerprint on first use | Yes | Standard TLS 1.2, ECDHE-RSA-AES128-GCM. Trust on first use is what the official player does too; no access control is bypassed. |
| `NX-password` login with the owner's local account | Probably | The v3-compatible text protocol is documented in GPL sources. The exact v4+ greeting and options have to be checked against a capture. |
| Start or attach a session (`type=physical-desktop`) | Probably | Same text protocol. The cookie and session id come back in the reply. |
| Switch to nxproxy and speak the display channel | **No (blocked)** | NoMachine's v4+ display framing (H.264/VP8 inside nxcomp-derived messages) is closed and versioned (`NXPROXY-x.y.z`). Neither the open nx-libs nxproxy (3.x) nor anything published matches it. |
| RT channel over UDP | **No (blocked)** | Same problem, and the cipher (Blowfish-CFB64, 64-bit blocks, no integrity) is one windowcast should not adopt. |
| Input, audio, clipboard | No | They depend on the closed display channel. |

So a client could log in and set up a session, then hit a closed
proprietary data plane. Reading that data plane would mean disassembling
NoMachine's libraries. Each NoMachine release can change it (the proxy
versions are negotiated, and releases come every few weeks: 8.26.2 to
8.27.1 on this PC within a month), so any compatibility would keep
breaking.

## What blocks it

- **A closed, versioned, undocumented data plane.** This is the decisive
  blocker. Nothing in it uses keys we cannot hold: the TLS session and the
  RT key are open to a legitimate client. The problem is that the format is
  unknown.
- **Pinned certificates** are not a blocker. The server's certificate is
  checked by fingerprint on first use, like SSH, and a windowcast client
  could do the same honestly.
- **Signed binaries** are not a blocker for a client of our own. NoMachine
  signs its binaries (DigiCert), but a client never has to load or change
  them.
- **Licensing on the server side.** The free NoMachine edition allows one
  connection to the physical desktop. A second client, ours, would take
  that slot, which is fine for testing.

## Legal and EULA

Plain notes, not legal advice.

- The NoMachine EULA, section 3.2: "You may not decompile, decrypt, reverse
  engineer, disassemble or otherwise reduce the Software to human-readable
  form ... except to the extent the foregoing restriction is expressly
  prohibited by applicable law." The EULA is governed by **Luxembourg law**
  (section 11 and the governing-law clause).
- Luxembourg applies the EU Software Directive (2009/24/EC). Article 6
  allows decompiling a program when that is needed to make an
  independently written program work with it. The conditions: the
  information is not otherwise readily available, the work is limited to
  the parts needed for that, and the results are not used for anything
  else. Article 8 makes contract terms that override this void. A
  windowcast client for NoMachine servers is the kind of independent
  program that article covers. That is an argument a lawyer would have to
  confirm for the owner's situation, not a green light. The owner's
  2026-10-09 approval does not cover disassembly, and this study did none.
- Reading the shipped configuration documentation, the logs and the
  readable library strings, and capturing the owner's own sessions, are
  normal administrator activity. They are within the approval.
- "NoMachine" and "NX" are NoMachine's trademarks. A windowcast backend
  would describe itself as "connects to NoMachine servers", not use the
  names as its own.
- None of NoMachine's binaries may be redistributed (EULA section 3.1).
  Nothing here needs them to be.

## Recommendation and effort

**Verdict: not recommended.** A windowcast client can get as far as TLS,
login and session setup with a local account, about 1 to 2 weeks of work.
After that it hits NoMachine's closed display and RT channels. Opening
those would take approved disassembly under the EU interoperability
exception, an estimated **3 to 6 months**, and the result would break with
NoMachine's frequent releases. For what the owner gets out of it, that does
not pay. windowcast's own host agents already stream windows from the same
machines a NoMachine server would run on. The other reach-a-foreign-server
cases are better met by open protocols:

- **X2Go / nx-libs** (open NX 3 lineage, GPL; Droidtop/tracker#439) for
  Linux servers that run X2Go.
- **RDP and VNC** (windowcast's `Rdp` and `Vnc` backends, #111) for
  everything else, NoMachine-managed machines included, since Windows and
  most Linux desktops can also serve RDP or VNC.

If the owner still wants NoMachine reach, the next step is the owner's
decision on disassembly, not more agent time. A capture session would only
confirm the TLS and text-protocol stages above; it cannot reveal the
encrypted data plane.

### An optional capture session (owner steps)

This only confirms stages 1 to 4: the TLS version and cipher actually
chosen, the certificate, the UDP ports, and whether the RT channel is in
use. The text protocol runs inside TLS, so its content stays unreadable in
a capture.

1. On this PC, as administrator in PowerShell:
   `pktmon filter add -p 4000`, then `pktmon filter add -t UDP` (this one
   catches the RT ports 4011–4999), then
   `pktmon start --capture --pkt-size 0 -f <capture-dir>\nx.etl`.
2. From another device on the LAN, connect with the official NoMachine
   client to this PC (the NoMachine service must be running) with your
   Windows account. Move a window for about 30 seconds, then disconnect.
3. `pktmon stop`, then
   `pktmon etl2pcap <capture-dir>\nx.etl -o <capture-dir>\nx.pcapng`,
   then `pktmon filter remove`.
4. Put `<capture-dir>` where the study agent can read it (the coordinator names the folder) and say so. The study agent reads the TLS
   handshake and the packet timing with `tshark` in the container.

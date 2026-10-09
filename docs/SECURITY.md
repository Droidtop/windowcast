# Security & authentication model

The naive version of "stream a window over WebRTC" — PIN just confirms a
human is present, WebRTC's DTLS handles the rest — has a real hole: WebRTC's
DTLS handshake is only as trustworthy as the SDP exchange that carries each
side's DTLS certificate fingerprint. If that exchange goes over an
untrusted rendezvous path (a relay server, a QR code photographed by
someone else, plain text typed across a network), an attacker who controls
that path can substitute their own fingerprint and sit in the middle — a
PIN that's only checked out-of-band, and never actually bound into the key
agreement, doesn't stop that.

windowcast's credential is the device: the client device itself is the
identity, the same no matter who is sitting at it. This is the
Moonlight/GameStream shape: pair once with a device, stream to that device
from then on.

## Device credential: PIN-authenticated key exchange, not just a PIN check

Pairing uses [SPAKE2](https://datatracker.ietf.org/doc/html/rfc9382) (a
password-authenticated key exchange), seeded with a short PIN the host
displays. The PAKE run produces a strong shared secret *and* proves both
sides know the PIN — without the PIN, or anything equivalent to it, ever
crossing the wire, including the rendezvous/signaling path itself. See
`windowcast-pairing`.

That shared secret (put through HKDF-SHA256 to get a fixed-length key) then
authenticates each side's WebRTC DTLS fingerprint via HMAC-SHA256 *before*
the DTLS handshake proceeds. A substituted fingerprint fails this check —
the MITM is caught here, not discovered later. This is the same class of
technique [Magic Wormhole](https://magic-wormhole.readthedocs.io/) uses for
the same problem.

SPAKE2 deliberately does **not** reveal whether the two sides used the same
PIN at the key-derivation step itself (that's the point — no
password-guessing oracle). A wrong PIN instead makes both sides derive
*different* keys silently; it's the fingerprint-authentication step that
actually detects and fails on that. Skipping that step defeats the whole
design.

## How the descriptions are authenticated on the wire

`windowcast-transport::signaling` implements the two sections above and
the next one as one exchange over any byte stream (a LAN TCP socket today):

1. Both sides send `Hello`: protocol version, persistent Ed25519 public
   key, a fresh 32-byte nonce, and the client's mode (`Pair` or `Resume`).
2. `Pair` only: one SPAKE2 message each way, seeded with the PIN.
3. The client's offer and the host's answer are each signed with the
   sender's identity key over a transcript of: a domain label, the
   description kind, the mode, both nonces, both public keys, and the
   **complete SDP**. While pairing, each is also HMAC-SHA256-tagged with
   the PIN-derived key over the same transcript.

Authenticating the whole SDP rather than just the fingerprint line also
covers the ICE credentials and the media sections, so nothing in a
description can be swapped. The HMAC tag is what ties both public keys to
the PIN (only someone who knew the PIN can produce it over a transcript
naming their key); the signature proves the sender holds that key. On
`Resume` there is no tag, and the signer must already be pinned. The
nonces make every transcript unique, so a recorded exchange cannot be
replayed. webrtc's DTLS handshake then refuses any certificate whose
fingerprint differs from the one in the authenticated SDP.

A host that fails any check sends one generic "authentication failed"
and closes, so each wrong PIN costs a guesser a full connection, and the
reference agent withdraws its PIN after three failures. After the
connection is up, renegotiation (adding or removing a window track) rides
the control data channel inside the already-authenticated DTLS session.

## Device credential, every connection after the first: pinned Ed25519 identity

During that first PAKE-authenticated pairing, client and host each
generate a persistent Ed25519 keypair (`windowcast-identity`) if they don't
already have one, and exchange + pin public keys. Every later connection
authenticates via those pinned keys — sign a fresh nonce + DTLS fingerprint
at connect time — instead of repeating the PAKE/PIN. The PIN is a one-time
bootstrap, not something re-entered per session.

A paired peer's public key is a revocable grant (`TrustStore::revoke`), not
a permanent "once paired, forever trusted" record.

## Away from the LAN: addresses through third parties, never data

A host and a client it trusts find each other away from the LAN the way
Syncthing's devices do, with the code droidtop-agent uses too
(`windowcast-rendezvous`): each side learns the address its NAT gives its
rendezvous socket from public STUN servers, announces it to Syncthing's
global discovery under its discovery ID, looks the other up, and both send
towards each other until the NATs let a stream through. Only addresses go
to those servers. There is no relay of any kind: no TURN, no Syncthing
relays; when two NATs cannot be punched (both changing the port for every
destination), the session does not happen.

- **Who can be found.** A discovery ID is the SHA-256 of a certificate made
  from a key derived from the device's identity seed and the label
  `windowcast discovery certificate v1`; it reveals nothing about the
  identity itself. The two sides tell each other their IDs over a session
  they already trust (`ControlMessage::Rendezvous`, on the LAN first) and
  keep them beside the trust store (`remote-peers.json`, `remote-hosts.json`).
  A host looks up only clients it trusts and punches only towards them.
- **What the discovery servers learn.** That a device with this ID is at
  this address, as for any Syncthing device. They see no identity, no
  window, nothing of the session.
- **Signaling over the punched stream** is the same authenticated
  offer/answer as on the LAN: whoever sees or alters the datagrams cannot
  substitute a description. A host takes no pairing from away; only
  clients it already pinned get in.
- **The session** crosses the NATs with ICE, its candidates including the
  addresses STUN reports, inside the same DTLS-SRTP as on the LAN.
- **Off by default** on hosts ("Reachable away from home"); a client only
  looks for a paired host away when it does not answer on the LAN, unless
  told never to.

## Bulk media/data encryption

WebRTC mandates DTLS-SRTP. windowcast requires AES-128-GCM (hardware-
accelerated via AES-NI / ARMv8 Crypto Extensions on essentially every
target platform, including Android's ARM cores) as the SRTP cipher, with
ChaCha20-Poly1305 as the software fallback on cores without AES hardware
acceleration. Both are AEAD — authenticated and encrypted in one pass, not
a bolt-on MAC.

"Each window is its own channel" is where this matters for performance:
windowcast does **not** pay a new DTLS/ECDHE handshake per window. One
`PeerConnection` (one DTLS session, one ECDHE key agreement) per
client<->host session multiplexes every open window as a separate
track/data-channel within it — opening or closing a window is cheap (add
or remove a track), while the expensive asymmetric handshake happens once
per session. Every track rides the same already-negotiated AEAD keys via
SRTP's own per-packet nonce derivation, so windows stay cryptographically
independent streams without independent handshakes.

## Input reaches only the windows a session streams

Input from a client (pointer, keys, text, touch, gamepads) is accepted
only for windows that session is streaming: a pointer or touch event for
any other window is dropped, and keys and text go to the last streamed
window the client pointed at, never to whatever happens to have focus on
the host. A paired client therefore cannot drive a window it was not
given with them. Gamepads are different by nature: a session's pads are
virtual devices on the whole host (ViGEmBus or uinput), read by whatever
application reads pads, so they are accepted only while the session
streams a window and only while the host allows input, and they are
unplugged when the session ends. The clipboard is shared both ways while
a session is open.

## Authorization is separate from authentication

A pinned identity proves *who* is connecting, not *what* they're allowed to
see. A host agent is expected to maintain a per-identity grant list — by
default, prompting the host user to approve a new client's first request
for each window (or a coarser "this client may see any window" grant, at
the host user's choice) — independent of the transport/crypto layer
entirely. "Stream any window on my desktop" is a much bigger attack surface
than a fixed, host-configured allowlist, so this authorization step is not
optional scaffolding — it's load-bearing.

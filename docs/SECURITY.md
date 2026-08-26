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

windowcast has **two separate credential types**, not one model stretched
to cover both cases:

- A **device credential** identifies one specific machine — the client
  device itself is the identity, and it's the same identity no matter who
  is sitting at it. This is the Moonlight/GameStream shape: pair once with
  a device, stream to that device from then on.
- An **account credential** identifies a person, who may connect from any
  client device. This is the RDP/RemoteApp/NoMachine shape: a user logs
  in, and it doesn't matter which machine they're logging in from.

Real deployments need both — "pair my handheld with my home PC" is a
device relationship; "let anyone on my team remote into their own desktop
from whatever machine they're at" is an account relationship. Neither one
is a special case of the other, so windowcast keeps them as two credential
types that both feed the *same* downstream mechanism (authenticating a
WebRTC session's DTLS fingerprint) from two different trust roots.

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

## Device credential, every connection after the first: pinned Ed25519 identity

During that first PAKE-authenticated pairing, client and host each
generate a persistent Ed25519 keypair (`windowcast-identity`) if they don't
already have one, and exchange + pin public keys. Every later connection
authenticates via those pinned keys — sign a fresh nonce + DTLS fingerprint
at connect time — instead of repeating the PAKE/PIN. The PIN is a one-time
bootstrap, not something re-entered per session.

A paired peer's public key is a revocable grant (`TrustStore::revoke`), not
a permanent "once paired, forever trusted" record.

## Account credential: directory-issued session certificates

`windowcast-directory` is a self-contained account/login system: a real
account database (Argon2id-hashed passwords, no external dependency for
v1 — OIDC/SSO is a deliberate later extension point, not built now) and a
certificate authority the directory itself operates.

At login, the directory verifies the account's password, then mints a
**session certificate**: a small signed claims structure (account name,
role, expiry, and — critically — the *client's own* Ed25519 public key for
this login session) signed with the directory's CA key. A host that trusts
this directory (by pinning the CA's public key, not each individual user's
key) accepts any certificate that CA vouches for.

Two checks matter, not one: verifying the certificate's signature proves
the *claims* genuinely came from a trusted directory, but a host must
*also* require the presenter to sign a fresh nonce/DTLS fingerprint with
the private key matching `session_peer_id` in those claims — otherwise a
captured certificate (the claims are not secret) could be replayed by
anyone, not just the account holder who actually logged in. The
certificate proves the directory vouches for this account; the signature
proves whoever is connecting right now actually holds that session's key.

Session certificates are short-lived (`DEFAULT_SESSION_TTL_SECONDS`, 12
hours) and re-minted per login, not per connection — a revoked account
(`AccountStore::revoke`) simply can't log in again to get a new one, and
existing certificates age out on their own rather than needing active
revocation-list distribution to every host.

This is deliberately *not* a device credential in disguise: the same
account can hold a different `session_peer_id` on every device it logs in
from, and a host authorizing "this account" is authorizing the person, not
whichever machine happens to be running the client this time.

## Directory-mediated sessions: authenticate-and-broker, not intercept

A directory can also help two peers behind restrictive NATs actually
connect, via a **blind TURN relay** (`transport::Session::with_relay`) —
this is standard WebRTC TURN, not a windowcast-specific protocol. The
directory relays opaque DTLS-SRTP packets it cannot decrypt; it never
becomes a party to the encrypted session and never sees window content.
This was a deliberate choice, not a limitation to work around later: a
terminating proxy (one that decrypts and re-encrypts to inspect or log
content) is a fundamentally different trust model — a real, designed-in
man-in-the-middle — and nothing in windowcast does that today.

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

## Authorization is separate from authentication

A pinned identity proves *who* is connecting, not *what* they're allowed to
see. A host agent is expected to maintain a per-identity grant list — by
default, prompting the host user to approve a new client's first request
for each window (or a coarser "this client may see any window" grant, at
the host user's choice) — independent of the transport/crypto layer
entirely. "Stream any window on my desktop" is a much bigger attack surface
than a fixed, host-configured allowlist, so this authorization step is not
optional scaffolding — it's load-bearing.

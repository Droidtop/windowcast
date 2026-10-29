# The Android viewer: opening a host from another app

The viewer (`dev.windowcast.viewer`) is windowcast's Android client. Besides
its own form (an address, and a PIN the first time), another app can ask it to
open a host's stream: droidtop does, after it asks a computer to start a game
with `stream: windowcast` (Droidtop/tracker#552).

## The contract

Either form opens the viewer on that host:

- **A link**, `ACTION_VIEW`:
  `windowcast://connect?host=<device id>&address=<HOST:PORT>[&window=<title>]`
  (each value URL-encoded).
- **An explicit intent**, action `dev.windowcast.viewer.action.CONNECT`, with
  string extras `dev.windowcast.viewer.extra.HOST_ID`,
  `dev.windowcast.viewer.extra.ADDRESS` and, optionally,
  `dev.windowcast.viewer.extra.WINDOW`.

The values:

- **device id**: the host's identity, 64 hex digits (what the host shows as
  its device id, and what a session reports as its host id). Required.
- **address**: where the host listens, `HOST:PORT` (an IPv6 host in
  brackets), port 1 to 65535; nothing else (no path, user or spaces).
  Required.
- **window**: part of the title of the window to watch, compared without
  case. The viewer asks the host for its windows every 2 s until one matches,
  for up to 90 s (a game started just now takes a while to open), then
  watches it; without it, or when none matches in time, the window list is
  shown.

Decided 2026-10-16 (the owner's decision, relayed by the coordinator): the
viewer's entry point names the host. Both forms above carry the host's device
id and address, and only a paired host opens (next section); a request never
means "whichever host is nearby".

## Only paired hosts

A request never pairs and never asks for a PIN. The viewer connects without
one, which resumes only with a host in its own trust store (the hosts this
viewer has paired with, `files/windowcast`); and the host that answers must be
the one the device id names, or the session is closed ("it answered as another
computer"). A host the viewer has not paired with is refused with "only a host
this viewer has paired with opens this way": pair once from the viewer's form
with the host's PIN, and later requests open it directly.

If the viewer is already connected to that host it keeps the session and only
looks for the window. A request with a missing or malformed device id or
address is refused with a message and changes nothing. Parsing is
`android/windowcast/src/main/java/dev/windowcast/ConnectRequest.kt` (in the library, so any client can use it), tested
in `ConnectRequestTest.kt` (CI's `:windowcast:testDebugUnitTest`).

## Several windows at once, and typing

The owner's rule (Droidtop/tracker#118): "since windowcast is arbitrary window casting,
multiple streams is intended behavior. A window stream should only close when it is
instructed to, or when the session closes."

So the viewer streams as many windows as the user asks for:

- The first window watched fills the screen. Any other window watched while one is up (the
  menu's **Watch another window**, or a link from another app) gets a floating window of its
  own, with its own surface, decoder and sound, a **Keyboard** button and a **Close** button.
- Asking for a window that is already showing does nothing.
- A window stops only when the user closes it (Close, or Back), when the host closes the
  window, or when the session ends. Ending the full-screen window does not close the others,
  and a new connection or a lost one closes them all.
- Carrier switching (video or RDP pictures, compatibility mode, the private session) acts on
  the full-screen window only. A floating window shows the carrier the host started it on.

A stream keeps its window's shape: the picture is the largest one of the window's own
proportions that fits the screen, centred, with black bars, and it follows the host resizing the
window. The host's pointer is drawn over the picture, not over the bars.

Typing without a hardware keyboard: the menu's **Keyboard** (or the button on a floating
window) brings up the soft keyboard over the bottom of the screen; the stream does not move.
Keys go to the host as a hardware keyboard's would; a character a keyboard can only commit as
text, such as an emoji, goes as text input. Keys typed while a floating window is up go to the
host and follow whichever window was touched last.

## Getting the viewer

Each windowcast release from the one carrying this change has
`windowcast-viewer-<version>.apk`, signed with windowcast's release key
(`.github/workflows/viewer-release.yml`, run when a release is published; its
SHA-256 is in `SHA256SUMS-viewer`). Every release is signed with the same key,
so each installs over the last; the debug viewer is signed with another key
and does not.

Decided 2026-10-16 (the owner's decision, relayed by the coordinator): viewer
APKs are published as signed releases, by `viewer-release.yml`, with the
release key. The key is derived from Droidtop Core; its certificate's SHA-256
is `24cfa96c7426467cf1ca4228916acf9cd15c30e87c0eca92e8d1f102d51b3323`, and the
public certificate is `coordination/signing/windowcast/viewer-apk.pem` in
Droidtop/tracker. Check an APK against that fingerprint before trusting a
copy that did not come from a release.

# The Android library: one set of calls for every client

Owner direction (2026-10-29, Droidtop/tracker#428 and #115): remote streaming stays in the windowcast
library. droidtop embeds `windowcast-android-<version>.aar` and uses it for every remote stream; it
writes no streaming or protocol code of its own. The viewer app is the **reference implementation**
of a client, not droidtop's client: droidtop is its own client, through the library. So the viewer and
droidtop must make exactly the same calls, which means anything the viewer does that is not drawing
belongs in the library, not in `MainActivity`.

## What the library gives a client today

| Need | Call |
|---|---|
| Identity and trust store | `WindowcastClient(dataDir)` |
| Pair / reconnect | `connect(address, pin)` (no PIN resumes only a host already paired), `signInOptions` + `connectAccount` for account hosts |
| Follow a session | `EventPump(session, SessionListener)`: one thread, every `Event` delivered to the matching method, in order |
| List windows | `session.requestWindows()` → `onWindows` |
| Decide primary / extra / already showing | `WatchSet` |
| Switch a window's carrier (video / RDP pictures) | `CarrierSwitcher`: the switch under way, the first-picture handover, the host's refusals so a refused switch is not offered again |
| Wait for a window asked for by title | `WantedWindow` (time passed in; the viewer and droidtop wait the same way) |
| Open a host from another app | `ConnectRequest` (parses the link / intent contract of docs/ANDROID-VIEWER.md) |
| Stream a window | `startWindow`, `switchWindow`, `stopWindow`; `WindowDecoder`, `WindowPictures` render |
| Input, clipboard, audio, gamepads, terminal | `send`, `setClipboard`, `AudioPlayer`, `Microphone`, `Gamepads`, `openTerminal` |

Discovery of hosts (finding a computer to connect to) is not here yet: droidtop's paired computers
already know their addresses, and the directory and LAN discovery are Droidtop/tracker#113.

## What is still in the viewer and must move

`MainActivity` (about 1,700 lines) still decides, with no library call to share:
- popups and extra windows: which events open which floating window, placement against the owner;
- the pointer overlay and its shape cache; letterboxing (`Letterbox.kt`);
- the connect flow's order (try PIN, else sign-in options, host confirmation, password / browser /
  device sign-in);
- the host-state messages (locked, private session) as text.

Each moves behind a library type with the same shape as `WatchSet` and `WantedWindow`: plain Kotlin
where it can be, the Android pieces (surfaces, decoders) behind interfaces the client supplies. Order:
1. (this change) `EventPump` / `SessionListener`, `WantedWindow`; `ConnectRequest` and `WatchSet` move
   into the library; the viewer uses them.
2. (done) Carrier switching and refused-switch memory as `CarrierSwitcher` (plain Kotlin, tested).
3. The connect / sign-in sequence as a `ConnectFlow` that asks the client only for what a person must
   answer (a PIN, a password, a browser).
4. Popups and extras as a `WindowLayout` the client draws.
Then droidtop's `ComputerLaunch` calls the same types instead of opening the viewer.

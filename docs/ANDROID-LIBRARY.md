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
| Start, replace and end the one current session | `SessionHolder.start`: stops and joins the old reader, closes the old session, opens the new one, follows it, asks for its windows; `adopt` and `release` hand a live session across an activity's recreation; `end` closes it |
| Follow a session | `EventPump(session, SessionListener)`: one thread, every `Event` delivered to the matching method, in order (`SessionHolder` runs one) |
| List windows | `session.requestWindows()` → `onWindows` |
| Decide primary / extra / already showing | `WatchSet` |
| Switch a window's carrier (video / RDP pictures) | `CarrierSwitcher`: the switch under way, the first-picture handover, the host's refusals so a refused switch is not offered again |
| Wait for a window asked for by title | `WantedWindow` (time passed in; the viewer and droidtop wait the same way) |
| Sign in with an account | `signInOptions` → `SignInPlan.of(options)` (confirmation, methods, words) → `connectAccount`; `SignInAttempts` for waiting sign-ins |
| Open a host from another app | `ConnectRequest` (parses the link / intent contract of docs/ANDROID-VIEWER.md) |
| Stream a window | `startWindow`, `switchWindow`, `stopWindow`; `WindowDecoder`, `WindowPictures` render |
| Input, clipboard, audio, gamepads, terminal | `send`, `setClipboard`, `AudioPlayer`, `Microphone`, `Gamepads`, `openTerminal` |

Discovery of hosts (finding a computer to connect to) is not here yet: droidtop's paired computers
already know their addresses, and the directory and LAN discovery are Droidtop/tracker#113.

## What is still in the viewer and must move

`MainActivity` (about 1,700 lines) still decides, with no library call to share:
- popups and extra windows: which events open which floating window, placement against the owner;
- the pointer overlay and its shape cache; letterboxing (`Letterbox.kt`);
- the host-state messages (locked, private session) as text.

Each moves behind a library type with the same shape as `WatchSet` and `WantedWindow`: plain Kotlin
where it can be, the Android pieces (surfaces, decoders) behind interfaces the client supplies. Order:
1. (this change) `EventPump` / `SessionListener`, `WantedWindow`; `ConnectRequest` and `WatchSet` move
   into the library; the viewer uses them.
2. (done) Carrier switching and refused-switch memory as `CarrierSwitcher` (plain Kotlin, tested).
3. (done, as a plan rather than a threaded flow) The account sign-in rules as `SignInPlan` (confirm an
   untrusted host by fingerprint, then the methods offered, in order, and the words) and
   `SignInAttempts` (a cancelled or replaced browser/device sign-in is ignored when it answers). The
   dialogs stay the client's. The PIN connect is one call, `connect(address, pin)`.
3b. (done) Starting a session as `SessionHolder` (plain Kotlin over `HostedSession`, tested with a fake).
4. Popups and extras as a `WindowLayout` the client draws.
Then droidtop's `ComputerLaunch` calls the same types instead of opening the viewer.

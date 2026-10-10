# The command stream: shells, application launch and remote commands

Owner (Droidtop/tracker#444): most every network thing on a computer is
done over windowcast, so windowcast has a command stream: an interactive
shell when the user wants one, and a background channel that is always
available to the session, over which the client starts applications on the
host and runs other remote commands.

## One mechanism

A **command stream** is a set of numbered **channels** between a client and
a host, each opened for one purpose (`ChannelKind`) and carrying bytes both
ways until it ends. Everything the stream does is a channel kind:

| Kind | What the host does | Channel carries | Ends with |
|---|---|---|---|
| `Shell` | runs the user's shell on a pseudo-terminal (ConPTY on Windows, openpty elsewhere) | terminal bytes, both ways; `Resize` | the shell's exit |
| `Exec` | runs one command (argv), with a pseudo-terminal when asked | stdin in, output out | the command's exit code |
| `Launch` | starts an application detached from the stream | nothing, or its first output | `Exited` at once with the process id; the window it makes appears in the session's window list (`ListWindows`), where the usual selection rules pick its backend |
| later kinds | file and process operations | typed requests in `Data` | answered or refused |

The shell, application launch and every other command share the same
open/data/resize/eof/close/exit messages, the same authorization check,
the same limits (channels per session, chunk size), and the same client
object. A kind that is not built answers `Refused`.

Terminals are not chosen by selection: a user asks for a shell or for an
application, so no window content leads to a channel and `selection`
has no rule for it (docs/BACKENDS.md).

## Two transports, one client API

1. **On a windowcast session.** `ControlMessage::Command(CommandMessage)` on
   the control channel, inside the already authenticated DTLS session. The
   channels are always there while the session is: the client never needs a
   second connection to launch an application. The host serves them from
   `windowcast-host`.
2. **To any SSH server.** `windowcast-terminal` has an SSH client (russh)
   that opens the same channel kinds as SSH `session` channels (`pty-req` +
   `shell`, `exec`), so the client object behaves the same. Server
   identities are pinned: first connection records the host key
   (trust on first use, or a fingerprint the user typed), a changed key is
   refused. Authentication is a private key or a password.

The client side is `CommandStream` in `windowcast-client`: `open(kind)`
returns a `Channel` (write, resize, read events, close) whichever transport
is behind it. The screen model (`vt100`) that turns a shell's bytes into
cells lives in the client library too, so every viewer renders the same.

## Authorization and authentication

`windowcast_host::CommandAuthorizer` is the single check, called for every
`Open` with the `Principal` and the request:

```rust
pub struct Principal { pub peer: PeerId, pub account: Option<Account> }
pub trait CommandAuthorizer { fn authorize(&self, who: &Principal, kind: &ChannelKind) -> Result<(), String>; }
```

* `Account` is `windowcast_accounts::Account` (name, groups, method,
  provider). `Principal::account` is the account the session signed in
  with or its device is registered to (#443, docs/ACCOUNTS.md), `None` for
  a device paired by PIN.
* The host's own check is `PairedDevices` by default: any paired device
  may open every kind, as the owner asked (authorized by the existing
  pairing). `HostControl::set_command_authorizer` replaces it for sessions
  that start later.
* While account sign-in is on, `AccountPolicy` stands in front of it for
  every session: the policy rule matching the principal decides when it
  says `commands` (`false` refuses, `true` admits), and a rule that does
  not say leaves it to the host's own check. While sign-in is off the host's
  own check decides alone, as before accounts. One check per open, read
  from the policy at that moment.
* For SSH servers the server does the authenticating. The client supplies
  a password, a key, or a certificate a windowcast host issued for its
  own SSH key after an account sign-in (`SshAuth::Certificate`), never
  stores a password, and calls a `HostKeyPolicy` hook for unknown or
  changed keys.

## What is built

* **Protocol** (`protocol/src/command.rs`): `ControlMessage::Command`. Old
  peers drop it as an undecodable control message, so there is no version
  bump; a client that gets no answer to an `Open` within 15 seconds says
  the host may not support commands.
* **Host** (`host-core/src/command.rs`): `Shell` on a PTY; `Exec` on a PTY
  when asked, otherwise on pipes with standard output and error merged
  into one stream; `Launch` detached (null stdio, reaped), answered with
  `Opened{pid}` and `Exited` at once. The host runs them as the user the
  agent runs as, in that user's graphical session, so a launched
  application's windows are the ones `list_windows` shows.
  `HostControl::commands()` lists what is open. Closing a channel, or the
  session ending, kills the command.
* **SSH** (`terminal/src/ssh.rs`): `Shell` is `pty-req` + `shell`; `Exec` is
  `exec` (with `pty-req` when asked); `Launch` is an `exec` of
  `nohup ... & echo $!`, so it needs a POSIX shell on the server (it does
  not work on a Windows OpenSSH server). Host certificates are not
  supported (the key must be a plain host key).
* **Client** (`client-core/src/command.rs`): `ClientSession::open_channel`,
  `open_terminal`, `launch`, `exec`; `Client::ssh_connect` and `SshSession`
  with the same. The C interface is `ffi_terminal.rs` and `windowcast.h`.
* **Viewers**: the egui reference app and the Android library and viewer
  draw the screen snapshot; the emulator is in the library, not the viewers.

Not built yet: file and process kinds, X11/agent/port forwarding, SSH
logins with Kerberos tickets (GSSAPI; russh has the client side), mouse
reporting and text selection in the viewers.

## Limits and flow

Eight channels per session, 16 KiB per data message. Data rides the
reliable control channel; a flooding command can delay input events behind
it, which is acceptable for a baseline and the reason a dedicated data
channel is the follow-up if it shows.

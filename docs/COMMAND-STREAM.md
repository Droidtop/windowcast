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
pub trait CommandAuthorizer { fn authorize(&self, who: &Principal, open: &Open) -> Result<(), String>; }
```

* Today a principal is a paired device (`account: None`) and the default
  authorizer allows any paired device the shell kinds the host enables
  (`HostControl::set_commands`), as the owner asked: authorized by the
  existing pairing.
* #443's account layer (OIDC, LDAP/AD, Kerberos) fills `Principal::account`
  and installs its own `CommandAuthorizer`; that is the whole interface
  between the two pieces of work. It is not built here. The agreed shape
  is exactly the two items above; #443 may widen `Account`.
* For SSH servers the server does the authenticating. The client supplies
  a key or a password (`SshAuth`), never stores a password, and calls a
  `HostKeyPolicy` hook for unknown or changed keys. A later account layer
  can supply certificates or Kerberos tickets through the same `SshAuth`
  enum.

## Limits and flow

Eight channels per session, 16 KiB per data message. Data rides the
reliable control channel; a flooding command can delay input events behind
it, which is acceptable for a baseline and the reason a dedicated data
channel is the follow-up if it shows.

## Build order

1. protocol messages, PTY wrapper; 2. host service (`Shell`, `Exec`,
`Launch`) with the authorizer; 3. SSH client with pinning and auth;
4. client library: screen model, `CommandStream` over both transports,
C interface; 5. reference apps (egui terminal, Android viewer);
file/process kinds after.

# Upstream: release IronRDP on sspi 0.23.1 (picky 7.0.0-rc.27)

**For:** IronRDP (Devolutions). **Status:** nothing to file yet; recorded so
the pin below moves the day IronRDP releases.

windowcast's RDP crate (`rdp/`) takes every IronRDP crate from IronRDP's
repository at commit `f96f664abf0bdfe547f222bcd37e09899f00f310` (master, unchanged), because no released
IronRDP is on the stack that builds beside russh. The released
`ironrdp-server` 0.13 and `ironrdp-acceptor` 0.10 still use
`ironrdp-connector` 0.10, which needs `sspi` 0.21, and that pins
`ed25519-dalek = "=3.0.0-rc.1"` and other RustCrypto pre-releases exactly
(through picky 7.0.0-rc.25 and its macOS targets). russh 0.64 and 0.65
(the command stream, Droidtop/tracker#444; droidtop's SFTP,
Droidtop/tracker#573) need `ed25519-dalek` 3, and Cargo keeps one version
per compatible range, so the two cannot share a lockfile.

IronRDP master already moved: `ironrdp-connector` 0.12 on `sspi` 0.23.1 and
`picky = "=7.0.0-rc.27"`, `ironrdp-server` 0.14 on it. sspi-rs released
`sspi` 0.23.1 and `winscard` 0.4.1 on rc.27. Until now windowcast carried
patched copies of `ironrdp-connector`, `sspi` and `winscard` through
`[patch.crates-io]`; a patch applies only inside windowcast's own
workspace, so every program that depends on windowcast (droidtop-agent,
droidtop's `native/`) met the conflict again. A git dependency is a real
dependency and reaches them.

**When IronRDP releases `ironrdp-server` 0.14 (with `ironrdp-connector`
0.12, `ironrdp-session` 0.13 and the rest of that set),** replace the git
pins in `rdp/Cargo.toml` with those versions and delete this note.

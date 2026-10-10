# Draft upstream issue: release IronRDP and sspi on picky 7.0.0-rc.27

**For:** IronRDP (Devolutions; `ironrdp-connector` 0.10.0) and sspi-rs
(`sspi` 0.21.x). **Status:** draft for the owner to review and file; not
filed. Until a release exists, windowcast carries patched copies of
`ironrdp-connector`, `sspi` and `picky` under `vendor/` through
`[patch.crates-io]` (see `NOTICE` and each copy's README).

## Title

Exact pre-release pins in picky 7.0.0-rc.25 make IronRDP impossible to
build beside current RustCrypto users (russh 0.63+); please release on
picky rc.27

## Summary

`ironrdp-connector` 0.10.0 and `sspi` 0.21 require `picky = "=7.0.0-rc.25"`.
That picky pins RustCrypto pre-releases exactly:

- `curve25519-dalek = "=5.0.0-rc.1"`
- `ed25519-dalek = "=3.0.0-rc.1"`, `x25519-dalek = "=3.0.0-rc.1"`
- `ecdsa = "=0.17.0-rc.22"`, `p256`/`p384`/`p521 = "=0.14.0-rc.14"`,
  `primeorder = "=0.14.0-rc.14"`, `rustcrypto-ff`/`-group` `=0.14.0-rc.1`

(and `sspi` 0.21 pins `curve25519-dalek` itself). Cargo keeps one version
per semver-compatible range in a workspace, and `5.0.0-rc.1` and `5.0.0`
share one, so any other crate in the same workspace that needs the stable
releases cannot be built with IronRDP. One we hit: `russh` 0.63 and 0.64
(an SSH client) need `curve25519-dalek = "5"` and `ed25519-dalek = "3"`;
the error is

```
failed to select a version for `curve25519-dalek` which could resolve this conflict
  previously selected package `curve25519-dalek v5.0.0-rc.1`
    ... required by picky v7.0.0-rc.25
    ... required by ironrdp-connector v0.10.0
```

Older russh releases do not help: 0.62.x matches the dalek pins but needs
`p256 = "=0.14.0-rc.15"` (so a different `ecdsa` pre-release), and 0.60
needs `pkcs5`/`pkcs8` pre-releases that conflict with picky's `pkcs8 0.11`.

picky 7.0.0-rc.27 drops these exact pins (`ed25519-dalek = "3"`,
`x25519-dalek = "3"`, `p256`/`p384`/`p521 = "0.14"`, no direct
`curve25519-dalek`, `ecdsa`, `primeorder` or `rustcrypto-*`), and with it
everything resolves.

## Request

Release `sspi` and `ironrdp-connector` (and whatever else pins picky) on
`picky = "7.0.0-rc.27"` or later, ideally with a `^` requirement rather
than `=`, so applications can share the RustCrypto releases with other
crates.

## Evidence

- `ironrdp-connector-0.10.0/Cargo.toml`: `[dependencies.picky] version =
  "=7.0.0-rc.25"`.
- `sspi-0.21.3/Cargo.toml`: `[dependencies.picky] version =
  "=7.0.0-rc.25"`.
- `picky-7.0.0-rc.25/Cargo.toml` against `picky-7.0.0-rc.27/Cargo.toml`:
  the pins listed above, removed in rc.27.
- `russh` 0.64.1 requires `curve25519-dalek = "5"`, `ed25519-dalek = "3"`.

# sspi 0.21.3, patched for windowcast

This is the published crate `sspi` 0.21.3 from crates.io, unchanged
except for its `Cargo.toml` dependency requirements:

- `picky` (dependency and dev-dependency): `=7.0.0-rc.25` to `=7.0.0-rc.27`.
- macOS and iOS target dependencies, from exact pre-release pins to the
  releases picky rc.27 uses: `curve25519-dalek` `=5.0.0-rc.1` to `5`,
  `ed25519-dalek` `=3.0.0-rc.1` to `3`, `p256`/`p384`/`p521` and
  `primeorder` `=0.14.0-rc.14` to `0.14`, `rustcrypto-ff` and
  `rustcrypto-group` `=0.14.0-rc.1` to `0.14.0-rc.1` (no stable
  release exists), `rustcrypto-ff_derive` `=0.14.0-rc.0` to `0.14.0-rc.0`.

Why: IronRDP 0.10 (`ironrdp-connector`), `sspi` 0.21 and `winscard` 0.3
pin picky pre-releases (`=7.0.0-rc.25`, `=7.0.0-rc.26`), and picky rc.25
pins RustCrypto pre-releases exactly (`curve25519-dalek =5.0.0-rc.1` and
others). Those cannot share a workspace with `russh` 0.64, which the
command stream (Droidtop/tracker#444) uses: Cargo keeps one version per
compatible range, and `5.0.0-rc.1` and `5.0.0` share one. picky
7.0.0-rc.27 drops the exact pins, so these copies ask for it instead.

The workspace's `[patch.crates-io]` (root `Cargo.toml`) points here, and
the workspace excludes `vendor/`, so this code is neither linted nor
reformatted. Drop this copy and the patch entry once IronRDP, sspi and
winscard release on picky rc.27 or later
(docs/upstream/ironrdp-picky-rc27.md asks for that).

Licence: as upstream, MIT or Apache-2.0 (the crate's own licence files are
here); see the repository's NOTICE.

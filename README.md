<h1 align="center">windowcast</h1>

<p align="center">
  <b>Stream application windows, not whole desktops.</b><br>
  One window per stream, paired by PIN, encrypted end to end, written in Rust.
</p>

<p align="center">
  <a href="https://github.com/Droidtop/windowcast/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/Droidtop/windowcast?style=flat-square"></a>
  <a href="https://github.com/Droidtop/windowcast/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/Droidtop/windowcast/ci.yml?branch=main&label=ci&style=flat-square"></a>
  <img alt="Rust" src="https://img.shields.io/badge/Rust-library-b7410e?style=flat-square&logo=rust">
  <img alt="Hosts: Windows and Linux" src="https://img.shields.io/badge/hosts-Windows%20%C2%B7%20Linux-2f7bf5?style=flat-square">
  <a href="LICENSE"><img alt="License: GPL-3.0" src="https://img.shields.io/github/license/Droidtop/windowcast?style=flat-square"></a>
  <img alt="Status: early" src="https://img.shields.io/badge/status-early-d69e2e?style=flat-square">
</p>

<p align="center">
  <a href="#what-it-does">What it does</a> ·
  <a href="#what-works-today">What works</a> ·
  <a href="#install">Install</a> ·
  <a href="#how-it-stays-safe">Security</a> ·
  <a href="#faq">FAQ</a> ·
  <a href="#build">Build</a> ·
  <a href="#licence">Licence</a>
</p>

windowcast is a library for streaming single application **windows** from a host to a client. Each window travels over the protocol that suits it, and several streams can run at once. It is the core behind droidtop's remote-window feature, and it is kept independent of droidtop so other projects can embed it.

## What it does

| | |
|---|---|
| **One window at a time** | Pick a window from the host's list and stream just that one, with its own sound. |
| **Right protocol per window** | The native WebRTC tracks carry most windows. A game can go over GameStream and a text editor over RDP. The backend is chosen per window. |
| **Pairing by PIN** | A PIN shown on the host authenticates the connection, then both devices pin each other's identity for later reconnects. |
| **Input both ways** | Mouse, keyboard, typed text, touch and up to four gamepads go to the host. The clipboard goes both ways. |
| **Adapts to the network** | The host steps frame rate and picture size down when the link is thin and climbs back when it clears. |
| **Away from home** | Finds a paired device off the local network with hole punching. Addresses only, never data, and no relay. |

## What works today

windowcast is early. The identity, pairing and protocol pieces are real and tested, and the rest is built crate by crate.

| | Windows host | Linux host |
|---|---|---|
| Stream a window (H.264, H.265, AV1 tracks) | yes, GPU or software encoding | yes, Wayland (sway 1.11 and wlroots 0.19 or later), OpenH264 |
| Window sound | yes | yes |
| Input and clipboard | yes | pointer and keys under sway |
| Gamepads (up to four, as Xbox 360 pads) | through ViGEmBus | through uinput |

Clients: the Windows client decodes with Media Foundation and sends pointer and keys back. The Android library and viewer decode with MediaCodec.

Not done: macOS, rumble back to a client's pads, gamepads from the Windows client, clipboard and GPU encoding on Linux, and a microphone from the Linux client. The Android library builds in CI for arm64-v8a and x86_64 but has not yet been run on a device.

The full crate-by-crate list is in [docs/OVERVIEW.md](docs/OVERVIEW.md), and the backend design is in [docs/BACKENDS.md](docs/BACKENDS.md).

## Install

Every [release](https://github.com/Droidtop/windowcast/releases/latest) lists its files.

**Linux (x86_64 or aarch64)**, for you only and without root:

```
curl -fsSL https://github.com/Droidtop/windowcast/releases/latest/download/install.sh | sh
```

Add `-s -- --autostart` after `sh` to start the host agent when you sign in. The script checks the download against `SHA256SUMS-linux`, puts the programs in `~/.local/bin` and lists everything it creates in `~/.local/share/windowcast/install-manifest`. `~/.local/share/windowcast/uninstall.sh` removes exactly that list.

Other forms, with the same programs: `windowcast-linux-<arch>.tar.zst`, a single `.run` file, a `.deb` for Debian and Ubuntu, and an `.rpm` for Fedora and openSUSE. There is no Flatpak, because a sandbox would defeat a program that controls the session it runs in.

**Windows (x86_64):** download `windowcast-app-<version>-windows-x86_64.exe` for the reference app, or `windowcast-agent-windows-<version>-x86_64.exe` for the host agent alone. For gamepads on the host, install the ViGEmBus driver.

**Android:** `windowcast-viewer-<version>-debug.apk` is a test viewer, and `windowcast-android-<version>.aar` is the library for embedding.

To try it, run `windowcast-app --role host` on one machine and `windowcast-app --role client` on another, then enter the host's PIN in the client and pick a window.

## How it stays safe

WebRTC encrypts the media, but its handshake is only as trustworthy as the channel that carries the fingerprints. windowcast closes that gap with a PIN-based key exchange (SPAKE2) that authenticates the fingerprint exchange itself. A persistent Ed25519 identity takes over for every later connection. The full model is in [docs/SECURITY.md](docs/SECURITY.md).

## FAQ

**Is it a whole-desktop remote tool?**
No. It streams windows. A desktop can be one of them.

**Is it compatible with NoMachine, Moonlight or Parsec?**
It is a new design built on WebRTC. A stock Moonlight client can pair with its GameStream host and stream windows from it, but the native protocol is its own.

**Does it need a server?**
No. Devices find each other on the local network, or away from it by hole punching. There is no relay.

**Is it part of droidtop?**
No. droidtop uses it, and any project can embed it through the C interface in `client-core`.

## Build

```
cargo build --workspace
cargo test --workspace
```

The Android library and viewer have their own steps, and the Linux host agent needs Wayland and PulseAudio development headers. Both are in [docs/OVERVIEW.md](docs/OVERVIEW.md).

## Licence

GPL-3.0-only, see [LICENSE](LICENSE) and [NOTICE](NOTICE) for third-party crates. The RDP backend links IronRDP's protocol crates (MIT or Apache-2.0).

# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-09-28

### Added
- Core streaming stack: a versioned message protocol, persistent device identities with a pinned-peer trust store, and PIN pairing that authenticates the WebRTC fingerprint exchange, so an untrusted signaling channel cannot silently substitute keys.
- Multiple concurrent streams per session: each window or game stream is started and stopped on its own and carries its own backend, so the built-in WebRTC capture can run alongside a stream handed off to another protocol library.
- Account credentials through a directory server: password-based accounts and short-lived signed session certificates, so one host can serve many users without pairing every device individually.
- TURN relay support for sessions that need a relay to connect.
- A client for a local Sunshine/Apollo host that reads server information and parses the application list.
- A Linux host agent that lists the compositor's open windows, and a reference command-line client.

### Changed
- Session certificates now use the standardized PASETO v4.public token format instead of a custom envelope, with expiry enforced by the token format itself.
- CI cancels a run made obsolete by a newer push instead of letting both finish in parallel and report the same failure repeatedly.
- Rewrote the README introduction in a plainer, first-person voice.

### Fixed
- CI no longer leaves a write-capable repository token inside the checkout where build steps can read it, and pins the third-party steps it runs to exact revisions instead of moving tags.

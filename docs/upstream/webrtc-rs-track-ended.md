# Draft upstream issue: remote tracks never end, so a reused track id never arrives again

**For:** webrtc-rs (`webrtc` 0.21.0 / `rtc` 0.21.0). **Status:** draft for the
owner to review and file; not filed. Source references are to the published
0.21.0 crates.

## Title

`RTCTrackEvent::OnClose`/`OnClosing` are never emitted, and a remote track
with a previously used track id never fires `on_track` again

## Summary

When the remote peer removes a track and renegotiates, the receiving side
gets no event: `TrackRemote` never sees `OnEnding`/`OnEnded`. Because the
async `webrtc` driver keys remote tracks by track id and only removes an
entry on close, the entry stays forever, and a later track that reuses the
same track id (the same media source sent again, a common pattern) is
treated as already open: `on_track` is never called for it and its packets
go to the dead `TrackRemote` from before.

## Details

1. `rtc` declares the close events
   (`rtc-0.21.0/src/peer_connection/event/track_event.rs:186` `OnClosing`,
   `:191` `OnClose`), but nothing in `rtc-0.21.0/src` constructs either; the
   only mentions are the declarations and doc examples
   (`src/lib.rs:148`, `:628`, `track_event.rs:117`). Data channels do get
   theirs (`handler/datachannel.rs:451`, `:542`).
2. The `webrtc` driver handles `OnClosing`/`OnClose` by forwarding them to
   the `TrackRemote` (`webrtc-0.21.0/src/peer_connection/driver.rs:1286`,
   `:1291`), so the plumbing on that side exists and waits for events that
   never come.
3. On `OnOpen`, the driver creates a `TrackRemote` and calls
   `PeerConnectionEventHandler::on_track` only if
   `track_remote_events_tx` has no entry for the track id yet
   (`driver.rs:1199-1243`; the check exists for simulcast, where each
   layer's first packet fires `OnOpen` for the same id). With no close
   event the entry is never removed, so a second track under that id is
   "already open".

## Reproduction

1. Peer A adds a video track with id `t`, negotiates, sends media; peer B
   gets `on_track` and reads packets.
2. A calls `remove_track` and renegotiates; B's `TrackRemote` gets no
   `OnEnded` (reads simply stop).
3. A adds a new track, again with id `t` (fresh SSRC, `add_track` reusing
   the stopped transceiver or `add_transceiver_from_track` with a new
   one), renegotiates and sends media.
4. B never gets `on_track` for it.

We hit this in windowcast, where each streamed window is a track named
after the window: streaming a window, stopping it and streaming it again in
the same session delivered no frames. A two-peer loopback test (detach a
window's track, attach it again, expect a new remote track) failed with a
timeout on step 4 (windowcast commit 6e14368,
`transport/tests/loopback.rs`). Our workaround is a unique track id per
attach (`window-<id>-<ssrc>`), and the host telling the client on its own
control channel when a track stops, since the close event is missing.

## Expected

- `rtc` emits `RTCTrackEvent::OnClosing`/`OnClose` for a remote track when
  renegotiation stops its receiver (the transceiver's direction no longer
  includes receiving, or the m-section is rejected), and when the peer
  connection closes.
- The `webrtc` driver removes the `track_remote_events_tx` entry on
  `OnClose`, so a later track with the same id fires `on_track` again.

## Suggested change

In `rtc`'s handling of a remote description, where a transceiver's
receiver is stopped or its direction loses `recv`, queue
`RTCPeerConnectionEvent::OnTrack(RTCTrackEvent::OnClosing(id))` then
`OnClose(id)` for each of the receiver's tracks; in the `webrtc` driver,
remove the entry from `track_remote_events_tx` after forwarding `OnClose`.
A test: add a track, remove it, add a track with the same id; expect
`on_track` twice and `OnEnded` once in between.

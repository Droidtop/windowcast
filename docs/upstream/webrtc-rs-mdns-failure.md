# Draft upstream issue: a failing mDNS socket fails the whole peer connection

**For:** webrtc-rs (`webrtc` 0.21.0, `rtc-mdns` 0.21.0). **Status:** draft
for the owner to review and file; not filed. Source references are to the
published 0.21.0 crates.

## Title

Peer connections fail on a host where the mDNS multicast socket cannot be
opened (for example loopback-only), even though mDNS is optional

## Summary

With the default settings, binding the peer connection's transports opens a
multicast socket for mDNS and propagates its error with `?`. On a machine
where joining 224.0.0.251 fails, such as one whose only interface is
loopback (a phone or handheld with Wi-Fi off, a container or network
namespace with only `lo`), the whole peer connection fails, although host
candidates on loopback (or any other bound address) would connect without
mDNS at all.

## Details

- The default mDNS mode is `QueryOnly`
  (`rtc-0.21.0/src/peer_connection/configuration/setting_engine.rs:210`,
  `rtc-ice-0.21.0/src/mdns/mod.rs:20-21`).
- `bind_transports` opens the socket whenever mDNS is not disabled and
  returns early on failure:
  `self.mdns_socket = Some(runtime.wrap_udp_socket(MulticastSocket::new().into_std()?)?);`
  (`webrtc-0.21.0/src/peer_connection/driver.rs:443-444`).
- `MulticastSocket::into_std` binds and joins the group, each with `?`
  (`rtc-mdns-0.21.0/src/socket.rs:171-207`; the join is
  `socket.join_multicast_v4(&MDNS_MULTICAST_IPV4, &iface)?` at `:207`),
  which fails when no interface can carry multicast.
- The same function is careful about the UDP sockets themselves: the
  comment above it says only binding nothing at all is an error, and a
  single dead address is skipped (`driver.rs:430-433`, webrtc#874). The
  mDNS socket does not get the same treatment.

## Reproduction

Run any two-peer test inside a network namespace with only loopback up,
for example `sudo unshare -n sh -c 'ip link set lo up && <test binary>'`,
with the peers binding `127.0.0.1:0` (`with_udp_addrs`) and default
`SettingEngine` settings: building the peer connection fails. Setting
`with_multicast_dns_mode(MulticastDnsMode::Disabled)` makes the same test
connect over loopback.

windowcast hit this for same-device sessions (a client and host on one
device with no network up). We disable mDNS (windowcast commit 0c43a7f,
`transport/src/lib.rs`), and our CI reruns the two-peer tests in such a
namespace (commit 3beabf5).

## Expected

mDNS is an optimisation for candidate privacy and resolution; a peer
connection should not fail because it is unavailable. When the multicast
socket cannot be opened, log it, leave `mdns_socket` as `None` (gathering
and resolving `.local` candidates are then simply unavailable), and go on
with the other transports, as for a single UDP address that fails to bind.

## Suggested change

In `bind_transports`, replace the `?` on the mDNS socket with a match that
warns and continues:

```rust
if self.mdns_mode != MulticastDnsMode::Disabled {
    match MulticastSocket::new().into_std() {
        Ok(socket) => self.mdns_socket = Some(runtime.wrap_udp_socket(socket)?),
        Err(err) => warn!("mDNS unavailable, continuing without it: {err}"),
    }
}
```

and a test that builds a peer connection with default settings in a
loopback-only namespace (or with a `MulticastSocket` that fails) and
connects it.

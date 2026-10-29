# Finding a host on the local network, and waking it

Droidtop/tracker#113. Away from the LAN, hosts are found through the rendezvous directory and hole
punching (`windowcast-rendezvous`). On the LAN, until now, a client needed the host's address typed in.

## Local discovery

`windowcast-discovery` (`discovery/`). A host that has announcing on broadcasts a **beacon** every
3 seconds; a client listens for a moment and lists what it heard.

- **Transport:** IPv4 UDP broadcast to 255.255.255.255, port **47101**. No multicast group, no mDNS
  stack, no permission beyond the network. It is windowcast's own format, not mDNS: nothing else on
  the network needs to understand it, and a mDNS responder is not needed to find another windowcast.
  (Decided 2026-10-29: a mDNS responder and browser are two protocol stacks to maintain for a feature
  that is only "list the hosts here"; revisit if a platform needs the system's service browser.)
- **Beacon:** magic `WCB\x01`, the host's device id (32 bytes), its session port (2 bytes, not 0),
  a name length and the name (at most 64 bytes of text without control characters). Anything else
  (wrong magic, cut short, too long, bad text, port 0) is ignored by the listener.
- **Off by default.** A host announces only when its application turns it on
  (`HostControl::set_announce(Some(name))`; the reference app has the setting "Announce this computer
  on the local network"). A host that is not asked says nothing. The beacon carries the name, the
  device id and the port, and nothing about what is shared or who is connected.
- **Never trusted.** A beacon only says where to look. A client connects exactly as if the address had
  been typed: it resumes only with a host whose pinned identity matches, and pairs with the PIN the
  first time. A forged beacon can at worst send a client to the wrong address, where the identity check
  fails. Nothing is sent in reply to a beacon.
- **Clients:** the same call everywhere: Rust `windowcast_client::discover_hosts(window)`, C `windowcast_discover_hosts(window_ms, out, cap)` (JSON), Android `HostDiscovery.find(windowMs)`. `discover_hosts(window)` lists `DiscoveredHost { device_id, name,
  address }` heard within `window`, once per device id (the latest beacon wins). The sender's address
  with the beacon's port is the address to connect to.

Limits: broadcast does not cross routers, so a host on another subnet is not listed (use its address,
or the away path). Only one listener per computer can hold port 47101.

## Wake-on-LAN

`windowcast_client::wake_host(mac, directed)` (C `windowcast_wake_host`, Android `HostDiscovery.wake`) broadcasts the standard magic packet (six 0xFF bytes,
then the MAC sixteen times) to the LAN on port 9, and to `directed` (a subnet broadcast address)
when given. The computer's network card has to be set to wake on it, and it has to be on the same
network; a packet cannot wake what is off at the wall. Where the client learns a host's MAC is the
application's to decide (the host can report it in its own listing; not done yet).

## Not done

- A deployable directory and TURN relay (the code is there; running them for people is an operations
  decision for the owner).
- A host reporting its own MAC address, so a client can offer "wake" for a host it paired with.

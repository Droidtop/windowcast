//! Wake-on-LAN: the standard magic packet.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

/// The port conventionally used for magic packets (the discard service).
pub const PORT: u16 = 9;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("not a MAC address (six hex pairs separated by ':' or '-')")]
pub struct BadMac;

/// Reads `aa:bb:cc:dd:ee:ff` or `aa-bb-cc-dd-ee-ff`.
pub fn parse_mac(text: &str) -> Result<[u8; 6], BadMac> {
    let text = text.trim();
    let parts: Vec<&str> = text.split([':', '-']).collect();
    if parts.len() != 6 {
        return Err(BadMac);
    }
    let mut mac = [0u8; 6];
    for (slot, part) in mac.iter_mut().zip(parts) {
        if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(BadMac);
        }
        *slot = u8::from_str_radix(part, 16).map_err(|_| BadMac)?;
    }
    Ok(mac)
}

/// Six 0xFF bytes, then `mac` sixteen times: 102 bytes.
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut packet = [0xFFu8; 102];
    for copy in 0..16 {
        packet[6 + copy * 6..12 + copy * 6].copy_from_slice(&mac);
    }
    packet
}

/// Broadcasts the magic packet for `mac` to the local network (port 9), and
/// to `directed` as well when given (the subnet's broadcast address, for a
/// computer on another subnet of the same site).
pub fn send(mac: [u8; 6], directed: Option<Ipv4Addr>) -> io::Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    let packet = magic_packet(mac);
    socket.send_to(&packet, SocketAddr::from((Ipv4Addr::BROADCAST, PORT)))?;
    if let Some(address) = directed {
        socket.send_to(&packet, SocketAddr::from((address, PORT)))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_packet_is_six_ff_then_the_mac_sixteen_times() {
        let mac = [1, 2, 3, 4, 5, 6];
        let packet = magic_packet(mac);
        assert_eq!(packet.len(), 102);
        assert!(packet[..6].iter().all(|b| *b == 0xFF));
        for copy in 0..16 {
            assert_eq!(&packet[6 + copy * 6..12 + copy * 6], &mac);
        }
    }

    #[test]
    fn a_mac_is_read_with_either_separator_and_either_case() {
        assert_eq!(
            parse_mac("aa:BB:cc:00:1f:ff"),
            Ok([0xaa, 0xbb, 0xcc, 0x00, 0x1f, 0xff])
        );
        assert_eq!(parse_mac(" 01-02-03-04-05-06 "), Ok([1, 2, 3, 4, 5, 6]));
    }

    #[test]
    fn what_is_not_a_mac_is_refused() {
        for bad in [
            "",
            "aa:bb:cc:dd:ee",
            "aa:bb:cc:dd:ee:ff:00",
            "aa:bb:cc:dd:ee:gg",
            "a:b:c:d:e:f",
            "aabbccddeeff",
            "aa:bb:cc:dd:ee:+f",
        ] {
            assert_eq!(parse_mac(bad), Err(BadMac), "{bad}");
        }
    }
}

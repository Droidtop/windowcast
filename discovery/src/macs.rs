//! This computer's own hardware addresses, for Wake-on-LAN: what a client needs in order to wake it.
//!
//! A host reports them to the person at its own window and to the application around it
//! ([`local`]); nothing here goes on the wire, because windowcast's wire is version-locked and
//! a MAC is wanted only by the part that wakes (docs/DISCOVERY.md).

/// `aa:bb:cc:dd:ee:ff`, lower case.
pub fn format(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// The addresses of this computer's network cards that a wake packet can reach, best first: wired
/// before wireless on Linux (the order of the kernel's names), the order `getmac` lists on Windows.
/// Loopback, virtual and disconnected adapters are left out where the system says which they are.
pub fn local() -> Vec<[u8; 6]> {
    #[cfg(target_os = "linux")]
    {
        linux()
    }
    #[cfg(windows)]
    {
        windows()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
fn linux() -> Vec<[u8; 6]> {
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    let mut found = Vec::new();
    for name in names {
        let base = std::path::Path::new("/sys/class/net").join(&name);
        // A card has a `device` link; bridges, tunnels and veth pairs do not.
        if !base.join("device").exists() {
            continue;
        }
        let kind = std::fs::read_to_string(base.join("type")).unwrap_or_default();
        let address = std::fs::read_to_string(base.join("address")).unwrap_or_default();
        if let Some(mac) = accept_sysfs(kind.trim(), address.trim()) {
            if !found.contains(&mac) {
                found.push(mac);
            }
        }
    }
    found
}

/// One `/sys/class/net/<name>` entry: its `type` file (1 is Ethernet) and its `address`.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn accept_sysfs(kind: &str, address: &str) -> Option<[u8; 6]> {
    if kind != "1" {
        return None;
    }
    parse(address)
}

#[cfg(windows)]
fn windows() -> Vec<[u8; 6]> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = std::process::Command::new("getmac")
        .args(["/fo", "csv", "/nh"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match output {
        Ok(out) if out.status.success() => parse_getmac(&String::from_utf8_lossy(&out.stdout)),
        _ => Vec::new(),
    }
}

/// `getmac /fo csv /nh`: one line per adapter, `"Physical Address","Transport Name"`; an adapter
/// that is disconnected has `N/A` for its transport and a media-disconnected name, and is left out.
#[cfg(any(windows, test))]
pub(crate) fn parse_getmac(text: &str) -> Vec<[u8; 6]> {
    let mut found = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split("\",\"").map(|f| f.trim_matches('"')).collect();
        let (Some(address), Some(transport)) = (fields.first(), fields.get(1)) else {
            continue;
        };
        if transport.contains("N/A") || transport.to_lowercase().contains("disconnected") {
            continue;
        }
        if let Some(mac) = parse(address) {
            if !found.contains(&mac) {
                found.push(mac);
            }
        }
    }
    found
}

/// `aa:bb:cc:dd:ee:ff` or `AA-BB-CC-DD-EE-FF`; not all zeros, not a multicast address.
pub(crate) fn parse(text: &str) -> Option<[u8; 6]> {
    let mac = crate::wake::parse_mac(text).ok()?;
    (mac != [0; 6] && mac[0] & 1 == 0).then_some(mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mac_is_written_lower_case_with_colons() {
        assert_eq!(
            format([0xAA, 0x0B, 0xcc, 0x00, 0x1f, 0xff]),
            "aa:0b:cc:00:1f:ff"
        );
    }

    #[test]
    fn only_ethernet_cards_with_a_real_address_count_on_linux() {
        assert_eq!(
            accept_sysfs("1", "aa:bb:cc:dd:ee:ff\n".trim()),
            Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
        assert_eq!(accept_sysfs("772", "00:00:00:00:00:00"), None, "loopback");
        assert_eq!(accept_sysfs("1", "00:00:00:00:00:00"), None);
        assert_eq!(accept_sysfs("1", "01:00:5e:00:00:01"), None, "multicast");
        assert_eq!(accept_sysfs("1", "not a mac"), None);
    }

    #[test]
    fn getmac_lists_connected_adapters_once_each() {
        let text = "\"A4-BB-6D-12-34-56\",\"\\Device\\Tcpip_{1111}\"\r\n\
                    \"00-FF-AA-BB-CC-DD\",\"Media disconnected\"\r\n\
                    \"B8-27-EB-00-11-22\",\"N/A\"\r\n\
                    \"a4-bb-6d-12-34-56\",\"\\Device\\Tcpip_{2222}\"\r\n\
                    garbage\r\n";
        assert_eq!(
            parse_getmac(text),
            vec![[0xa4, 0xbb, 0x6d, 0x12, 0x34, 0x56]]
        );
    }
}

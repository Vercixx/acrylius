//! What this machine looks like from the network.
//!
//! Wake-on-LAN needs two facts about the computer that will be asleep: a MAC
//! address to put in the packet, and somewhere to aim it. Both are things the
//! machine knows about itself and neither is a secret, so leaving them for a
//! person to fill in by hand is a step that is easy to get wrong and, when it
//! is left blank, fails invisibly — the phone shows no wake button, and nothing
//! anywhere says why.
//!
//! Anything found here is a default. A value in the config always wins.

use std::net::{IpAddr, UdpSocket};
use std::path::Path;

/// This machine's address on the network it routes over.
///
/// Found by asking the kernel which source address it would use to reach a
/// documentation address, which sends nothing and needs no reply. Picking the
/// first non-loopback interface instead gets it wrong on any machine with a
/// VPN, a container bridge, or a second card.
#[must_use]
pub fn routed_ipv4() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    // TEST-NET-1. Routable enough for the kernel to choose an interface, and
    // not somewhere a packet would ever go.
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() => Some(v4.to_string()),
        _ => None,
    }
}

/// Every MAC address worth putting in a wake packet.
///
/// All of them, not the best one. The interface that carries the route is
/// usually Wi-Fi on a laptop, and Wake-on-Wireless is rarely enabled even where
/// it exists; the ethernet card that would actually answer may be sitting there
/// with no route on it at all because nothing is currently plugged in. A phone
/// sends one small datagram per address and whichever card is listening wakes
/// the machine, so choosing between them is a guess with no upside.
///
/// Routed interfaces come first, cheapest metric first, so the most likely one
/// is tried first. Anything without real hardware behind it is skipped: a VPN
/// often holds the default route and is never wakeable.
#[must_use]
pub fn wakeable_macs() -> Vec<String> {
    let sys = Path::new("/sys/class/net");
    let mut order: Vec<String> = default_route_interfaces();
    let mut rest: Vec<String> = std::fs::read_dir(sys)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !order.contains(n))
        .collect();
    rest.sort();
    order.append(&mut rest);

    let mut macs: Vec<String> = Vec::new();
    for iface in order {
        if let Some(mac) = hardware_mac(sys, &iface)
            && !macs.contains(&mac)
        {
            macs.push(mac);
        }
    }
    macs
}

/// Interfaces holding a default route, cheapest metric first.
///
/// `/proc/net/route` rather than `ip route`, which need not be installed and
/// whose output is not a stable interface.
fn default_route_interfaces() -> Vec<String> {
    let Ok(table) = std::fs::read_to_string("/proc/net/route") else {
        return Vec::new();
    };
    let mut found: Vec<(u32, String)> = table
        .lines()
        .skip(1) // A header, not a route.
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let iface = cols.next()?;
            let destination = cols.next()?;
            // A destination of all zeroes is the default route. Hexadecimal,
            // little-endian, which does not matter when comparing to zero.
            if !destination.chars().all(|c| c == '0') {
                return None;
            }
            let metric = cols.nth(4).and_then(|m| m.parse().ok()).unwrap_or(0);
            Some((metric, iface.to_string()))
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, iface)| iface).collect()
}

/// The MAC of an interface, if it has real hardware behind it.
///
/// The `device` link is the test. A bridge, a tunnel, a container veth and a
/// WireGuard interface all report an address of some kind, and none of them is
/// woken by a packet; only something with a driver under it is.
fn hardware_mac(sys: &Path, iface: &str) -> Option<String> {
    if iface == "lo" || !sys.join(iface).join("device").exists() {
        return None;
    }
    let mac = std::fs::read_to_string(sys.join(iface).join("address")).ok()?;
    let mac = mac.trim().to_ascii_lowercase();
    if mac.len() == 17 && mac.matches(':').count() == 5 && mac != "00:00:00:00:00:00" {
        Some(mac)
    } else {
        None
    }
}

/// The broadcast address for a `/24` around an address.
///
/// A guess, and a deliberately conservative one: it is only ever the second
/// place a phone aims, after the unicast address, and iOS cannot send to a
/// broadcast address at all without an entitlement a free account does not get.
#[must_use]
pub fn broadcast_for(ipv4: &str) -> String {
    let mut parts: Vec<&str> = ipv4.split('.').collect();
    if parts.len() == 4 && parts.iter().all(|p| p.parse::<u8>().is_ok()) {
        parts[3] = "255";
        return parts.join(".");
    }
    "255.255.255.255".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_broadcast_address_stays_on_the_same_network() {
        assert_eq!(broadcast_for("192.168.1.50"), "192.168.1.255");
        assert_eq!(broadcast_for("10.0.0.1"), "10.0.0.255");
    }

    #[test]
    fn nonsense_falls_back_to_the_whole_network() {
        assert_eq!(broadcast_for(""), "255.255.255.255");
        assert_eq!(broadcast_for("::1"), "255.255.255.255");
        assert_eq!(broadcast_for("192.168.1.999"), "255.255.255.255");
    }

    #[test]
    fn an_interface_with_nothing_behind_it_is_not_wakeable() {
        let dir = std::env::temp_dir().join(format!("acr-net-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // A tunnel: an address, but no driver under it.
        std::fs::create_dir_all(dir.join("tun0")).unwrap();
        std::fs::write(dir.join("tun0/address"), "aa:bb:cc:dd:ee:ff\n").unwrap();
        assert_eq!(hardware_mac(&dir, "tun0"), None, "no device link");

        // A real card.
        std::fs::create_dir_all(dir.join("eth0/device")).unwrap();
        std::fs::write(dir.join("eth0/address"), "AA:BB:CC:DD:EE:FF\n").unwrap();
        assert_eq!(
            hardware_mac(&dir, "eth0"),
            Some("aa:bb:cc:dd:ee:ff".to_string())
        );

        // Hardware that reports no address is not a target either.
        std::fs::create_dir_all(dir.join("eth1/device")).unwrap();
        std::fs::write(dir.join("eth1/address"), "00:00:00:00:00:00\n").unwrap();
        assert_eq!(hardware_mac(&dir, "eth1"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn this_machine_can_describe_itself() {
        // Not asserting a value: a build machine may have no route and no card
        // at all, and a test that demands one fails in CI for the wrong reason.
        // The shape is what matters, because a malformed MAC in a packet is
        // worse than no packet.
        for mac in wakeable_macs() {
            assert_eq!(mac.len(), 17, "aa:bb:cc:dd:ee:ff");
            assert_eq!(mac.matches(':').count(), 5);
        }
        if let Some(ip) = routed_ipv4() {
            assert!(ip.parse::<std::net::Ipv4Addr>().is_ok());
        }
    }
}

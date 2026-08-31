//! What this machine looks like from the network, for Wake-on-LAN: a MAC
//! address and somewhere to aim it. Anything found here is a default; a config value always wins.

use std::net::{IpAddr, UdpSocket};
use std::path::Path;

/// This machine's address on the network it routes over.
///
/// Asks the kernel which source address it'd use to reach a documentation
/// address (sends nothing); picking the first non-loopback interface instead
/// breaks on a VPN, bridge, or second NIC.
#[must_use]
pub fn routed_ipv4() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    // TEST-NET-1: routable enough to pick an interface, never actually reachable.
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() => Some(v4.to_string()),
        _ => None,
    }
}

/// Every MAC address worth putting in a wake packet — all of them, not just
/// the routed one, since the routed interface (often Wi-Fi) may not be the
/// card that's actually plugged in and listening for WoL.
///
/// Routed interfaces come first, cheapest metric first; interfaces with no
/// real hardware behind them (a VPN, say) are skipped.
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
            // All-zero destination = default route (hex, little-endian, irrelevant when comparing to zero).
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
/// Tested via the `device` link: a bridge, tunnel, veth, or WireGuard
/// interface all report an address but none is woken by a packet.
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
/// A conservative guess: it's only the second place a phone aims, after
/// unicast, since iOS can't send broadcast without an entitlement free accounts lack.
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
        // Not asserting a value: CI may have no route/card. Only the shape matters.
        for mac in wakeable_macs() {
            assert_eq!(mac.len(), 17, "aa:bb:cc:dd:ee:ff");
            assert_eq!(mac.matches(':').count(), 5);
        }
        if let Some(ip) = routed_ipv4() {
            assert!(ip.parse::<std::net::Ipv4Addr>().is_ok());
        }
    }
}

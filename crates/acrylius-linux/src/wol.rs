//! Magic packets.
//!
//! A magic packet is `ff` six times followed by the target MAC repeated sixteen
//! times: 102 bytes, or 108 with a six-byte SecureOn password. A network
//! interface matches it by that payload and pays no attention to the destination
//! address, so a unicast datagram to the sleeping machine's last known address
//! wakes it exactly as well as a broadcast.
//!
//! That detail carries real weight here, because iOS gates UDP broadcast behind
//! an entitlement a free developer account cannot get. Unicast-first is not a
//! fallback, it is the primary path, and it works as long as the router still
//! holds an ARP entry for the sleeping machine.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};

/// Parse a MAC written with colons, dashes, or nothing at all.
pub fn parse_mac(mac: &str) -> anyhow::Result<[u8; 6]> {
    let hex: Vec<u8> = mac
        .chars()
        .filter(char::is_ascii_hexdigit)
        .map(|c| c.to_digit(16).unwrap_or(0) as u8)
        .collect();
    if hex.len() != 12 {
        anyhow::bail!("{mac:?} is not a MAC address");
    }
    let mut out = [0u8; 6];
    for (i, pair) in hex.as_chunks::<2>().0.iter().enumerate() {
        out[i] = (pair[0] << 4) | pair[1];
    }
    Ok(out)
}

/// Build the packet. `secure_on` is the optional six-byte password some network
/// interfaces require before they will act on one.
#[must_use]
pub fn build(mac: [u8; 6], secure_on: Option<[u8; 6]>) -> Vec<u8> {
    let mut packet = Vec::with_capacity(108);
    packet.extend_from_slice(&[0xFF; 6]);
    for _ in 0..16 {
        packet.extend_from_slice(&mac);
    }
    if let Some(password) = secure_on {
        packet.extend_from_slice(&password);
    }
    packet
}

/// Send one packet to every destination given, in order.
///
/// Every destination is tried even when an earlier one succeeded. A send that
/// returns `Ok` only means the datagram left; whether the machine was listening
/// is not knowable from here, which is why the caller confirms by polling for
/// the machine to come back rather than by trusting this.
pub async fn send(macs: &[String], destinations: &[String], port: u16) -> anyhow::Result<usize> {
    let mut packets = Vec::new();
    for m in macs {
        packets.push(build(parse_mac(m)?, None));
    }
    let destinations = destinations.to_vec();

    tokio::task::spawn_blocking(move || {
        let socket = UdpSocket::bind(("0.0.0.0", 0))?;
        socket.set_broadcast(true)?;
        let mut sent = 0;
        for dest in &destinations {
            let addrs: Vec<SocketAddr> = match (dest.as_str(), port).to_socket_addrs() {
                Ok(a) => a.collect(),
                Err(e) => {
                    tracing::debug!(dest, error = %e, "could not resolve wake target");
                    continue;
                }
            };
            for addr in addrs {
                for packet in &packets {
                    match socket.send_to(packet, addr) {
                        Ok(_) => sent += 1,
                        Err(e) => tracing::debug!(%addr, error = %e, "wake send failed"),
                    }
                }
            }
        }
        if sent == 0 {
            anyhow::bail!("no wake packet could be sent");
        }
        Ok(sent)
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];

    #[test]
    fn a_mac_can_be_written_three_ways() {
        for spelling in ["00:11:22:33:44:55", "00-11-22-33-44-55", "001122334455"] {
            assert_eq!(parse_mac(spelling).unwrap(), MAC, "{spelling} should parse");
        }
    }

    #[test]
    fn upper_and_lower_case_agree() {
        assert_eq!(
            parse_mac("AA:BB:CC:DD:EE:FF").unwrap(),
            parse_mac("aa:bb:cc:dd:ee:ff").unwrap()
        );
    }

    #[test]
    fn anything_that_is_not_a_mac_is_refused() {
        for junk in [
            "",
            "00:11:22:33:44",
            "00:11:22:33:44:55:66",
            "zz:zz:zz:zz:zz:zz",
        ] {
            assert!(parse_mac(junk).is_err(), "{junk:?} should not parse");
        }
    }

    #[test]
    fn the_packet_is_the_shape_the_hardware_looks_for() {
        let p = build(MAC, None);
        assert_eq!(p.len(), 102);
        assert_eq!(&p[..6], &[0xFF; 6]);
        // Sixteen repetitions, checked at both ends rather than just the first.
        assert_eq!(&p[6..12], &MAC);
        assert_eq!(&p[96..102], &MAC);
        for chunk in p[6..].as_chunks::<6>().0 {
            assert_eq!(chunk, &MAC);
        }
    }

    #[test]
    fn a_secureon_password_adds_six_bytes_at_the_end() {
        let password = [1, 2, 3, 4, 5, 6];
        let p = build(MAC, Some(password));
        assert_eq!(p.len(), 108);
        assert_eq!(&p[102..], &password);
    }
}

//! Key derivations off a completed pairing handshake. Pairing runs plain `XX`;
//! both ends derive the same six digits from the handshake hash ([`sas`]) and a
//! person confirms they match.

use alloc::string::String;
use hkdf::Hkdf;
use sha2::Sha256;

const SAS_INFO: &[u8] = b"acrylius/pair/v1/sas";
const SESSION_PSK_INFO: &[u8] = b"acrylius/pair/v1/session-psk";

/// Digits in the short authentication string shown on both screens.
pub const SAS_DIGITS: u32 = 6;

/// The short authentication string, from the completed handshake hash.
///
/// This is the security mechanism: a relay attack yields two handshake hashes,
/// so the digits differ, and the person comparing them authenticates the
/// pairing. A UI must ask for the comparison, not merely display the digits.
#[must_use]
pub fn sas(handshake_hash: &[u8]) -> String {
    let mut out = [0u8; 4];
    Hkdf::<Sha256>::new(None, handshake_hash)
        .expand(SAS_INFO, &mut out)
        .expect("4 bytes is a valid HKDF-SHA256 length");
    let modulus = 10u32.pow(SAS_DIGITS);
    let value = u32::from_be_bytes(out) % modulus;
    let digits = SAS_DIGITS as usize;
    let mut s = alloc::format!("{value:0digits$}");
    // Grouped for reading aloud: "123 456".
    s.insert(digits / 2, ' ');
    s
}

/// The long-lived session PSK for `IKpsk2`, derived once at pairing time and
/// stored by both sides. Never transmitted.
#[must_use]
pub fn session_psk(pairing_handshake_hash: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(None, pairing_handshake_hash)
        .expand(SESSION_PSK_INFO, &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 length");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sas_is_six_digits_grouped() {
        let s = sas(b"any handshake hash at all");
        assert_eq!(
            s.len(),
            SAS_DIGITS as usize + 1,
            "six digits plus one space"
        );
        assert_eq!(s.chars().nth(3), Some(' '));
        assert!(s.chars().filter(|c| *c != ' ').all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn sas_keeps_leading_zeros() {
        // If the formatter dropped leading zeros a 4-digit SAS would silently ship.
        for i in 0..2000u32 {
            let s = sas(&i.to_be_bytes());
            assert_eq!(s.chars().filter(|c| *c != ' ').count(), SAS_DIGITS as usize);
        }
    }

    #[test]
    fn sas_differs_for_different_handshakes() {
        assert_ne!(sas(b"handshake A"), sas(b"handshake B"));
    }

    #[test]
    fn one_flipped_bit_anywhere_in_the_hash_changes_the_digits() {
        // A derivation reading only part of the hash would pass the other tests
        // while staying blind to flips in the bytes it skipped.
        let base = [0u8; 32];
        let want = sas(&base);
        let mut same = 0;
        for byte in 0..base.len() {
            for bit in 0..8 {
                let mut h = base;
                h[byte] ^= 1 << bit;
                if sas(&h) == want {
                    same += 1;
                }
            }
        }
        // Collisions are possible in a 10^6 space; ignoring whole bytes would show dozens.
        assert!(
            same <= 1,
            "{same} of 256 single-bit flips left the SAS alone"
        );
    }

    #[test]
    fn session_psk_is_domain_separated_from_the_sas() {
        // Without the info strings the session key would be recoverable from the SAS.
        let hh = b"the same handshake hash";
        let sk = session_psk(hh);
        let mut sas_raw = [0u8; 4];
        Hkdf::<Sha256>::new(None, hh)
            .expand(SAS_INFO, &mut sas_raw)
            .unwrap();
        assert_ne!(&sk[..4], &sas_raw[..]);
    }
}

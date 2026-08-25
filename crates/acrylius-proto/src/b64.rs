//! Strict base64url, no padding.
//!
//! Carried over from `pc-helper-ios` deliberately. A lenient decoder lets the
//! same key or fingerprint be spelled several ways, which turns an identifier
//! into a set rather than a value, and identifiers get compared, indexed and used
//! as map keys. So this rejects padding, non-alphabet bytes, impossible lengths,
//! and non-canonical trailing bits.

use alloc::string::String;
use alloc::vec::Vec;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Reverse lookup: byte -> 6-bit value, or 0xFF when not in the alphabet.
static DECODE: [u8; 256] = {
    let mut t = [0xFFu8; 256];
    let mut i = 0;
    while i < 64 {
        t[ALPHABET[i] as usize] = i as u8;
        i += 1;
    }
    t
};

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum B64Error {
    #[error("base64url input has an impossible length ({0} bytes)")]
    BadLength(usize),
    #[error("base64url input contains a byte outside the alphabet at index {0}")]
    BadByte(usize),
    #[error("base64url input has non-canonical trailing bits")]
    NonCanonical,
    #[error("expected {expected} decoded bytes, got {actual}")]
    WrongSize { expected: usize, actual: usize },
}

pub fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = |i: usize| *chunk.get(i).unwrap_or(&0) as u32;
        let n = (b(0) << 16) | (b(1) << 8) | b(2);
        // 3 input bytes -> 4 chars; 2 -> 3; 1 -> 2. Never any padding.
        let chars = chunk.len() + 1;
        for i in 0..chars {
            let sextet = (n >> (18 - 6 * i)) & 0x3F;
            out.push(ALPHABET[sextet as usize] as char);
        }
    }
    out
}

pub fn decode(input: &str) -> Result<Vec<u8>, B64Error> {
    let bytes = input.as_bytes();
    // A length of 1 mod 4 can never be produced by the encoder.
    if bytes.len() % 4 == 1 {
        return Err(B64Error::BadLength(bytes.len()));
    }

    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (ci, chunk) in bytes.chunks(4).enumerate() {
        let mut n: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            let v = DECODE[c as usize];
            if v == 0xFF {
                return Err(B64Error::BadByte(ci * 4 + i));
            }
            n |= (v as u32) << (18 - 6 * i);
        }
        // A partial chunk of n chars carries n-1 whole bytes; the bits below
        // those must be zero or the same value has two spellings.
        let whole = chunk.len() - 1;
        for i in 0..whole {
            out.push(((n >> (16 - 8 * i)) & 0xFF) as u8);
        }
        if chunk.len() < 4 {
            let used_bits = whole * 8;
            let leftover = n & ((1 << (24 - used_bits)) - 1);
            if leftover != 0 {
                return Err(B64Error::NonCanonical);
            }
        }
    }
    Ok(out)
}

/// Decode and require an exact length, which is what identifiers actually want.
pub fn decode_exact<const N: usize>(input: &str) -> Result<[u8; N], B64Error> {
    let v = decode(input)?;
    <[u8; N]>::try_from(v.as_slice()).map_err(|_| B64Error::WrongSize {
        expected: N,
        actual: v.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn round_trips_every_length_up_to_64() {
        for len in 0..=64usize {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 + 13) as u8).collect();
            let enc = encode(&data);
            assert!(!enc.contains('='), "encoder emitted padding at len {len}");
            assert_eq!(
                decode(&enc).unwrap(),
                data,
                "round trip failed at len {len}"
            );
        }
    }

    #[test]
    fn rejects_padding() {
        // "AA==" is how a padded encoder spells one zero byte.
        assert_eq!(decode("AA=="), Err(B64Error::BadByte(2)));
    }

    #[test]
    fn rejects_standard_base64_alphabet() {
        // '+' and '/' are base64, not base64url. They must not decode.
        assert_eq!(decode("ab+d"), Err(B64Error::BadByte(2)));
        assert_eq!(decode("ab/d"), Err(B64Error::BadByte(2)));
    }

    #[test]
    fn rejects_impossible_length() {
        assert_eq!(decode("A"), Err(B64Error::BadLength(1)));
        assert_eq!(decode("AAAAA"), Err(B64Error::BadLength(5)));
    }

    #[test]
    fn rejects_non_canonical_trailing_bits() {
        // One byte 0x00 encodes as "AA". "AB" would decode to the same byte
        // while carrying a stray low bit, so it is a second spelling.
        assert_eq!(decode("AA").unwrap(), vec![0x00]);
        assert_eq!(decode("AB"), Err(B64Error::NonCanonical));
        // Two bytes: "AAA" is canonical, "AAB" is not.
        assert_eq!(decode("AAA").unwrap(), vec![0x00, 0x00]);
        assert_eq!(decode("AAB"), Err(B64Error::NonCanonical));
    }

    #[test]
    fn decode_exact_enforces_size() {
        let enc = encode(&[1u8; 32]);
        assert!(decode_exact::<32>(&enc).is_ok());
        assert_eq!(
            decode_exact::<16>(&enc),
            Err(B64Error::WrongSize {
                expected: 16,
                actual: 32
            })
        );
    }

    #[test]
    fn known_vectors() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg");
        assert_eq!(encode(b"fo"), "Zm8");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg");
        assert_eq!(encode(b"fooba"), "Zm9vYmE");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        // The two bytes that differ between base64 and base64url.
        assert_eq!(encode(&[0xFF, 0xEF, 0xBF]), "_--_");
    }
}

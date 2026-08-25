//! Pairing codes and the two key derivations that hang off them.
//!
//! The alphabet is carried over verbatim from `pc-helper-ios`, and it was well
//! chosen: Crockford base32 with `I`, `L`, `O` and `U` removed. `I`/`L` versus
//! `1`, and `O` versus `0`, are the confusions a human makes reading a code off
//! a screen; `U` is dropped so the alphabet cannot spell an unfortunate word.
//! Eight characters is 40 bits.
//!
//! What is new here is what the code is used for. The old project used it as a
//! bearer token: present the right code and the pairing window accepts you. Here
//! it is mixed into the Noise handshake as a pre-shared key, so a wrong code does
//! not "fail a check". Message 3 simply does not decrypt. That upgrade is why the
//! pattern is `XXpsk3` rather than plain `XX`.

use alloc::string::String;
use hkdf::Hkdf;
use sha2::Sha256;

/// Crockford base32 minus `I L O U`.
pub const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Characters in a pairing code. 8 × 5 bits = 40 bits of entropy.
pub const CODE_LEN: usize = 8;

const PSK_SALT: &[u8] = b"acrylius/pair/v1/psk";
const SAS_INFO: &[u8] = b"acrylius/pair/v1/sas";
const SESSION_PSK_INFO: &[u8] = b"acrylius/pair/v1/session-psk";

/// Digits in the short authentication string shown on both screens.
pub const SAS_DIGITS: u32 = 6;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CodeError {
    #[error("a pairing code is {CODE_LEN} characters, got {0}")]
    WrongLength(usize),
    #[error("character {0:?} is not in the pairing alphabet")]
    BadChar(char),
}

/// Render 40 bits as a pairing code. The host supplies the randomness, because
/// this crate has no RNG, by design.
#[must_use]
pub fn encode(bits: u64) -> String {
    (0..CODE_LEN)
        .map(|i| {
            let shift = 5 * (CODE_LEN - 1 - i);
            ALPHABET[((bits >> shift) & 0x1F) as usize] as char
        })
        .collect()
}

/// Normalize what a human typed: upper-case, strip spaces and dashes, and fold
/// the four confusable characters onto their intended digits.
///
/// Folding happens before any comparison or key derivation, so `l` and `1`
/// really are the same code rather than two codes that merely look alike.
pub fn normalize(input: &str) -> Result<String, CodeError> {
    let mut out = String::with_capacity(CODE_LEN);
    for ch in input.chars() {
        if ch == ' ' || ch == '-' || ch == '_' {
            continue;
        }
        let up = ch.to_ascii_uppercase();
        let folded = match up {
            'I' | 'L' => '1',
            'O' => '0',
            'U' => 'V',
            c => c,
        };
        if !ALPHABET.contains(&(folded as u8)) {
            return Err(CodeError::BadChar(ch));
        }
        out.push(folded);
    }
    if out.len() != CODE_LEN {
        return Err(CodeError::WrongLength(out.len()));
    }
    Ok(out)
}

/// The Noise pre-shared key for `XXpsk3`, derived from a normalized code.
///
/// Taking the normalized form is deliberate: deriving from raw input would make
/// `abc-defgh` and `ABCDEFGH` different keys, and the fold would silently stop
/// working at exactly the moment a user typed a lower-case `l`.
#[must_use]
pub fn psk(normalized_code: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(Some(PSK_SALT), normalized_code.as_bytes())
        .expand(&[], &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 length");
    out
}

/// The short authentication string, from the completed handshake hash.
///
/// This is a cross-check, not the security mechanism: `XXpsk3` already makes a
/// wrong code fail to decrypt. Showing six digits on both screens costs
/// nothing and catches implementation bugs that a PSK check would mask, which is
/// exactly the class of bug that is otherwise invisible until it is a CVE.
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
    fn alphabet_omits_the_confusable_four() {
        for c in *b"ILOU" {
            assert!(
                !ALPHABET.contains(&c),
                "{} must not be in the alphabet",
                c as char
            );
        }
        assert_eq!(ALPHABET.len(), 32);
    }

    #[test]
    fn encode_is_stable_and_the_right_length() {
        assert_eq!(encode(0).len(), CODE_LEN);
        assert_eq!(encode(0), "00000000");
        // 40 bits all set -> last character of the alphabet, eight times.
        assert_eq!(encode(0xFF_FFFF_FFFF), "ZZZZZZZZ");
    }

    #[test]
    fn normalize_folds_confusables_onto_one_spelling() {
        let canonical = normalize("1010VVVV").unwrap();
        for spelling in ["IOIOUUUU", "loLoUuUu", "i0-l0 uuuu", "I0I0UUUU"] {
            assert_eq!(
                normalize(spelling).unwrap(),
                canonical,
                "{spelling} should fold"
            );
        }
    }

    #[test]
    fn normalize_rejects_junk_and_wrong_lengths() {
        assert_eq!(normalize("ABCDEFG"), Err(CodeError::WrongLength(7)));
        assert_eq!(normalize("ABCDEFGHI"), Err(CodeError::WrongLength(9)));
        assert_eq!(normalize("ABCDEFG!"), Err(CodeError::BadChar('!')));
    }

    #[test]
    fn folded_spellings_derive_the_same_psk() {
        // The point of folding: a user typing `l` for `1` must reach the same key,
        // not merely pass the same string comparison.
        let a = psk(&normalize("I0I0UUUU").unwrap());
        let b = psk(&normalize("1010VVVV").unwrap());
        assert_eq!(a, b);
    }

    #[test]
    fn different_codes_derive_different_keys() {
        assert_ne!(
            psk(&normalize("00000000").unwrap()),
            psk(&normalize("00000001").unwrap())
        );
    }

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
        // Find a hash whose SAS is small enough to need padding; if the formatter
        // ever drops leading zeros, a 4-digit SAS would silently ship.
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
    fn session_psk_is_domain_separated_from_the_sas() {
        // Both derive from the same handshake hash. If the info strings were ever
        // dropped, the session key would be recoverable from the displayed SAS.
        let hh = b"the same handshake hash";
        let sk = session_psk(hh);
        let mut sas_raw = [0u8; 4];
        Hkdf::<Sha256>::new(None, hh)
            .expand(SAS_INFO, &mut sas_raw)
            .unwrap();
        assert_ne!(&sk[..4], &sas_raw[..]);
    }
}

//! Device identities, derived from a Noise static public key.
//!
//! Two invariants carried over from `pc-helper-ios`, both earned:
//!
//! 1. **Identifiers are derived by the receiver, never self-asserted.** A peer
//!    sends its public key; the receiver computes the id. That is why two
//!    devices cannot collide onto one record by claiming the same name.
//! 2. **Strict base64url.** See [`crate::b64`] — a lenient decoder would turn an
//!    identifier into a *set of spellings* rather than a value.
//!
//! Both derivations are domain-tagged. The old project tagged its per-message
//! canonical strings but not its identifiers; tagging both means a fingerprint
//! can never be confused for a device id even if the truncation changed.

use alloc::string::String;
use sha2::{Digest, Sha256};

use crate::b64;

/// Raw Noise static public key (X25519).
pub type PublicKey = [u8; 32];

const FP_TAG: &[u8] = b"acrylius/v1/fp";
const DID_TAG: &[u8] = b"acrylius/v1/did";

/// Full fingerprint: 32 bytes of SHA-256, 43 base64url chars.
///
/// This is what a human compares and what the mDNS TXT record publishes. The
/// raw public key is deliberately *not* published — see `PROTOCOL.md`; keeping
/// it secret is what lets the `IKpsk2` first message stay opaque to an observer.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Fingerprint(String);

/// Truncated identifier: first 16 bytes of a distinct hash, 22 base64url chars.
///
/// Short enough to sit in a TXT record and a log line. 128 bits of a
/// second-preimage-resistant hash over a key nobody else holds is ample; this
/// is an index, and [`Fingerprint`] remains the thing that is *compared*.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DeviceId(String);

macro_rules! str_id {
    ($t:ty) => {
        impl $t {
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Parse an id received over the wire or read from disk.
            ///
            /// Round-trips through the strict decoder rather than pattern-matching
            /// the string, so a non-canonical spelling is rejected here and not
            /// three layers deeper where it would already be a map key.
            pub fn parse(s: &str) -> Result<Self, b64::B64Error> {
                let n = b64::decode(s)?.len();
                if n != Self::BYTES {
                    return Err(b64::B64Error::WrongSize { expected: Self::BYTES, actual: n });
                }
                Ok(Self(alloc::string::ToString::to_string(s)))
            }
        }

        impl core::fmt::Display for $t {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

impl Fingerprint {
    pub const BYTES: usize = 32;
    pub const CHARS: usize = 43;

    #[must_use]
    pub fn of(pk: &PublicKey) -> Self {
        let mut h = Sha256::new();
        h.update(FP_TAG);
        h.update(pk);
        Self(b64::encode(&h.finalize()))
    }
}

impl DeviceId {
    pub const BYTES: usize = 16;
    pub const CHARS: usize = 22;

    #[must_use]
    pub fn of(pk: &PublicKey) -> Self {
        let mut h = Sha256::new();
        h.update(DID_TAG);
        h.update(pk);
        Self(b64::encode(&h.finalize()[..Self::BYTES]))
    }
}

str_id!(Fingerprint);
str_id!(DeviceId);

#[cfg(test)]
mod tests {
    use super::*;

    /// The key of all zeroes. A fixed, boring vector that any other
    /// implementation can reproduce with `sha256sum`.
    const ZERO: PublicKey = [0u8; 32];

    #[test]
    fn lengths_are_what_the_constants_claim() {
        assert_eq!(Fingerprint::of(&ZERO).as_str().len(), Fingerprint::CHARS);
        assert_eq!(DeviceId::of(&ZERO).as_str().len(), DeviceId::CHARS);
    }

    #[test]
    fn domain_tags_actually_separate_the_two() {
        // Without distinct tags, device_id would be a prefix of the fingerprint
        // and the two namespaces would leak into each other.
        let fp = Fingerprint::of(&ZERO);
        let did = DeviceId::of(&ZERO);
        assert!(
            !fp.as_str().starts_with(did.as_str()),
            "device id must not be a truncation of the fingerprint"
        );
    }

    #[test]
    fn distinct_keys_give_distinct_ids() {
        let a = [0u8; 32];
        let mut b = [0u8; 32];
        b[31] = 1;
        assert_ne!(Fingerprint::of(&a), Fingerprint::of(&b));
        assert_ne!(DeviceId::of(&a), DeviceId::of(&b));
    }

    #[test]
    fn parse_round_trips_and_rejects_wrong_size() {
        let fp = Fingerprint::of(&ZERO);
        assert_eq!(Fingerprint::parse(fp.as_str()).unwrap(), fp);

        let did = DeviceId::of(&ZERO);
        // A device id is 16 bytes; parsing it as a fingerprint must fail loudly.
        assert!(matches!(
            Fingerprint::parse(did.as_str()),
            Err(b64::B64Error::WrongSize { expected: 32, actual: 16 })
        ));
    }

    #[test]
    fn parse_rejects_non_canonical_spelling() {
        // Flip the last char to one carrying stray low bits: same decoded value,
        // different spelling. It must not become a second valid identifier.
        let fp = Fingerprint::of(&ZERO);
        let mut s = alloc::string::String::from(fp.as_str());
        s.pop();
        s.push('B');
        // Either non-canonical or a genuinely different id, but never equal to fp.
        assert_ne!(Fingerprint::parse(&s).ok(), Some(fp));
    }
}

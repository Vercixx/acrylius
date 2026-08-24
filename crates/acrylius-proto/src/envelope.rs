//! The packet envelope.
//!
//! Two properties are load-bearing and both were corrections during design review.
//!
//! **Explicit numeric field indices.** `minicbor`'s derive keys fields by number,
//! not by name, so adding a field is "use index 8" and old readers skip it. A
//! name-keyed encoding gives a much fuzzier evolution story for something that
//! has to stay compatible across an app the user updates on a 7-day cycle and a
//! daemon they update whenever.
//!
//! **`body` is an opaque byte string.** It is *not* an inline CBOR map. If it
//! were, the core would have to be able to parse plugin schemas in order to route
//! a packet — which contradicts the entire plugin design. Opaque bodies mean the
//! core routes, queues and forwards without understanding anything, plugins may
//! use whatever encoding they like, and handing an envelope to an out-of-process
//! plugin later is a memcpy. This is the same layering COSE uses. It costs a few
//! bytes of double encoding, which is the right trade.

use alloc::vec::Vec;

/// A capability identifier carries its own major version:
/// `org.acrylius.clipboard/1`.
///
/// That makes negotiation a plain string-set intersection with no separate
/// version field to get wrong, and it makes a breaking change simply *a different
/// capability* — a peer that speaks both advertises both.
pub type Cap<'a> = &'a str;

#[derive(Clone, PartialEq, Eq, Debug, minicbor::Encode, minicbor::Decode)]
pub struct Envelope<'a> {
    /// Wire version. See [`crate::WIRE_VERSION`].
    #[n(0)]
    pub v: u8,
    /// Sender-assigned, unique within a session. Used to correlate `re`.
    #[n(1)]
    pub id: u32,
    /// Set on a reply, to the `id` of the message being answered.
    #[n(2)]
    pub re: Option<u32>,
    /// e.g. `"org.acrylius.session/1"`.
    #[b(3)]
    pub cap: &'a str,
    /// Short verb within the capability: `"lock"`, `"state"`, `"ok"`, `"err"`.
    #[b(4)]
    pub ty: &'a str,
    /// Opaque to the core. The plugin owning `cap` decodes it.
    #[cbor(b(5), with = "minicbor::bytes")]
    pub body: &'a [u8],
    /// Reserved. No flags are defined at v1; present so adding one is not a
    /// field addition on a hot path.
    #[n(6)]
    pub flags: u8,
    /// Bulk transfer this envelope refers to, if any. Bulk bytes never travel
    /// through the envelope — see `PROTOCOL.md`.
    #[n(7)]
    pub bulk: Option<u64>,
}

impl<'a> Envelope<'a> {
    /// A plain message with no reply correlation.
    #[must_use]
    pub fn new(id: u32, cap: &'a str, ty: &'a str, body: &'a [u8]) -> Self {
        Self {
            v: crate::WIRE_VERSION,
            id,
            re: None,
            cap,
            ty,
            body,
            flags: 0,
            bulk: None,
        }
    }

    /// A reply to `to`, reusing its capability.
    #[must_use]
    pub fn reply_to(id: u32, to: &Envelope<'a>, ty: &'a str, body: &'a [u8]) -> Self {
        Self {
            re: Some(to.id),
            ..Self::new(id, to.cap, ty, body)
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, minicbor::encode::Error<core::convert::Infallible>> {
        minicbor::to_vec(self)
    }

    pub fn decode(bytes: &'a [u8]) -> Result<Self, minicbor::decode::Error> {
        minicbor::decode(bytes)
    }
}

/// The fixed error vocabulary.
///
/// Carried over as a *discipline* from the old project, where a closed set of
/// error codes was what made the client's user-facing copy possible: a client can
/// only say something useful about a failure it can name. Adding a variant is a
/// deliberate act, not a new string literal at a call site.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ErrorCode {
    /// The capability was not in the negotiated intersection for this direction.
    CapNotNegotiated,
    /// Well-formed envelope, but this `ty` is unknown within the capability.
    UnknownType,
    /// The body failed to decode against the plugin's schema.
    BadBody,
    /// Refused by policy: an id absent from an allowlist, a disabled direction.
    NotAllowed,
    /// The host could not carry out the effect (no session, no compositor, ...).
    EffectFailed,
    /// The peer is known but the operation needs a pairing that is not complete.
    NotPaired,
    /// Body or payload exceeded a configured cap.
    TooLarge,
    /// The operation did not confirm within its window. Distinct from
    /// `EffectFailed`: it may yet have succeeded, and the caller should re-read.
    Timeout,
    /// Internal fault. Always paired with a log line carrying the detail.
    Internal,
}

impl ErrorCode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CapNotNegotiated => "cap_not_negotiated",
            Self::UnknownType => "unknown_type",
            Self::BadBody => "bad_body",
            Self::NotAllowed => "not_allowed",
            Self::EffectFailed => "effect_failed",
            Self::NotPaired => "not_paired",
            Self::TooLarge => "too_large",
            Self::Timeout => "timeout",
            Self::Internal => "internal",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let body = b"\x00\xff\x01opaque";
        let e = Envelope::new(7, "org.acrylius.session/1", "lock", body);
        let bytes = e.encode().unwrap();
        assert_eq!(Envelope::decode(&bytes).unwrap(), e);
    }

    #[test]
    fn body_is_opaque_bytes_not_parsed_cbor() {
        // A body that is NOT valid CBOR must survive a round trip untouched.
        // If this ever fails, someone has made the core parse plugin schemas.
        let garbage: &[u8] = &[0xff, 0xfe, 0xfd, 0x00, 0x1a];
        let e = Envelope::new(1, "x/1", "y", garbage);
        let back = e.encode().unwrap();
        assert_eq!(Envelope::decode(&back).unwrap().body, garbage);
    }

    #[test]
    fn reply_correlates_and_inherits_cap() {
        let req = Envelope::new(42, "org.acrylius.session/1", "lock", b"");
        let rep = Envelope::reply_to(43, &req, "result", b"");
        assert_eq!(rep.re, Some(42));
        assert_eq!(rep.cap, req.cap, "a reply must stay within its capability");
    }

    #[test]
    fn unknown_trailing_fields_are_skipped_by_an_old_reader() {
        // Simulates a future peer that added field 8. Decoding must succeed:
        // this is the whole reason for numeric indices.
        #[derive(minicbor::Encode)]
        struct Future<'a> {
            #[n(0)]
            v: u8,
            #[n(1)]
            id: u32,
            #[n(2)]
            re: Option<u32>,
            #[b(3)]
            cap: &'a str,
            #[b(4)]
            ty: &'a str,
            #[cbor(b(5), with = "minicbor::bytes")]
            body: &'a [u8],
            #[n(6)]
            flags: u8,
            #[n(7)]
            bulk: Option<u64>,
            #[n(8)]
            invented_later: u64,
        }
        let f = Future {
            v: 1,
            id: 5,
            re: None,
            cap: "c/1",
            ty: "t",
            body: b"b",
            flags: 0,
            bulk: None,
            invented_later: 99,
        };
        let bytes = minicbor::to_vec(&f).unwrap();
        let e = Envelope::decode(&bytes).expect("old reader must skip unknown fields");
        assert_eq!((e.id, e.cap, e.body), (5, "c/1", &b"b"[..]));
    }

    #[test]
    fn error_codes_are_unique_strings() {
        use super::ErrorCode::*;
        let all = [
            CapNotNegotiated,
            UnknownType,
            BadBody,
            NotAllowed,
            EffectFailed,
            NotPaired,
            TooLarge,
            Timeout,
            Internal,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.as_str(), b.as_str());
            }
        }
    }
}

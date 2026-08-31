//! The packet envelope. Fields are keyed by explicit number so old readers skip
//! additions; `body` is an opaque byte string so the core routes without
//! parsing plugin schemas.

use alloc::vec::Vec;

/// A capability id carries its own major version (`org.acrylius.clipboard/1`),
/// so negotiation is a string-set intersection and a breaking change is a new capability.
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
    /// Reserved. No flags at v1; present so adding one is not a field addition.
    #[n(6)]
    pub flags: u8,
    /// Bulk transfer this refers to, if any; bulk bytes never travel in the envelope.
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

/// The body of an `err` message: `code` is acted on, `message` is for a human.
#[derive(Clone, PartialEq, Eq, Debug, minicbor::Encode, minicbor::Decode)]
pub struct ErrorBody {
    #[n(0)]
    pub code: alloc::string::String,
    #[n(1)]
    pub message: alloc::string::String,
}

/// The fixed error vocabulary; adding a variant is a deliberate act, not a new
/// string literal at a call site.
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
        // A body that is not valid CBOR must survive a round trip untouched.
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
        // A future peer that added field 8; decoding must skip it.
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

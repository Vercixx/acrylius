//! The acrylius wire format. There is one implementation, and this is it.
//!
//! This crate is deliberately tiny and deliberately dependency-poor: no `snow`,
//! no state machine, no IO, no async. It holds the envelope, the identity
//! derivations, the handshake payloads and the golden test vectors, and nothing
//! else. Two consequences follow:
//!
//! * `acryliusctl trace` can decode a packet without linking the crypto;
//! * a third-party plugin author depends on *this*, not on the whole core.
//!
//! It is `no_std + alloc`. That is not vanity: it is the honest check that the
//! format has not quietly grown a coupling to tokio.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod b64;
pub mod envelope;
pub mod frame;
pub mod handshake;
pub mod ids;
pub mod pairing;

/// Wire format version. Bumped only for a change no `Envelope` field index can express.
pub const WIRE_VERSION: u8 = 1;

/// mDNS service type. Not 1716: that is KDE Connect's, and we deliberately do
/// not interoperate, so sharing a port would only produce confusing failures.
pub const SERVICE_TYPE: &str = "_acrylius._tcp";
pub const DEFAULT_PORT: u16 = 1971;

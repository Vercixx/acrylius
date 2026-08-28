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
pub mod ble;
pub mod bulk;
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

/// The BLE GATT service, and the three characteristics under it.
///
/// Here rather than in a transport because both ends must agree on them exactly:
/// the daemon publishes them and the phone scans for them, and a disagreement of
/// one hex digit is a phone that never finds a desktop with no error anywhere to
/// say so. The first eight bytes spell `acrylius` in ASCII, which makes them
/// findable in a `btmon` trace.
///
/// These are load-bearing in a way an mDNS service type is not: the layout must
/// stay **identical across releases**. iOS caches a peer's attribute table, so a
/// database that changes shape strands every phone that has already seen the old
/// one. Adding a capability must not mean adding a characteristic.
pub const BLE_SERVICE_UUID: &str = "61637279-6c69-7573-8001-000000000001";
/// Read to learn who this device is: the same facts the mDNS TXT record carries.
/// Read after connecting rather than advertised, because a 43-character
/// fingerprint does not fit beside a 128-bit UUID in a 31-byte advertisement.
pub const BLE_IDENTITY_UUID: &str = "61637279-6c69-7573-8001-000000000002";
/// Phone to desktop. Written without response, one fragment at a time.
pub const BLE_RX_UUID: &str = "61637279-6c69-7573-8001-000000000003";
/// Desktop to phone, by notification.
pub const BLE_TX_UUID: &str = "61637279-6c69-7573-8001-000000000004";

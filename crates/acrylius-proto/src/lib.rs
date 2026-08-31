//! The acrylius wire format — the one implementation of it. `no_std + alloc`,
//! no IO, no async, so decoders and plugin authors depend on this, not the core.

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

/// mDNS service type. The port is not KDE Connect's 1716; we do not interoperate.
pub const SERVICE_TYPE: &str = "_acrylius._tcp";
pub const DEFAULT_PORT: u16 = 1971;

/// The BLE GATT service; the first eight bytes spell `acrylius` in ASCII.
/// The layout must stay identical across releases: iOS caches a peer's
/// attribute table, and a database that changes shape strands old phones.
pub const BLE_SERVICE_UUID: &str = "61637279-6c69-7573-8001-000000000001";
/// Who this device is, read after connecting: a 43-char fingerprint does not
/// fit in a 31-byte advertisement.
pub const BLE_IDENTITY_UUID: &str = "61637279-6c69-7573-8001-000000000002";
/// Phone to desktop. Written without response, one fragment at a time.
pub const BLE_RX_UUID: &str = "61637279-6c69-7573-8001-000000000003";
/// Desktop to phone, by notification.
pub const BLE_TX_UUID: &str = "61637279-6c69-7573-8001-000000000004";

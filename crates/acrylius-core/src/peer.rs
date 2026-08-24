//! What we remember about a device between runs.

use crate::proto::ids::{DeviceId, Fingerprint, PublicKey};

/// A paired peer. Persisted; everything needed to re-establish a session
/// without a human present.
#[derive(Clone, PartialEq, Eq, Debug, minicbor::Encode, minicbor::Decode)]
pub struct PeerRecord {
    #[n(0)]
    pub device_id: String,
    /// The Noise static public key. This, not the name or the address, is the
    /// peer's identity.
    #[cbor(n(1), with = "minicbor::bytes")]
    pub public_key: Vec<u8>,
    #[n(2)]
    pub name: String,
    #[n(3)]
    pub platform: String,
    /// Derived from the pairing handshake hash, never transmitted.
    #[cbor(n(4), with = "minicbor::bytes")]
    pub session_psk: Vec<u8>,
    /// The replay watermark. Persisting this is the point: a daemon that forgot
    /// it would accept a recorded session opener again after a restart.
    #[n(5)]
    pub greatest_seen: u64,
    /// What the peer last told us it can send and receive. A cache for the UI;
    /// the live handshake is what any routing decision uses.
    #[n(6)]
    pub caps_out: Vec<String>,
    #[n(7)]
    pub caps_in: Vec<String>,
}

impl PeerRecord {
    #[must_use]
    pub fn public_key_array(&self) -> Option<PublicKey> {
        self.public_key.as_slice().try_into().ok()
    }

    #[must_use]
    pub fn session_psk_array(&self) -> Option<[u8; 32]> {
        self.session_psk.as_slice().try_into().ok()
    }

    #[must_use]
    pub fn fingerprint(&self) -> Option<Fingerprint> {
        Some(Fingerprint::of(&self.public_key_array()?))
    }

    #[must_use]
    pub fn id(&self) -> Option<DeviceId> {
        Some(DeviceId::of(&self.public_key_array()?))
    }
}

/// Whether we can currently reach a peer.
///
/// The core models *reachability*, never a role. On iOS a peer is reachable only
/// while the app is foregrounded, and that is an ordinary state rather than an
/// error — which is why a plugin sending to an unreachable peer gets a plain
/// outcome and not a failure path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PeerState {
    Unreachable,
    Connecting,
    Reachable,
}

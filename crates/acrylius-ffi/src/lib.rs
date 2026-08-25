//! The UniFFI facade — the only crate iOS sees.
//!
//! It is deliberately thin. Because the core is sans-IO, this boundary is
//! **synchronous**: Swift calls [`AcryliusCore::handle`] and gets a list of
//! actions back. There is no async across the FFI and, more importantly, no
//! Rust→Swift call anywhere — no foreign traits, no callback interfaces. Swift
//! only ever calls *into* Rust.
//!
//! That is not a small detail. Foreign traits would put a Rust→Swift call in the
//! hot path and reintroduce exactly the reentrancy surface the sans-IO design
//! was chosen to avoid, and it is a surface UniFFI's own documentation declines
//! to give advice about.
//!
//! ## The rule the host must follow
//!
//! Actions are executed by a **single serial executor**, results come back as
//! events, and `handle()` is never called from inside an action handler. On iOS
//! that is one `actor` draining an `AsyncStream`. The `Mutex` below makes
//! breaking the rule safe rather than corrupting, but a host that breaks it will
//! still deadlock itself, so do not.

pub mod bodies;
pub mod types;

use std::sync::Mutex;

use acrylius_core::config::CoreConfig;
use acrylius_core::core::{Core, CoreBuilder};
use acrylius_core::noise::Identity;
use acrylius_core::peer::{PeerRecord, PeerState};
use acrylius_core::plugins::ping;

pub use bodies::*;
pub use types::*;

uniffi::setup_scaffolding!();

#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiConfig {
    pub name: String,
    pub platform: String,
    pub pairing_window_ms: u64,
    pub max_pairing_attempts: u8,
    pub handshake_timeout_ms: u64,
}

impl Default for FfiConfig {
    fn default() -> Self {
        let d = CoreConfig::default();
        Self {
            name: d.name,
            platform: "ios".to_string(),
            pairing_window_ms: d.pairing_window_ms,
            max_pairing_attempts: d.max_pairing_attempts,
            handshake_timeout_ms: d.handshake_timeout_ms,
        }
    }
}

/// Sensible defaults, so a host does not have to invent a pairing window.
#[uniffi::export]
#[must_use]
pub fn default_config(name: String, platform: String) -> FfiConfig {
    FfiConfig {
        name,
        platform,
        ..FfiConfig::default()
    }
}

/// A fresh static identity, as raw private key bytes.
///
/// The host stores these in the Keychain with `WhenUnlockedThisDeviceOnly` and
/// **no biometric ACL**: an item behind `.biometryCurrentSet` cannot be read
/// while the phone is locked, which would break every short-lived extension.
/// Biometrics belong on the *action* — a `LAContext` check before sending an
/// unlock — not on the key. The old project learned that one the hard way.
#[uniffi::export]
#[must_use]
pub fn generate_identity() -> Vec<u8> {
    Identity::generate()
        .map(|i| i.private().to_vec())
        .unwrap_or_default()
}

/// A device's public fingerprint, from its private key. Lets a host show its own
/// identity before building a core.
#[uniffi::export]
pub fn fingerprint_of(identity_key: Vec<u8>) -> Result<String, FfiError> {
    Ok(identity(&identity_key)?.fingerprint().to_string())
}

fn identity(key: &[u8]) -> Result<Identity, FfiError> {
    let key: [u8; 32] = key.try_into().map_err(|_| FfiError::BadInput {
        detail: "an identity key is 32 bytes".to_string(),
    })?;
    Ok(Identity::from_private(key))
}

/// A paired device, for the UI.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPeer {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub fingerprint: String,
    pub reachable: bool,
}

#[derive(uniffi::Object)]
pub struct AcryliusCore {
    inner: Mutex<Core>,
}

#[uniffi::export]
impl AcryliusCore {
    /// Build a core.
    ///
    /// `peers` are the raw blobs the host stored from earlier `Persist` actions,
    /// in any order. One that fails to decode is skipped and reported by
    /// [`Self::restored_peers`] being smaller than what was handed in — the host
    /// should treat that as a corrupted record worth telling someone about,
    /// because "absent" means "this device is a stranger".
    #[uniffi::constructor]
    pub fn new(
        config: FfiConfig,
        identity_key: Vec<u8>,
        peers: Vec<Vec<u8>>,
    ) -> Result<Self, FfiError> {
        let id = identity(&identity_key)?;
        let records: Vec<PeerRecord> = peers
            .iter()
            .filter_map(|b| minicbor::decode(b).ok())
            .collect();
        let core = CoreBuilder::new(
            id,
            CoreConfig {
                name: config.name,
                platform: config.platform,
                pairing_window_ms: config.pairing_window_ms,
                max_pairing_attempts: config.max_pairing_attempts,
                handshake_timeout_ms: config.handshake_timeout_ms,
            },
        )
        // iOS declares no effects yet, so only effect-free plugins survive
        // negotiation. Adding a clipboard effector here is what will switch the
        // clipboard capability on, with no change to the plugin set.
        .effects([])
        .plugin(ping::PingPlugin::default())
        .restore(records)
        .build();
        Ok(Self {
            inner: Mutex::new(core),
        })
    }

    /// The single entry point. `now_ms` is the host's **monotonic** clock, so
    /// that moving the wall clock cannot extend a pairing window.
    pub fn handle(&self, now_ms: u64, event: FfiEvent) -> Result<FfiOutcome, FfiError> {
        let ev = event.try_into()?;
        let mut core = self.inner.lock().expect("core mutex poisoned");
        let out = core.handle(now_ms, ev);
        Ok(FfiOutcome {
            actions: out.actions.into_iter().map(Into::into).collect(),
            next_deadline_ms: out.next_deadline_ms,
        })
    }

    #[must_use]
    pub fn device_id(&self) -> String {
        self.inner
            .lock()
            .expect("core mutex poisoned")
            .device_id()
            .to_string()
    }

    #[must_use]
    pub fn fingerprint(&self) -> String {
        self.inner
            .lock()
            .expect("core mutex poisoned")
            .fingerprint()
            .to_string()
    }

    /// The code currently awaiting confirmation, if any. A view that was
    /// backgrounded and came back reads this rather than relying on having
    /// caught the `PairingSas` event.
    #[must_use]
    pub fn pending_sas(&self) -> Option<String> {
        self.inner
            .lock()
            .expect("core mutex poisoned")
            .pending_sas()
            .map(str::to_string)
    }

    #[must_use]
    pub fn peers(&self) -> Vec<FfiPeer> {
        let core = self.inner.lock().expect("core mutex poisoned");
        core.peers()
            .filter_map(|p| {
                let id = p.id()?;
                Some(FfiPeer {
                    device_id: id.to_string(),
                    name: p.name.clone(),
                    platform: p.platform.clone(),
                    fingerprint: p.fingerprint()?.to_string(),
                    reachable: core.peer_state(&id) == PeerState::Reachable,
                })
            })
            .collect()
    }

    #[must_use]
    pub fn restored_peers(&self) -> u32 {
        u32::try_from(
            self.inner
                .lock()
                .expect("core mutex poisoned")
                .peers()
                .count(),
        )
        .unwrap_or(u32::MAX)
    }

    #[must_use]
    pub fn caps_in(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("core mutex poisoned")
            .caps_in()
            .to_vec()
    }

    #[must_use]
    pub fn caps_out(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("core mutex poisoned")
            .caps_out()
            .to_vec()
    }
}

/// The Bonjour service type the iOS host must list in `NSBonjourServices`.
#[uniffi::export]
#[must_use]
pub fn service_type() -> String {
    acrylius_proto::SERVICE_TYPE.to_string()
}

#[uniffi::export]
#[must_use]
pub fn default_port() -> u16 {
    acrylius_proto::DEFAULT_PORT
}

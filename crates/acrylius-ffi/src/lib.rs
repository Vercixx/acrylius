//! The UniFFI facade, the only crate iOS sees.
//!
//! Synchronous only: no async, no Rust-to-Swift calls cross this boundary. Run
//! actions through one serial executor; never call `handle()` from inside an
//! action handler, or the host deadlocks itself.

pub mod ble;
pub mod bodies;
pub mod bulk;
pub mod types;

use std::sync::Mutex;

use acrylius_core::config::CoreConfig;
use acrylius_core::core::{Core, CoreBuilder};
use acrylius_core::noise::Identity;
use acrylius_core::peer::{PeerRecord, PeerState};
use acrylius_core::plugins::{clipboard, command, media, ping, session, share, touchpad, wol};

pub use bodies::*;
pub use types::*;

uniffi::setup_scaffolding!();

#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiConfig {
    pub name: String,
    pub platform: String,
    pub pairing_window_ms: u64,
    pub handshake_timeout_ms: u64,
}

impl Default for FfiConfig {
    fn default() -> Self {
        let d = CoreConfig::default();
        Self {
            name: d.name,
            platform: "ios".to_string(),
            pairing_window_ms: d.pairing_window_ms,
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
/// Store in Keychain with `WhenUnlockedThisDeviceOnly`, no biometric ACL —
/// biometrics belong on the action, not the key. Fallible rather than
/// returning empty bytes on failure, which would get persisted as a
/// permanent bad identity.
#[uniffi::export]
pub fn generate_identity() -> Result<Vec<u8>, FfiError> {
    Identity::generate()
        .map(|i| i.private().to_vec())
        .map_err(|e| FfiError::Effect {
            detail: e.to_string(),
        })
}

/// A device's public fingerprint, from its private key. Lets a host show its
/// own identity before building a core.
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
    /// Reachable, being reached, or not.
    ///
    /// Three states rather than a bool: a peer mid-handshake and a peer that
    /// gave up look identical through `reachable` alone.
    pub state: FfiPeerState,
    /// What is carrying the session, when one is up. `None` means
    /// unreachable, not unknown.
    pub transport: Option<FfiTransportKind>,
    /// Why the last attempt to reach it ended without a session.
    ///
    /// Only set alongside `Unreachable`. Read at draw time, not delivered as
    /// an event, so a peer reconnecting normally doesn't flicker an error.
    pub trouble: Option<String>,
}

#[derive(uniffi::Object)]
pub struct AcryliusCore {
    inner: Mutex<Core>,
}

#[uniffi::export]
impl AcryliusCore {
    /// Build a core.
    ///
    /// `effects` is what this host can actually carry out; a plugin with a
    /// missing effect still loads and can still send.
    ///
    /// `peers` are raw blobs from earlier `Persist` actions, in any order.
    /// One that fails to decode is skipped — see [`Self::restored_peers`].
    #[uniffi::constructor]
    pub fn new(
        config: FfiConfig,
        identity_key: Vec<u8>,
        peers: Vec<Vec<u8>>,
        effects: Vec<FfiEffectKind>,
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
                handshake_timeout_ms: config.handshake_timeout_ms,
                // Not exposed to hosts: only a sensible default for a phone.
                reconnect_every_ms: CoreConfig::default().reconnect_every_ms,
                // Backstop for a transport's own dial bound. See [`dial_timeout_ms`].
                dial_timeout_ms: CoreConfig::default().dial_timeout_ms,
                // A phone is never dialled (`NWTransport::advertise` is
                // unimplemented), so these just bound its own pairing attempts.
                accept_pair_requests: CoreConfig::default().accept_pair_requests,
                pair_cooldown_ms: CoreConfig::default().pair_cooldown_ms,
                pair_denied_cooldown_ms: CoreConfig::default().pair_denied_cooldown_ms,
            },
        )
        .effects(effects.into_iter().map(Into::into))
        .plugin(ping::PingPlugin::default())
        .plugin(session::SessionPlugin::default())
        // Relays for nobody: sends magic packets itself; empty allowlist refuses relay use.
        .plugin(wol::WolPlugin::new(wol::WolConfig::default(), Vec::new()))
        .plugin(clipboard::ClipboardPlugin::new(clipboard::Directions {
            // Never volunteers pasteboard contents — iOS 16+ prompts "Allow Paste?" per read.
            send: false,
            receive: true,
        }))
        // Runs nothing on request; can still list/run what a peer offers.
        .plugin(command::CommandPlugin::new(Vec::new()))
        // Drives a peer's players; doesn't offer its own.
        .plugin(media::MediaPlugin::default())
        // No Share effect registered: an offer gets an explicit refusal, not silence.
        .plugin(share::SharePlugin::default())
        // No Touchpad effect registered: a phone has none of its own to serve.
        .plugin(touchpad::TouchpadPlugin::default())
        .restore(records)
        .build();
        Ok(Self {
            inner: Mutex::new(core),
        })
    }

    /// The single entry point.
    ///
    /// `monotonic_ms` drives deadlines and only moves forward. `wall_ms` is
    /// Unix-epoch millis, used only for the handshake timestamp peers compare
    /// against their own clock — don't pass the same value for both, or it
    /// reads as a stale timestamp and gets refused.
    pub fn handle(
        &self,
        monotonic_ms: u64,
        wall_ms: u64,
        event: FfiEvent,
    ) -> Result<FfiOutcome, FfiError> {
        let ev = event.try_into()?;
        let mut core = self.inner.lock().expect("core mutex poisoned");
        let out = core.handle(
            acrylius_core::vocab::Now {
                monotonic_ms,
                wall_ms,
            },
            ev,
        );
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

    /// The code currently awaiting confirmation, if any — for a view that
    /// missed the `PairingSas` event.
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
                    state: core.peer_state(&id).into(),
                    transport: core.transport_for(&id).map(Into::into),
                    // Only surfaced while Unreachable — a stale reason is worse than none.
                    trouble: (core.peer_state(&id) == PeerState::Unreachable)
                        .then(|| core.dial_trouble(&id).map(str::to_string))
                        .flatten(),
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

    /// What this device can carry out itself; anything in `caps_in` but absent
    /// here it asks a peer for and refuses if asked.
    #[must_use]
    pub fn caps_served(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("core mutex poisoned")
            .caps_served()
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

/// The BLE service the daemon advertises, for `scanForPeripherals(withServices:)`.
#[uniffi::export]
#[must_use]
pub fn ble_service_uuid() -> String {
    acrylius_proto::BLE_SERVICE_UUID.to_string()
}

/// Read after connecting to learn who a peripheral is: the same facts the mDNS
/// TXT record carries, as `k=v` lines.
#[uniffi::export]
#[must_use]
pub fn ble_identity_uuid() -> String {
    acrylius_proto::BLE_IDENTITY_UUID.to_string()
}

/// Written to, one fragment at a time, without response.
#[uniffi::export]
#[must_use]
pub fn ble_rx_uuid() -> String {
    acrylius_proto::BLE_RX_UUID.to_string()
}

/// Subscribed to for fragments coming back.
#[uniffi::export]
#[must_use]
pub fn ble_tx_uuid() -> String {
    acrylius_proto::BLE_TX_UUID.to_string()
}

#[uniffi::export]
#[must_use]
pub fn default_port() -> u16 {
    acrylius_proto::DEFAULT_PORT
}

/// How long to wait for a lock to be answered before calling it a failure.
///
/// Must match what the host actually waits before answering, or a client
/// times out before a real failure occurs.
#[uniffi::export]
#[must_use]
pub fn session_lock_budget_ms() -> u64 {
    acrylius_core::plugins::session::LOCK_REPLY_BUDGET_MS
}

/// See [`session_lock_budget_ms`].
#[uniffi::export]
#[must_use]
pub fn session_unlock_budget_ms() -> u64 {
    acrylius_core::plugins::session::UNLOCK_REPLY_BUDGET_MS
}

/// See [`session_lock_budget_ms`].
#[uniffi::export]
#[must_use]
pub fn media_command_budget_ms() -> u64 {
    acrylius_core::plugins::media::CONTROL_REPLY_BUDGET_MS
}

/// How long a peer may stop answering before its socket is treated as broken.
/// See [`acrylius_core::link::DEAD_PEER_MS`].
#[uniffi::export]
#[must_use]
pub fn dead_peer_ms() -> u64 {
    acrylius_core::link::DEAD_PEER_MS
}

/// How long a dial may go unanswered before the route it was trying is spent.
/// See [`acrylius_core::link::DIAL_TIMEOUT_MS`].
///
/// The transport must use this exact number for its own dial bound, or the
/// core's backstop and the host's timeout drift out of order.
#[uniffi::export]
#[must_use]
pub fn dial_timeout_ms() -> u64 {
    acrylius_core::link::DIAL_TIMEOUT_MS
}

/// How often to re-read a peer's media while watching it play. See
/// [`acrylius_core::plugins::media::WATCH_INTERVAL_MS`].
#[uniffi::export]
#[must_use]
pub fn media_watch_interval_ms() -> u64 {
    acrylius_core::plugins::media::WATCH_INTERVAL_MS
}

/// The same, over a link where a round trip is expensive — Bluetooth.
#[uniffi::export]
#[must_use]
pub fn media_watch_slow_interval_ms() -> u64 {
    acrylius_core::plugins::media::WATCH_INTERVAL_SLOW_MS
}

/// How often to re-read while nothing is playing.
#[uniffi::export]
#[must_use]
pub fn media_idle_interval_ms() -> u64 {
    acrylius_core::plugins::media::IDLE_INTERVAL_MS
}

/// Whether a reading taken after a command shows the player having acted on it.
///
/// Same rule the desktop uses. `None` means a reading can't answer the
/// question (e.g. a seek moves a position that also moves on its own),
/// and the caller should stop waiting rather than guess.
#[uniffi::export]
#[must_use]
pub fn media_command_landed(
    verb: String,
    player: String,
    value: i64,
    before: crate::bodies::FfiMediaState,
    now: crate::bodies::FfiMediaState,
) -> Option<bool> {
    use acrylius_core::plugins::media;
    let cmd = media::MediaCommand {
        player: player.clone(),
        value,
    };
    // A verb this build does not know is not a question a reading can answer.
    let Ok(action) = media::MediaPlugin::action_for(&verb, &cmd) else {
        return None;
    };
    media::landed(&action, &player, &before.into(), &now.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The budgets a host reads are the core's own constants, not copies.
    #[test]
    fn the_exported_budgets_are_the_ones_the_core_holds() {
        assert_eq!(dead_peer_ms(), acrylius_core::link::DEAD_PEER_MS);
        assert_eq!(dial_timeout_ms(), acrylius_core::link::DIAL_TIMEOUT_MS);
        assert_eq!(
            media_watch_interval_ms(),
            acrylius_core::plugins::media::WATCH_INTERVAL_MS
        );
        // Host must give up before the core's backstop; only the host can hang up.
        assert!(dial_timeout_ms() < CoreConfig::default().dial_timeout_ms);
    }
}

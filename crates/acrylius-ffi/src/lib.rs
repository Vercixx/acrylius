//! The UniFFI facade, the only crate iOS sees.
//!
//! It is deliberately thin. Because the core is sans-IO, this boundary is
//! synchronous: Swift calls [`AcryliusCore::handle`] and gets a list of actions
//! back. There is no async across the FFI and, more importantly, no Rust to
//! Swift call anywhere: no foreign traits, no callback interfaces. Swift only
//! ever calls into Rust.
//!
//! That is not a small detail. Foreign traits would put a Rust to Swift call in
//! the hot path and reintroduce exactly the reentrancy surface the sans-IO design
//! was chosen to avoid, and it is a surface UniFFI's own documentation declines
//! to give advice about.
//!
//! ## The rule the host must follow
//!
//! Actions are executed by a single serial executor, results come back as
//! events, and `handle()` is never called from inside an action handler. On iOS
//! that is one `actor` draining an `AsyncStream`. The `Mutex` below makes
//! breaking the rule safe rather than corrupting, but a host that breaks it will
//! still deadlock itself, so do not.

pub mod ble;
pub mod bodies;
pub mod bulk;
pub mod types;

use std::sync::Mutex;

use acrylius_core::config::CoreConfig;
use acrylius_core::core::{Core, CoreBuilder};
use acrylius_core::noise::Identity;
use acrylius_core::peer::{PeerRecord, PeerState};
use acrylius_core::plugins::{clipboard, command, media, ping, session, share, wol};

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
/// The host stores these in the Keychain with `WhenUnlockedThisDeviceOnly` and
/// no biometric ACL: an item behind `.biometryCurrentSet` cannot be read while
/// the phone is locked, which would break every short-lived extension.
/// Biometrics belong on the action, as a `LAContext` check before sending an
/// unlock, not on the key. The old project learned that one the hard way.
/// Fallible, because the empty vector it used to return on failure was stored
/// as the identity. A caller reads "first run", writes what it is given, and a
/// key that is not a key is then on disk for good — every launch after it loads
/// the same empty bytes and fails the same way, with no path back but reinstall.
#[uniffi::export]
pub fn generate_identity() -> Result<Vec<u8>, FfiError> {
    Identity::generate()
        .map(|i| i.private().to_vec())
        .map_err(|e| FfiError::Effect {
            detail: e.to_string(),
        })
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
    /// Reachable, being reached, or not.
    ///
    /// Three states rather than a bool, because nothing presses Connect any
    /// more: a peer that is mid-handshake and a peer that has given up look
    /// identical through `reachable`, and only one of them is worth explaining
    /// to the person holding the phone.
    pub state: FfiPeerState,
    /// What is carrying the session, when one is up.
    ///
    /// `None` means unreachable, not unknown. Worth showing because the whole
    /// point of a second transport is that it takes over silently, and silence
    /// is indistinguishable from a thing not working.
    pub transport: Option<FfiTransportKind>,
    /// Why the last attempt to reach it ended without a session.
    ///
    /// Only ever set alongside `Unreachable`; a peer still being dialled has
    /// nothing to explain yet. Read at draw time rather than delivered as an
    /// event, so a device coming up normally does not flicker an error.
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
    /// `effects` is what this host can actually carry out. A plugin whose
    /// effects are missing still loads and can still send — being unable to
    /// serve a capability says nothing about being able to use one — so a
    /// phone that cannot lock its own screen can still ask a computer to lock
    /// theirs.
    ///
    /// `peers` are the raw blobs the host stored from earlier `Persist` actions,
    /// in any order. One that fails to decode is skipped and reported by
    /// [`Self::restored_peers`] being smaller than what was handed in. The host
    /// should treat that as a corrupted record worth telling someone about,
    /// because "absent" means "this device is a stranger".
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
                // Not exposed through `FfiCoreConfig`. A phone is the device
                // this matters most on and the one least able to choose a
                // sensible number for itself, and nothing on that side has any
                // reason to want a different one.
                reconnect_every_ms: CoreConfig::default().reconnect_every_ms,
                // Likewise, and more so: this one is a backstop for a transport
                // that fails to bound its own dial, so it is the core's business
                // and not the host's. See [`dial_timeout_ms`].
                dial_timeout_ms: CoreConfig::default().dial_timeout_ms,
                // A phone is never dialled — `NWTransport::advertise` is
                // deliberately unimplemented — so nothing can raise a pairing
                // prompt on it uninvited and there is no door here to shut.
                // These bound the phone's own attempts instead.
                accept_pair_requests: CoreConfig::default().accept_pair_requests,
                pair_cooldown_ms: CoreConfig::default().pair_cooldown_ms,
                pair_denied_cooldown_ms: CoreConfig::default().pair_denied_cooldown_ms,
            },
        )
        // The same plugin list every device registers, which is the point of
        // the plugin set being platform-independent. This was left at ping
        // alone from the skeleton, so a phone advertised no capabilities and a
        // computer would not send it a clipboard — the failure looked like a
        // missing clipboard implementation, and was a missing registration.
        //
        // What this device can *serve* is what `effects` names; the rest it can
        // still ask other devices for.
        .effects(effects.into_iter().map(Into::into))
        .plugin(ping::PingPlugin::default())
        .plugin(session::SessionPlugin::default())
        // A phone relays a wake for nobody: it sends magic packets itself, and
        // an empty allowlist is what refuses to be used as a relay.
        .plugin(wol::WolPlugin::new(wol::WolConfig::default(), Vec::new()))
        .plugin(clipboard::ClipboardPlugin::new(clipboard::Directions {
            // Never volunteers what is on the pasteboard. Since iOS 16 reading
            // it raises a system "Allow Paste?" alert for anything another app
            // put there, so a phone that mirrored every change would prompt
            // constantly. Sending is a deliberate act; receiving is not.
            send: false,
            receive: true,
        }))
        // Runs nothing on request. It can still list and run what a computer
        // offers.
        .plugin(command::CommandPlugin::new(Vec::new()))
        // A phone plays its own audio through its own controls. It registers
        // this to drive a computer's players, not to offer its own.
        .plugin(media::MediaPlugin::default())
        // Registered without an `EffectKind::Share` to serve it, which is the
        // difference between "cannot" and "does not answer". A computer that
        // offers this phone a file gets a refusal it can show a person; without
        // the registration it would get silence and a capability the phone
        // never admitted to knowing about.
        .plugin(share::SharePlugin::default())
        .restore(records)
        .build();
        Ok(Self {
            inner: Mutex::new(core),
        })
    }

    /// The single entry point.
    ///
    /// Two clocks, and they are not interchangeable. `monotonic_ms` counts from
    /// an arbitrary origin and only ever moves forward; it drives deadlines, so
    /// that changing the system clock cannot extend a pairing window. `wall_ms`
    /// is milliseconds since the Unix epoch, and is used for one thing only:
    /// the handshake timestamp the other device compares against its own clock.
    ///
    /// Passing the monotonic clock for both means sending your *uptime* as a
    /// timestamp, which every peer reads as wildly stale and refuses.
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
                    state: core.peer_state(&id).into(),
                    transport: core.transport_for(&id).map(Into::into),
                    // Paired with the state deliberately: a reason kept past
                    // the reconnection it explains is worse than none.
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

    /// What this device can carry out itself. Anything in `caps_in` but absent
    /// here it can ask a peer for and will refuse if asked.
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
///
/// Exported for the same reason `service_type()` is: a UUID spelled out twice is
/// a UUID that can differ by one hex digit, and the failure that produces is a
/// phone which never finds a desktop with no error anywhere to say why. That is
/// the exact shape of the bug this project's predecessor died on.
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
/// Exported rather than written down again on this side. A host spends up to its
/// own confirm budget watching the screen locker before it answers, so a client
/// that waits any less reports a failure that did not happen — which is what a
/// pair of eight-second timeouts, one here and one in Swift, used to do.
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
/// Exported because the transport that opened the connection is the only thing
/// that can hang it up, so it has to bound the dial itself — and it must use
/// this number rather than one of its own, or the core's backstop and the
/// host's timeout drift into the order where the backstop fires first.
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
/// The same rule the desktop waits on, so the two ends cannot disagree about
/// what "it worked" means. `None` — surfaced here as a null — means a reading
/// cannot answer the question and the caller should stop waiting rather than
/// guess: a seek moves a position that also moves on its own.
///
/// Comparing whole states instead is what this replaces, and it was wrong in
/// both directions: a playing track's position moves between any two readings,
/// so every command looked like it landed, while a paused one looked like
/// nothing ever did.
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

    /// The budgets a host is told to use are the core's, not copies of them.
    ///
    /// These accessors exist so that a number lives in exactly one place and
    /// both ends read it. That only holds while each really returns the
    /// constant it names — an accessor quietly answering something else is
    /// indistinguishable from the drift they were added to prevent, and it
    /// would be read on a phone, where nothing else here can see it.
    #[test]
    fn the_exported_budgets_are_the_ones_the_core_holds() {
        assert_eq!(dead_peer_ms(), acrylius_core::link::DEAD_PEER_MS);
        assert_eq!(dial_timeout_ms(), acrylius_core::link::DIAL_TIMEOUT_MS);
        assert_eq!(
            media_watch_interval_ms(),
            acrylius_core::plugins::media::WATCH_INTERVAL_MS
        );
        // And the order between the two halves of a bounded dial: the host
        // gives up first, because only the host can hang up the connection.
        assert!(dial_timeout_ms() < CoreConfig::default().dial_timeout_ms);
    }
}

//! Core tunables.

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CoreConfig {
    /// What this device calls itself to peers.
    pub name: String,
    /// `"linux"`, `"ios"`. Advisory; used for icons and copy, never for policy.
    pub platform: String,
    /// How long a person has to compare the six digits, on the host's monotonic
    /// clock so changing the wall clock cannot extend it.
    pub pairing_window_ms: u64,
    /// Whether this device will answer a pairing handshake at all.
    pub accept_pair_requests: bool,
    /// How long to ignore further pairing handshakes after one lapses or is
    /// abandoned; caps how often a hostile device can raise a dialog.
    pub pair_cooldown_ms: u64,
    /// Cooldown after a person says the digits differ. Longer than
    /// [`Self::pair_cooldown_ms`]: a SAS mismatch suggests an active relay.
    pub pair_denied_cooldown_ms: u64,
    /// How often to redial a paired device nothing can currently reach.
    /// Needed because sightings (mDNS, CoreBluetooth) are not heartbeats.
    pub reconnect_every_ms: u64,
    /// How long an unfinished handshake may hold a link.
    pub handshake_timeout_ms: u64,
    /// Backstop for transports that do not time out their own dials; must fire
    /// later than [`crate::link::DIAL_TIMEOUT_MS`].
    pub dial_timeout_ms: u64,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            name: "acrylius".to_string(),
            platform: "unknown".to_string(),
            pairing_window_ms: 120_000,
            accept_pair_requests: true,
            pair_cooldown_ms: 30_000,
            pair_denied_cooldown_ms: 300_000,
            reconnect_every_ms: 10_000,
            handshake_timeout_ms: 15_000,
            dial_timeout_ms: crate::link::DIAL_TIMEOUT_MS * 2,
        }
    }
}

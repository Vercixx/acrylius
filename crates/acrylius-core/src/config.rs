//! Core tunables. Every one of these is a policy decision, not a constant of
//! nature, so they live together where they can be seen and argued with.

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CoreConfig {
    /// What this device calls itself to peers.
    pub name: String,
    /// `"linux"`, `"ios"`. Advisory; used for icons and copy, never for policy.
    pub platform: String,
    /// How long a person has to compare six digits before the pairing lapses.
    ///
    /// Measured by the host on a monotonic clock, so that changing the wall
    /// clock cannot extend it. That lesson is carried over from `pc-helper-ios`.
    pub pairing_window_ms: u64,
    /// Whether this device will answer a pairing handshake at all.
    ///
    /// `false` is the door shut: a machine that is already paired with
    /// everything it wants to be, or one on a network it does not trust to be
    /// allowed to raise a dialog on it.
    pub accept_pair_requests: bool,
    /// How long to ignore further pairing handshakes after one lapses or is
    /// abandoned.
    ///
    /// Anyone may start a handshake now, so without this a hostile device on the
    /// network is a dialog every two minutes forever.
    pub pair_cooldown_ms: u64,
    /// How long to ignore pairing handshakes after a person says the digits
    /// *differ*.
    ///
    /// Longer than [`Self::pair_cooldown_ms`], and the asymmetry is the point.
    /// Since the SAS is the security mechanism, a mismatch is not somebody
    /// fumbling — it is the one observable signal that something is relaying
    /// between two handshakes, and its next attempt is another one-in-a-million
    /// try at the same trick. Slow it down.
    pub pair_denied_cooldown_ms: u64,
    /// How often to try a paired device again that nothing can currently reach.
    ///
    /// Auto-connect fires on a *sighting*, and a sighting is not a heartbeat:
    /// mDNS resolves a service once and then says nothing until something
    /// about it changes, and CoreBluetooth reports a peripheral it has already
    /// reported at its own discretion. So every way of losing a session that
    /// does not end with a fresh advertisement — a computer sleeping, a phone
    /// spending a minute in a pocket, Wi-Fi coming back — left the device
    /// unreachable with nothing scheduled to fix it. This is that heartbeat.
    pub reconnect_every_ms: u64,
    /// How long an unfinished handshake may hold a link.
    pub handshake_timeout_ms: u64,
    /// How long the core waits for a transport to answer a dial before giving
    /// up on that route and trying the next.
    ///
    /// A backstop, not the primary bound: a transport that can time out its own
    /// dial should, because only it can hang up the connection it opened. This
    /// catches the one that does not, and it must therefore be the *later* of
    /// the two — see [`crate::link::DIAL_TIMEOUT_MS`], which is what the hosts
    /// bound themselves by and what this is derived from.
    pub dial_timeout_ms: u64,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            name: "acrylius".to_string(),
            platform: "unknown".to_string(),
            pairing_window_ms: 120_000,
            accept_pair_requests: true,
            // One dialog, then quiet for a while. Long enough that a device
            // trying repeatedly is not a stream of notifications, short enough
            // that a person who tapped the wrong machine and wants to try again
            // is not left wondering whether it is broken.
            pair_cooldown_ms: 30_000,
            pair_denied_cooldown_ms: 300_000,
            // Long enough that a machine which is simply off is not dialled
            // constantly, short enough that coming back from a locked phone
            // feels like it reconnected rather than like it was fixed.
            reconnect_every_ms: 10_000,
            handshake_timeout_ms: 15_000,
            // Derived rather than chosen, so the two cannot drift apart into
            // the order that makes this useless: a backstop that fires first
            // takes the route walk away from the host that was about to answer
            // properly, and leaves a stale token behind for it to answer into.
            dial_timeout_ms: crate::link::DIAL_TIMEOUT_MS * 2,
        }
    }
}

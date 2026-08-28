//! Core tunables. Every one of these is a policy decision, not a constant of
//! nature, so they live together where they can be seen and argued with.

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CoreConfig {
    /// What this device calls itself to peers.
    pub name: String,
    /// `"linux"`, `"ios"`. Advisory; used for icons and copy, never for policy.
    pub platform: String,
    /// How long a pairing window stays open.
    ///
    /// Measured by the host on a monotonic clock, so that changing the wall
    /// clock cannot extend it. That lesson is carried over from `pc-helper-ios`.
    pub pairing_window_ms: u64,
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
    /// How many failed handshakes close the window entirely.
    ///
    /// A correct code with a failed handshake is not a typo; it means the code
    /// reached someone who could not complete with it. Burn the window.
    pub max_pairing_attempts: u8,
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
            // Long enough that a machine which is simply off is not dialled
            // constantly, short enough that coming back from a locked phone
            // feels like it reconnected rather than like it was fixed.
            reconnect_every_ms: 10_000,
            max_pairing_attempts: 3,
            handshake_timeout_ms: 15_000,
            // Derived rather than chosen, so the two cannot drift apart into
            // the order that makes this useless: a backstop that fires first
            // takes the route walk away from the host that was about to answer
            // properly, and leaves a stale token behind for it to answer into.
            dial_timeout_ms: crate::link::DIAL_TIMEOUT_MS * 2,
        }
    }
}

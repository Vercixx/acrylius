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
    /// Measured by the host on a **monotonic** clock, so that changing the wall
    /// clock cannot extend it — a lesson carried over from `pc-helper-ios`.
    pub pairing_window_ms: u64,
    /// How many failed handshakes close the window entirely.
    ///
    /// A correct code with a failed handshake is not a typo; it means the code
    /// reached someone who could not complete with it. Burn the window.
    pub max_pairing_attempts: u8,
    /// How long an unfinished handshake may hold a link.
    pub handshake_timeout_ms: u64,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            name: "acrylius".to_string(),
            platform: "unknown".to_string(),
            pairing_window_ms: 120_000,
            max_pairing_attempts: 3,
            handshake_timeout_ms: 15_000,
        }
    }
}

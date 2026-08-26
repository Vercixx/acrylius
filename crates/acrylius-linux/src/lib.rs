//! Effectors for a Linux desktop.
//!
//! The core decides what should happen. This crate is where it actually
//! happens, and it is the only part of the project that knows logind exists.
//!
//! Everything here runs as your user, never root. logind passes a session's
//! owner uid to polkit as `good_user`, which short-circuits the check when the
//! caller's uid matches, so locking and unlocking your own session needs no
//! sudo, no setuid binary and no polkit rule. That is what lets the systemd unit
//! be locked down hard: nothing it does requires privilege.

pub mod clipboard;
pub mod command;
pub mod compositor;
pub mod effector;
pub mod media;
pub mod session;
pub mod wol;

/// This process's real uid.
///
/// Read from `/proc` rather than pulling in `libc` for one number.
#[must_use]
pub fn uid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|v| v.split_whitespace().next().map(str::to_string))
        })
        .and_then(|v| v.parse().ok())
        .unwrap_or(u32::MAX)
}

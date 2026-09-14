//! Effectors for a Linux desktop.
//!
//! Everything runs as your user, never root: logind short-circuits polkit when
//! the caller's uid owns the session, so no privilege is ever needed.

pub mod ble;
pub mod clipboard;
pub mod command;
pub mod compositor;
pub mod effector;
pub mod media;
pub mod mixer;
pub mod notify;
pub mod session;
pub mod touchpad;
pub mod usb;
pub mod wol;

/// This process's real uid, from `/proc` rather than `libc`.
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

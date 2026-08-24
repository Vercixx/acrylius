//! The acrylius protocol core: a sans-IO state machine.
//!
//! This crate owns no sockets, no clock, no filesystem and no async runtime. A
//! host feeds it [`Event`]s and applies the [`Action`]s that come back. That is
//! what lets one compiled artifact drive a tokio daemon on Linux and a
//! Network.framework transport on iOS, and it is what makes the handshake,
//! pairing and routing testable with `cargo test` alone — which matters a great
//! deal when there is no Mac in the building.
//!
//! **The rule that keeps this sound, on every host:** actions are executed by a
//! single serial executor, results come back as events, and `handle()` is never
//! called from inside an action handler.

pub use acrylius_proto as proto;

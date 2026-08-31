//! The acrylius protocol core: a sans-IO state machine. Hosts feed it
//! [`Event`]s and apply the [`Action`]s that come back.
//!
//! Invariant: actions run on a single serial executor, results return as
//! events, and `handle()` is never called from inside an action handler.

pub use acrylius_proto as proto;

pub mod config;
pub mod core;
pub mod link;
pub mod noise;
pub mod peer;
pub mod plugin;
pub mod plugins;
pub mod vocab;

//! The host runtime, for Rust hosts only.
//!
//! iOS does not use this crate and never will: there, Swift owns the sockets via
//! Network.framework and talks to the core through the same `Event`/`Action`
//! vocabulary. That vocabulary is the normative seam; the [`Transport`] trait
//! here is a convenience for hosts that happen to be written in Rust.
//!
//! Keeping that distinction sharp is what stops "transports are plugins" from
//! quietly becoming "transports are Rust trait objects", which iOS could not
//! satisfy.

pub mod effector;
pub mod runtime;
pub mod store;
pub mod tcp;
pub mod transport;

pub use runtime::Runtime;
pub use transport::{Transport, TransportCmd};

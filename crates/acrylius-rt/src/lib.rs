//! The host runtime, for Rust hosts only. iOS owns its sockets in Swift and
//! talks to the core via `Event`/`Action`; that vocabulary is the normative
//! seam, and the [`Transport`] trait here is a Rust-host convenience.

pub mod bulk;
pub mod effector;
pub mod runtime;
pub mod store;
pub mod tcp;
pub mod transport;

pub use runtime::Runtime;
pub use transport::{Transport, TransportCmd};

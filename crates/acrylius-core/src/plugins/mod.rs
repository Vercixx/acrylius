//! The built-in plugins' protocol halves.
//!
//! They live inside `acrylius-core` rather than in their own crate because they
//! need [`Plugin`](crate::plugin::Plugin) and [`Cx`](crate::plugin::Cx), which
//! live here — a separate crate would buy a dependency cycle and nothing else.
//! Moving them out later is a `git mv`, not a wire change.

pub mod clipboard;
pub mod command;
pub mod ping;
pub mod session;
pub mod wol;

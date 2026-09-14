//! The built-in plugins' protocol halves. In `acrylius-core` because they need
//! [`Plugin`](crate::plugin::Plugin) and [`Cx`](crate::plugin::Cx).

pub mod clipboard;
pub mod command;
pub mod media;
pub mod ping;
pub mod session;
pub mod share;
pub mod touchpad;
pub mod wol;

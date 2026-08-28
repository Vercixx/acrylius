//! Shared between the daemon and its CLI.
//!
//! The crate builds two binaries that talk to each other over a Unix socket,
//! and until M3 each carried its own copy of the vocabulary they spoke. They
//! agreed, but nothing made them: a field added to one compiled perfectly and
//! failed at runtime as a connection that closed with no reply.
//!
//! Only the wire types live here. The daemon's own modules stay private to its
//! binary — the CLI has no business knowing how a control socket is served.

pub mod ipc;

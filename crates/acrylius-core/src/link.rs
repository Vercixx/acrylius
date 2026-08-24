//! Transport vocabulary.
//!
//! The core never names a socket, an address family or a port. It knows only
//! that *links* exist, that they carry whole messages, and what each link can
//! and cannot do. A transport is whatever produces `LinkUp`/`LinkRecv`/`LinkDown`
//! and consumes `Dial`/`LinkSend`/`Close` — which is why the iOS transport can be
//! Swift over Network.framework while the Linux one is Rust over tokio, with no
//! trait crossing the FFI boundary between them.

/// Host-assigned, unique for the lifetime of a process. The core treats it as
/// opaque and never invents one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LinkId(pub u64);

/// Identifies which transport a link came from, so a reconnect goes back out the
/// same way it came in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TransportId(pub u16);

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TransportKind {
    /// TCP over a local network. The M1 transport.
    TcpLan,
    /// A Unix socket on the same machine — used by the loopback tests today, and
    /// by out-of-process plugins later.
    UnixLoopback,
    /// Bluetooth LE, L2CAP connection-oriented channel. Not implemented; the
    /// variant exists so the message-size and bulk plumbing has a real second
    /// case to answer to rather than a hypothetical one.
    BleL2cap,
    Custom(&'static str),
}

/// A hint for plugin behaviour, never for correctness. A plugin may use it to
/// decide how chatty to be; nothing may *depend* on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LatencyClass {
    Loopback,
    Lan,
    Ble,
    Wan,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BulkSupport {
    /// No bulk transfers. The core refuses one with a clear error rather than
    /// silently trying to push a gigabyte through 185-byte writes.
    None,
    /// The transport can open a separate channel for bulk bytes.
    SideChannel,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LinkAttrs {
    pub transport: TransportId,
    pub kind: TransportKind,
    /// The largest whole message this link accepts, *after* whatever
    /// fragmentation the transport does internally. The core will never hand
    /// down a frame larger than this, and enforces it on plugins so an
    /// oversized body is a `TooLarge` error rather than a mysterious hang.
    pub max_message: u32,
    pub reliable: bool,
    pub ordered: bool,
    pub latency: LatencyClass,
    pub bulk: BulkSupport,
}

impl LinkAttrs {
    /// The attributes of an ordinary LAN TCP link.
    #[must_use]
    pub fn tcp_lan(transport: TransportId) -> Self {
        Self {
            transport,
            kind: TransportKind::TcpLan,
            max_message: 1 << 20,
            reliable: true,
            ordered: true,
            latency: LatencyClass::Lan,
            bulk: BulkSupport::SideChannel,
        }
    }

    /// In-process, used by the loopback conformance tests.
    #[must_use]
    pub fn loopback(transport: TransportId) -> Self {
        Self {
            kind: TransportKind::UnixLoopback,
            latency: LatencyClass::Loopback,
            ..Self::tcp_lan(transport)
        }
    }

    /// Whether a Noise session on this link may keep its nonce counter
    /// internally. A lossy or unordered link needs caller-supplied nonces and a
    /// replay window instead — see `noise::Session`.
    #[must_use]
    pub fn supports_stateful_cipher(&self) -> bool {
        self.reliable && self.ordered
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LinkDownReason {
    /// The peer closed cleanly.
    Closed,
    /// The transport failed: reset, timeout, interface went away.
    Transport(String),
    /// We closed it, because the protocol said to.
    Protocol(crate::proto::envelope::ErrorCode),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lossy_link_may_not_use_a_stateful_cipher() {
        let mut attrs = LinkAttrs::tcp_lan(TransportId(0));
        assert!(attrs.supports_stateful_cipher());
        attrs.ordered = false;
        assert!(!attrs.supports_stateful_cipher());
        attrs.ordered = true;
        attrs.reliable = false;
        assert!(!attrs.supports_stateful_cipher());
    }
}

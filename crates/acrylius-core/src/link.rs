//! Transport vocabulary. The core never names a socket or an address family;
//! it knows only links that carry whole messages, and what each link can do.

/// How long a peer may stop answering before its socket is treated as broken.
/// Both hosts must bound this: sleep and Wi-Fi loss close nothing, they just
/// go quiet.
pub const DEAD_PEER_MS: u64 = 20_000;

/// How long a dial may go unanswered before the route it was trying is spent.
/// Needed because Network.framework never fails a dial with no viable path,
/// and the core will not start the next route while one is outstanding.
pub const DIAL_TIMEOUT_MS: u64 = 6_000;

/// Host-assigned, unique for the lifetime of a process. The core treats it as
/// opaque and never invents one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LinkId(pub u64);

/// Identifies which transport a link came from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TransportId(pub u16);

/// Bits of a `LinkId` left to the transport's own counter; the rest name the
/// transport.
const LINK_COUNTER_BITS: u32 = 48;

impl LinkId {
    /// Naming the transport in the high bits keeps ids from transports that
    /// each count from 1 from colliding.
    #[must_use]
    pub fn new(transport: TransportId, counter: u64) -> Self {
        Self((u64::from(transport.0) << LINK_COUNTER_BITS) | (counter & Self::COUNTER_MASK))
    }

    const COUNTER_MASK: u64 = (1 << LINK_COUNTER_BITS) - 1;

    /// Which transport minted this id.
    #[must_use]
    pub fn transport(self) -> TransportId {
        // Truncation is the point: the high 16 bits are the transport.
        #[allow(clippy::cast_possible_truncation)]
        TransportId((self.0 >> LINK_COUNTER_BITS) as u16)
    }
}

/// Where a device can be reached: at most one address per transport, so one
/// transport's sighting cannot evict another's route.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Routes(std::collections::BTreeMap<TransportId, String>);

impl Routes {
    pub fn set(&mut self, transport: TransportId, addr: String) {
        self.0.insert(transport, addr);
    }

    /// Forget this transport's address, if it is the one given. Checked because
    /// a withdrawal can race a newer sighting on the same transport.
    /// Returns whether anything was removed.
    pub fn forget(&mut self, transport: TransportId, addr: &str) -> bool {
        if self.0.get(&transport).is_some_and(|a| a == addr) {
            self.0.remove(&transport);
            return true;
        }
        false
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Routes to try, best first: ascending `TransportId`, which hosts assign
    /// in preference order.
    pub fn in_preference_order(&self) -> impl Iterator<Item = (TransportId, String)> + '_ {
        self.0.iter().map(|(t, a)| (*t, a.clone()))
    }

    /// Take everything `other` knows, letting it win where both have an answer.
    pub fn merge_from(&mut self, other: &Routes) {
        for (t, a) in &other.0 {
            self.0.insert(*t, a.clone());
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TransportKind {
    /// TCP over a local network.
    TcpLan,
    /// A Unix socket on the same machine, used by the loopback tests.
    UnixLoopback,
    /// Bluetooth LE, messages fragmented across GATT writes and notifications.
    /// See `acrylius_proto::ble` and PROTOCOL.md §5.1.
    BleGatt,
    Custom(&'static str),
}

/// A hint for plugin behaviour, never for correctness.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LatencyClass {
    Loopback,
    Lan,
    Ble,
    Wan,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BulkSupport {
    /// No bulk transfers; the core refuses one with a clear error.
    None,
    /// The transport can open a separate channel for bulk bytes.
    SideChannel,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LinkAttrs {
    pub transport: TransportId,
    pub kind: TransportKind,
    /// Largest whole message this link accepts, after transport-internal
    /// fragmentation. Enforced on plugins as a `TooLarge` error.
    pub max_message: u32,
    pub reliable: bool,
    pub ordered: bool,
    pub latency: LatencyClass,
    pub bulk: BulkSupport,
}

impl LinkAttrs {
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

    /// `max_message` is a latency budget, not the ATT MTU: the transport
    /// fragments, so this bounds how much the core hands down at once.
    #[must_use]
    pub fn ble(transport: TransportId) -> Self {
        Self {
            transport,
            kind: TransportKind::BleGatt,
            max_message: 16 * 1024,
            // The link layer retransmits; a connection that cannot deliver
            // drops, taking the reassembler with it.
            reliable: true,
            ordered: true,
            latency: LatencyClass::Ble,
            // The bulk side channel is a TCP listener, which BLE cannot offer.
            bulk: BulkSupport::None,
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
    /// internally; a lossy or unordered link needs caller-supplied nonces and
    /// a replay window instead. See `noise::Session`.
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
    fn two_transports_counting_from_one_do_not_collide() {
        let a = LinkId::new(TransportId(1), 1);
        let b = LinkId::new(TransportId(2), 1);
        assert_ne!(a, b, "the whole point of naming the transport in the id");
        assert_eq!(a.transport(), TransportId(1));
        assert_eq!(b.transport(), TransportId(2));
    }

    #[test]
    fn a_counter_that_overflows_its_field_stays_in_its_own_transport() {
        // Wrapping is survivable; leaking into another transport's range is not.
        let huge = LinkId::new(TransportId(7), u64::MAX);
        assert_eq!(huge.transport(), TransportId(7));
    }

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

//! Transport vocabulary.
//!
//! The core never names a socket, an address family or a port. It knows only
//! that links exist, that they carry whole messages, and what each link can and
//! cannot do. A transport is whatever produces `LinkUp`/`LinkRecv`/`LinkDown` and
//! consumes `Dial`/`LinkSend`/`Close`, which is why the iOS transport can be
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

/// Bits of a `LinkId` left to the transport's own counter. The rest name the
/// transport.
const LINK_COUNTER_BITS: u32 = 48;

impl LinkId {
    /// Build an id that no other transport can collide with.
    ///
    /// The core keys links in one flat table, but every transport hands out its
    /// own ids and none of them can see the others. Two transports that both
    /// start counting at 1 therefore produce two different links called 1, and
    /// because `Action::LinkSend` is offered to every transport and acted on by
    /// whichever recognises the id, the wrong one answers. Naming the transport
    /// in the high bits makes that impossible rather than unlikely — the same
    /// move `Runtime` makes for reentrancy.
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

/// Where a device can be reached: at most one address per transport.
///
/// A single `(TransportId, String)` was enough while there was one transport.
/// With two, discovery on either overwrites the other, and the last transport to
/// speak decides how we dial — so a BLE sighting could evict a working Wi-Fi
/// route and quietly make everything slower. Keeping one per transport also
/// gives a failed dial somewhere else to go.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Routes(std::collections::BTreeMap<TransportId, String>);

impl Routes {
    pub fn set(&mut self, transport: TransportId, addr: String) {
        self.0.insert(transport, addr);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Routes to try, best first.
    ///
    /// "Best" is ascending `TransportId`, which is not a guess: the host assigns
    /// those ids, and assigns them in preference order. The core stays ignorant
    /// of what any transport actually is, which is the rule this whole module
    /// exists to keep.
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
    /// TCP over a local network. The M1 transport.
    TcpLan,
    /// A Unix socket on the same machine, used by the loopback tests today and
    /// by out-of-process plugins later.
    UnixLoopback,
    /// Bluetooth LE, messages fragmented across GATT writes and notifications.
    /// See `acrylius_proto::ble` for the framing and PROTOCOL.md §5.1.
    ///
    /// GATT rather than an L2CAP connection-oriented channel, which is what this
    /// variant used to be called. L2CAP needs a PSM, which on BLE has to be
    /// published over GATT anyway, and on Linux it means raw `AF_BLUETOOTH`
    /// sockets the hardened user unit does not permit. It is still the right
    /// answer for bulk later, at which point it earns its own variant.
    BleGatt,
    Custom(&'static str),
}

/// A hint for plugin behaviour, never for correctness. A plugin may use it to
/// decide how chatty to be; nothing may depend on it.
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
    /// The largest whole message this link accepts, after whatever
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

    /// A Bluetooth LE link.
    ///
    /// `max_message` is deliberately not the ATT MTU. The transport fragments,
    /// so this is how much of a message the core may hand down at once — a
    /// budget, chosen for how long it takes to arrive rather than for what fits
    /// in one packet. At the 517-byte MTU an iPhone negotiates, 16 KiB is about
    /// thirty notifications.
    #[must_use]
    pub fn ble(transport: TransportId) -> Self {
        Self {
            transport,
            kind: TransportKind::BleGatt,
            max_message: 16 * 1024,
            // True for as long as the connection lives: the link layer
            // retransmits, and a connection that cannot deliver drops instead of
            // silently losing a fragment. When it drops, the link goes down and
            // the reassembler goes with it.
            reliable: true,
            ordered: true,
            latency: LatencyClass::Ble,
            // The bulk side channel is a TCP listener. Claiming one here would
            // be a lie, and `BulkSupport::None` is the variant that exists for
            // exactly this case.
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
    /// internally. A lossy or unordered link needs caller-supplied nonces and a
    /// replay window instead; see `noise::Session`.
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
        // Wrapping is survivable; leaking into the next transport's range is
        // not, because it would silently hand our link to someone else.
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

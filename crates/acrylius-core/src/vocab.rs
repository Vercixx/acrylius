//! The Event / Action vocabulary. This is the normative host seam.
//!
//! This is the part other implementations conform to. A host "implements a
//! transport" by producing [`Event::LinkUp`] / [`Event::LinkRecv`] /
//! [`Event::LinkDown`] and carrying out [`Action::Dial`] / [`Action::LinkSend`] /
//! [`Action::Close`]. It "implements an effector" by carrying out
//! [`Action::Effect`] and reporting [`Event::EffectDone`]. There is no trait to
//! implement and nothing to link against, which is exactly why the iOS host can
//! be Swift over Network.framework with no Rust to Swift call anywhere in it.
//!
//! Every host must follow one rule: actions are executed by a single serial
//! executor, results come back as events, and `handle()` is never called from
//! inside an action handler. Breaking it produces reentrancy bugs that are very
//! hard to see and very easy to avoid.

use crate::link::{LinkAttrs, LinkDownReason, LinkId, TransportId};
use crate::proto::envelope::ErrorCode;
use crate::proto::ids::{DeviceId, Fingerprint};

/// The two clocks the core needs. They are not interchangeable, and a struct
/// rather than two arguments so they cannot be transposed by accident.
///
/// Conflating them is not a hypothetical mistake. When the handshake timestamp
/// was taken from the monotonic clock, it carried each device's *uptime* — so a
/// computer up for hours and a phone just unlocked disagreed by hours, every
/// session was refused as stale, and the only reason no test caught it was that
/// every test started both cores in the same instant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Now {
    /// Milliseconds from an arbitrary origin that only ever moves forward.
    ///
    /// Deadlines only. It must not jump when the system clock is corrected, or
    /// a pairing window could be extended by changing the time.
    pub monotonic_ms: u64,
    /// Milliseconds since the Unix epoch.
    ///
    /// Used for exactly one thing: the handshake timestamp two devices compare
    /// against each other. Nothing local may depend on it.
    pub wall_ms: u64,
}

/// Correlates an [`Action::Effect`] with the [`Event::EffectDone`] answering it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct EffectToken(pub u64);

/// Correlates an [`Action::Dial`] with the link it eventually produces.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DialToken(pub u64);

/// A peer found by discovery. Untrusted: it supplies candidate addresses and
/// nothing more. Identity comes from the handshake, never from an advertisement.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DiscoveredPeer {
    /// Advertised fingerprint. A hint for matching against a known peer: a liar
    /// can put anything here, and the handshake is what settles it.
    pub fingerprint: Option<Fingerprint>,
    pub name: String,
    /// Transport-defined and opaque to the core: a `host:port`, a BLE address.
    pub addr: String,
    /// Whether the advertiser says it currently has a pairing window open.
    pub pairing: bool,
}

#[derive(Debug)]
pub enum Event {
    /// A link is usable. `dial` is set when this link answers an
    /// [`Action::Dial`], and `None` when the peer connected to us.
    LinkUp {
        link: LinkId,
        attrs: LinkAttrs,
        dial: Option<DialToken>,
    },
    /// One whole message. The transport has already dealt with framing and any
    /// fragmentation of its own.
    LinkRecv { link: LinkId, msg: Vec<u8> },
    LinkDown {
        link: LinkId,
        reason: LinkDownReason,
    },
    /// A dial failed before any link existed.
    DialFailed { dial: DialToken, reason: String },
    Discovered {
        transport: TransportId,
        peer: DiscoveredPeer,
    },
    /// The single host timer fired. See [`Outcome::next_deadline_ms`].
    Tick,
    /// A local UI or CLI asked for something.
    Local(LocalCommand),
    EffectDone {
        token: EffectToken,
        result: EffectResult,
    },
}

/// Something a human asked for, locally.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LocalCommand {
    /// Open a pairing window and wait for someone to use `code`.
    ///
    /// On the desktop this is reachable only over the `SO_PEERCRED`-guarded
    /// control socket. There is deliberately no network route to it, so "you
    /// must be at the machine" is a property of the transport rather than a rule
    /// a handler could forget to enforce.
    OpenPairingWindow {
        code: String,
    },
    /// Dial `addr` and try to pair using `code`.
    RequestPairing {
        transport: TransportId,
        addr: String,
        code: String,
    },
    /// Answer the SAS prompt. `false` means the codes did not match, which is
    /// treated as a hostile handshake, not a typo.
    ConfirmPairing {
        accept: bool,
    },
    ClosePairingWindow,
    /// Tell the core where a peer can be reached, bypassing discovery.
    ///
    /// Discovery is only ever a hint, so a hint supplied by a human who knows
    /// the address is worth exactly as much. It is also what makes the daemon
    /// usable on a network where mDNS is filtered.
    SetPeerAddress {
        peer: DeviceId,
        transport: TransportId,
        addr: String,
    },
    Connect {
        peer: DeviceId,
    },
    Disconnect {
        peer: DeviceId,
    },
    /// Forget a peer entirely. Its next connection is a stranger's.
    Revoke {
        peer: DeviceId,
    },
    /// Hand a plugin something to send.
    Plugin {
        peer: DeviceId,
        cap: String,
        ty: String,
        body: Vec<u8>,
    },
}

/// Something only the host can do. The core never touches a desktop, a
/// clipboard or a socket itself.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Effect {
    LockSession,
    UnlockSession,
    QuerySession,
    ClipboardRead,
    ClipboardWrite {
        mime: String,
        data: Vec<u8>,
    },
    ListCommands,
    RunCommand {
        id: String,
    },
    SendMagicPacket {
        macs: Vec<String>,
        dests: Vec<String>,
        port: u16,
    },
    /// Escape hatch for a plugin that needs something this enum does not name,
    /// so adding a plugin never means editing core's vocabulary. A host that
    /// does not recognise `ns` answers [`EffectResult::Unsupported`], and the
    /// core then omits that plugin's capabilities from the handshake.
    Custom {
        ns: String,
        verb: String,
        payload: Vec<u8>,
    },
}

/// Coarse classes of effect, declared by a host at construction.
///
/// A plugin lists what it requires; a host that cannot provide it has that
/// plugin disabled and its capabilities left out of the handshake. That is how
/// iOS and Linux register the identical plugin set and simply negotiate down,
/// rather than growing a `#[cfg]` forest.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum EffectKind {
    Session,
    Clipboard,
    Command,
    Wol,
    Custom,
}

impl Effect {
    #[must_use]
    pub fn kind(&self) -> EffectKind {
        match self {
            Self::LockSession | Self::UnlockSession | Self::QuerySession => EffectKind::Session,
            Self::ClipboardRead | Self::ClipboardWrite { .. } => EffectKind::Clipboard,
            Self::ListCommands | Self::RunCommand { .. } => EffectKind::Command,
            Self::SendMagicPacket { .. } => EffectKind::Wol,
            Self::Custom { .. } => EffectKind::Custom,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EffectResult {
    /// Opaque to the core; the plugin that asked knows how to read it.
    Ok(Vec<u8>),
    /// The host tried and could not.
    Failed(String),
    /// The host does not implement this effect at all. Distinct from `Failed`:
    /// it is a static property of the host, not a transient failure.
    Unsupported,
}

/// Anything a UI or CLI should show.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum UiEvent {
    PairingWindowOpen {
        code: String,
        expires_in_ms: u64,
    },
    /// Both ends show this. The user compares, then answers with
    /// [`LocalCommand::ConfirmPairing`].
    PairingSas {
        name: String,
        fingerprint: Fingerprint,
        sas: String,
    },
    PairingComplete {
        peer: DeviceId,
        name: String,
    },
    PairingFailed {
        reason: String,
    },
    PeerReachable {
        peer: DeviceId,
        name: String,
    },
    PeerUnreachable {
        peer: DeviceId,
    },
    /// A plugin has something to say to the local UI.
    Plugin {
        peer: DeviceId,
        cap: String,
        ty: String,
        body: Vec<u8>,
    },
    Error {
        code: ErrorCode,
        detail: String,
    },
}

/// Where a persisted value should live. The core does no IO, so it says what a
/// value is and lets the host decide where that belongs: Keychain on iOS, a
/// `0600` file on Linux.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sensitivity {
    /// Key material. Never a plain file, never a log, never a backup.
    Secret,
    /// Peer records, watermarks, settings.
    Ordinary,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    Dial {
        transport: TransportId,
        addr: String,
        dial: DialToken,
    },
    LinkSend {
        link: LinkId,
        msg: Vec<u8>,
    },
    Close {
        link: LinkId,
        reason: LinkDownReason,
    },
    Effect {
        token: EffectToken,
        effect: Effect,
    },
    Persist {
        key: String,
        value: Option<Vec<u8>>,
        sensitivity: Sensitivity,
    },
    /// Start or stop advertising ourselves over a transport's discovery.
    Advertise {
        transport: TransportId,
        enable: bool,
        txt: Vec<(String, String)>,
    },
    Discover {
        transport: TransportId,
        enable: bool,
    },
    Ui(UiEvent),
}

/// What one call to `Core::handle` produced.
#[derive(Default, Debug)]
pub struct Outcome {
    pub actions: Vec<Action>,
    /// Absolute monotonic milliseconds at which the host should deliver
    /// [`Event::Tick`], or `None` for "no timer needed".
    ///
    /// Deliberately a single deadline rather than `SetTimer`/`CancelTimer`
    /// actions carrying tokens: timer identifiers that cross a host boundary
    /// leak and desync, and every host then has to reimplement a timer wheel
    /// correctly. One deadline, re-armed on every outcome, is quinn's model and
    /// it moves the bookkeeping to the side that can actually test it.
    pub next_deadline_ms: Option<u64>,
}

impl Outcome {
    pub(crate) fn push(&mut self, a: Action) {
        self.actions.push(a);
    }

    pub(crate) fn ui(&mut self, e: UiEvent) {
        self.actions.push(Action::Ui(e));
    }
}

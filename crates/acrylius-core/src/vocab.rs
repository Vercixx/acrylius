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

/// One bulk transfer, for as long as it lasts.
///
/// Allocated by the host that starts it, and carried in the envelope so the
/// other end can name it. Unique within a session, which is all the key
/// derivation needs.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TransferId(pub u64);

/// The half of the range the core mints from, leaving the rest to hosts that
/// number their own sends. See [`crate::plugin::Cx::new_transfer`].
pub const MINTED_HERE: u64 = 1 << 63;

impl TransferId {
    /// The number to show a person, and the one they will type back.
    ///
    /// [`MINTED_HERE`] is there so an id minted by the core cannot collide with
    /// one a host numbered itself, which matters because a transfer is keyed by
    /// this alone — a collision cancels the wrong one. It is also nineteen
    /// digits, and `acryliusctl file accept` is a number a person reads off a screen
    /// and retypes. Which half of the range an id came from is not something
    /// they need to know, so it is not shown.
    #[must_use]
    pub fn short(self) -> u64 {
        self.0 & !MINTED_HERE
    }

    /// Whether `typed` is a way of writing this id.
    ///
    /// Both forms: a script that captured the full one keeps working, and a
    /// person reading the short one is understood. Matched against the
    /// transfers actually waiting rather than by putting the marker back
    /// blindly, so a number that names nothing is refused instead of quietly
    /// becoming something else.
    #[must_use]
    pub fn written_as(self, typed: u64) -> bool {
        typed == self.0 || typed == self.short()
    }
}

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
    /// A host has somewhere for the other end to connect for a bulk transfer.
    ///
    /// Only the side that can accept connections sends this. A phone cannot,
    /// which is why the endpoint is negotiated rather than assumed.
    BulkListening {
        transfer: TransferId,
        endpoint: String,
    },
    /// The far end has connected, and bytes are on their way.
    ///
    /// The one fact about a transfer that only a host can report, and the core
    /// cannot do without it. Waiting for a sender that never dials has to be
    /// bounded — an accepted offer otherwise holds a port and a reserved
    /// filename for the life of the process — while a file arriving must not be,
    /// because nothing here knows how long a gigabyte should take. From the
    /// outside those two are the same silence. This is what tells them apart.
    ///
    /// A host that never sends it gets the old behaviour for the transfer
    /// itself and keeps the bound wait, which is the safe way round.
    BulkStarted { transfer: TransferId },
    /// A bulk transfer ended, one way or the other. `detail` is empty on
    /// success.
    BulkFinished {
        transfer: TransferId,
        ok: bool,
        detail: String,
    },
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
    /// The network changed; try every peer again from the addresses on file.
    ///
    /// Distinct from `Connect` in what it is allowed to do: this may dial a
    /// peer that is *already reachable*, when a better transport has become
    /// possible. Going the other way — Bluetooth to Wi-Fi — depends on a fresh
    /// sighting, and a host has no way to make one happen; mDNS resolves a
    /// service once and then says nothing. So a phone that lost Wi-Fi and got
    /// it back stayed on Bluetooth, which cannot carry a file, with a perfectly
    /// good network in the room.
    ///
    /// Host-driven rather than a heartbeat, because the moment is knowable —
    /// iOS reports a path becoming satisfied — and dialling Wi-Fi every few
    /// seconds on the chance it has come back is a radio a phone in a pocket
    /// cannot afford.
    ReconsiderRoutes,
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
    /// What is playing, everywhere on this machine.
    MediaQuery,
    /// Act on a player. An empty `player` means whichever is active — the
    /// host decides what that means, because only it can see them.
    MediaControl {
        player: String,
        action: MediaAction,
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

/// What to do to a player.
///
/// Milliseconds and a whole percent, not seconds and a fraction. Nothing here
/// needs sub-millisecond precision, and integers keep this comparable — which
/// matters because `Effect` is compared in tests and a float would quietly make
/// that impossible.
///
/// Ranges are checked by the plugin before they get here, so a host may pass
/// them on without checking again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MediaAction {
    Play,
    Pause,
    PlayPause,
    Next,
    Previous,
    Stop,
    /// Relative, and may be negative. A skip button means "thirty seconds on
    /// from wherever it is", which needs no agreement about where that is.
    Seek {
        offset_ms: i64,
    },
    SetPosition {
        ms: u64,
    },
    /// 0 to 100.
    SetVolume {
        percent: u8,
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
    Media,
    /// Somewhere to put an incoming file.
    ///
    /// Receiving only, and that asymmetry is the point. A host that can pick a
    /// file can offer it — there is nothing to gate, because it starts the
    /// transfer and reads the bytes itself. Accepting one means a directory, a
    /// listening socket and a person to ask, which is three things rather than
    /// a capability, and any host that has all three may declare this.
    ///
    /// A phone does now, which it did not always: files land in the app's
    /// Documents directory, it binds a port for one transfer, and a person taps
    /// Accept. What it still cannot do is any of that with the app closed — but
    /// that is a reason for the app to say so, not for the core to decide on
    /// its behalf.
    Share,
    Custom,
}

/// What a host can actually carry out, as a set.
///
/// A plugin registers on every device and negotiates down, so "can this machine
/// do the thing behind this capability" is a question it has to be able to ask
/// — a phone has no desktop session to lock and nowhere to put a file, and
/// refusing a request it can never serve is better than accepting one and
/// leaving the far end waiting for an answer that is not coming.
///
/// A bitmask rather than a slice because a `Cx` is built for every message and
/// carrying this must not mean an allocation for every message.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct EffectSet(u16);

impl EffectSet {
    #[must_use]
    pub fn new(kinds: impl IntoIterator<Item = EffectKind>) -> Self {
        Self(kinds.into_iter().fold(0, |set, k| set | Self::bit(k)))
    }

    /// Everything. For a host that has not said otherwise, and for tests that
    /// are not about this.
    #[must_use]
    pub fn all() -> Self {
        Self(u16::MAX)
    }

    #[must_use]
    pub fn contains(self, kind: EffectKind) -> bool {
        self.0 & Self::bit(kind) != 0
    }

    const fn bit(kind: EffectKind) -> u16 {
        1 << (kind as u16)
    }
}

impl Effect {
    #[must_use]
    pub fn kind(&self) -> EffectKind {
        match self {
            Self::LockSession | Self::UnlockSession | Self::QuerySession => EffectKind::Session,
            Self::ClipboardRead | Self::ClipboardWrite { .. } => EffectKind::Clipboard,
            Self::ListCommands | Self::RunCommand { .. } => EffectKind::Command,
            Self::SendMagicPacket { .. } => EffectKind::Wol,
            Self::MediaQuery | Self::MediaControl { .. } => EffectKind::Media,
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
    /// A device nearby that this one is not paired with.
    ///
    /// Untrusted, like everything discovery says: it supplies a name to show
    /// and an address to try, and nothing may be decided from either. The
    /// handshake is what settles who is actually there.
    ///
    /// Only unpaired devices. A paired one is already in `peers` with somewhere
    /// to display it, and the core dials it without being asked — so reporting
    /// it here would be a second list of the same machine that does nothing.
    Discovered {
        fingerprint: Fingerprint,
        name: String,
        /// Transport-defined and opaque: a `host:port`, a BLE address.
        addr: String,
        transport: TransportId,
        /// Whether it says it has a pairing window open right now.
        pairing: bool,
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
        /// Which peer this is about, when it is about one.
        ///
        /// A host that correlates a request with its answer has to be able to
        /// tell "the peer you asked about refused" from "something else went
        /// wrong elsewhere". Without this the control socket reported any
        /// core-level error anywhere as the refusal of whatever request
        /// happened to be waiting — and with a `share` request waiting an hour,
        /// that window is an hour long.
        ///
        /// `None` for errors that belong to the machine rather than to a
        /// conversation with somebody.
        peer: Option<DeviceId>,
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
    /// Accept a bulk connection for `transfer`, and say where.
    ///
    /// The host answers with [`Event::BulkListening`] once it has somewhere,
    /// [`Event::BulkStarted`] when the far end actually connects, and
    /// [`Event::BulkFinished`] when the transfer ends. A host that cannot listen
    /// reports that as a finished-and-failed transfer rather than staying
    /// silent.
    BulkListen {
        transfer: TransferId,
        /// What the *sender* calls this transfer, which is a different number.
        ///
        /// The greeting on the bulk socket is written by the dialer, and a
        /// dialer only knows its own numbering — so this, not `transfer`, is
        /// what a listener must check that greeting against. Getting it wrong
        /// does not fail politely: the listener rejects the one connection it
        /// was waiting for, and the sender sees the socket close on it.
        offered_as: u64,
        /// Derived from the session. The core is the only thing that knows the
        /// session secret; the host gets a scoped, single-use key and nothing
        /// else.
        key: Vec<u8>,
        /// What the far end says it is sending, so a host can decide whether it
        /// wants it before anything arrives.
        expect_bytes: u64,
    },
    /// Connect to `endpoint` and stream the bytes for `transfer`.
    BulkSend {
        transfer: TransferId,
        endpoint: String,
        key: Vec<u8>,
    },
    /// Stop a transfer that has not finished.
    BulkCancel {
        transfer: TransferId,
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

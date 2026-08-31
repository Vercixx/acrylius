//! The Event / Action vocabulary — the normative host seam. There is no trait
//! to implement and nothing to link against.
//!
//! Host rule: actions run on a single serial executor, results come back as
//! events, and `handle()` is never called from inside an action handler.

use crate::link::{LinkAttrs, LinkDownReason, LinkId, TransportId};
use crate::proto::envelope::ErrorCode;
use crate::proto::ids::{DeviceId, Fingerprint};

/// The two clocks the core needs; a struct rather than two arguments so they
/// cannot be transposed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Now {
    /// Deadlines only. Must not jump when the system clock is corrected, or a
    /// pairing window could be extended by changing the time.
    pub monotonic_ms: u64,
    /// Milliseconds since the Unix epoch. Only for the handshake timestamp two
    /// devices compare against each other; nothing local may depend on it.
    pub wall_ms: u64,
}

/// Correlates an [`Action::Effect`] with the [`Event::EffectDone`] answering it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct EffectToken(pub u64);

/// One bulk transfer. Allocated by the host that starts it; unique within a
/// session, which is all the key derivation needs.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TransferId(pub u64);

/// The half of the range the core mints from, leaving the rest to hosts that
/// number their own sends. See [`crate::plugin::Cx::new_transfer`].
pub const MINTED_HERE: u64 = 1 << 63;

impl TransferId {
    /// The number to show a person, without the [`MINTED_HERE`] marker they
    /// would otherwise have to retype.
    #[must_use]
    pub fn short(self) -> u64 {
        self.0 & !MINTED_HERE
    }

    /// Whether `typed` is a way of writing this id, full or short form.
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
    /// A hint for matching against a known peer; the handshake settles it.
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
    /// Something discovery had found is no longer there. By address, because
    /// mDNS withdraws an instance, not a fingerprint.
    Undiscovered {
        transport: TransportId,
        addr: String,
    },
    /// The single host timer fired. See [`Outcome::next_deadline_ms`].
    Tick,
    /// A host has somewhere for the other end to connect for a bulk transfer.
    /// Only the side that can accept connections sends this.
    BulkListening {
        transfer: TransferId,
        endpoint: String,
    },
    /// The far end has connected and bytes are on their way. Ends the bounded
    /// wait for a sender that never dials; the transfer itself is unbounded.
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
    /// Dial `addr` and try to pair with whatever answers. The address decides
    /// where to knock and nothing else; the SAS settles who answered.
    RequestPairing {
        transport: TransportId,
        addr: String,
    },
    /// Answer the SAS prompt. `false` means the digits did not match — the one
    /// observable sign of a relayed handshake, not a typo.
    ConfirmPairing {
        accept: bool,
    },
    /// Tell the core where a peer can be reached, bypassing discovery.
    SetPeerAddress {
        peer: DeviceId,
        transport: TransportId,
        addr: String,
    },
    Connect {
        peer: DeviceId,
    },
    /// The network changed; try every peer again from the addresses on file.
    /// Unlike `Connect`, this may dial a peer that is already reachable, when
    /// a better transport has become possible.
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
    /// Escape hatch so adding a plugin never means editing this enum. A host
    /// that does not recognise `ns` answers [`EffectResult::Unsupported`].
    Custom {
        ns: String,
        verb: String,
        payload: Vec<u8>,
    },
}

/// What to do to a player. Integers keep `Effect` comparable (`Eq`); ranges
/// are checked by the plugin, so a host may pass them on unchecked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MediaAction {
    Play,
    Pause,
    PlayPause,
    Next,
    Previous,
    Stop,
    /// Relative, and may be negative.
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

/// Coarse classes of effect, declared by a host at construction. A host that
/// cannot provide what a plugin requires has that plugin's capabilities left
/// out of the handshake; every host registers the identical plugin set.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum EffectKind {
    Session,
    Clipboard,
    Command,
    Wol,
    Media,
    /// Somewhere to put an incoming file. Gates receiving only; offering needs
    /// no capability, since the sender reads its own bytes.
    Share,
    Custom,
}

/// What a host can actually carry out. A bitmask because a `Cx` is built for
/// every message and must not allocate.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct EffectSet(u16);

impl EffectSet {
    #[must_use]
    pub fn new(kinds: impl IntoIterator<Item = EffectKind>) -> Self {
        Self(kinds.into_iter().fold(0, |set, k| set | Self::bit(k)))
    }

    /// For a host that has not said otherwise, and tests not about this.
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
    /// The host does not implement this effect: a static property of the host,
    /// not a transient failure.
    Unsupported,
}

/// Anything a UI or CLI should show.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum UiEvent {
    /// Both ends show this; the person compares, then answers with
    /// [`LocalCommand::ConfirmPairing`]. The comparison is the security
    /// boundary — a UI that skips it has removed the authentication.
    PairingSas {
        name: String,
        fingerprint: Fingerprint,
        sas: String,
    },
    PairingComplete {
        peer: DeviceId,
        name: String,
    },
    /// A peer has been forgotten and its record is gone: the only reliable
    /// moment to redraw the peer list.
    Revoked {
        peer: DeviceId,
    },
    PairingFailed {
        reason: String,
    },
    /// A nearby device this one is not paired with. Untrusted: a name to show
    /// and an address to try, nothing more. Paired devices are not reported
    /// here — they are already in `peers`.
    Discovered {
        fingerprint: Fingerprint,
        name: String,
        /// Transport-defined and opaque: a `host:port`, a BLE address.
        addr: String,
        transport: TransportId,
        /// Whether it says it is already busy pairing with somebody; advisory.
        pairing: bool,
    },
    /// A machine that was on the network is not any more, and should stop being
    /// offered. The counterpart to [`UiEvent::Discovered`], and said only for
    /// devices that were reported through it.
    Undiscovered {
        fingerprint: Fingerprint,
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
        /// Which peer this is about, when it is about one; `None` for errors
        /// belonging to the machine rather than a conversation with somebody.
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
    /// A single re-armed deadline rather than `SetTimer`/`CancelTimer` actions
    /// with tokens, which would leak and desync across a host boundary.
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

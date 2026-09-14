//! The FFI mirror of the core's vocabulary.
//!
//! Exhaustively-matched, logic-free conversions, kept separate so
//! `acrylius-core` has no dependency on `uniffi`.

use acrylius_core::link as cl;
use acrylius_core::vocab as cv;

// ------------------------------------------------------------------ transport

#[derive(uniffi::Enum, Clone, Debug)]
pub enum FfiTransportKind {
    TcpLan,
    UnixLoopback,
    BleGatt,
    Custom { name: String },
}

// ----------------------------------------------------------------- peer state

/// Whether a peer can be reached, is being reached, or cannot be.
#[derive(uniffi::Enum, Clone, Copy, PartialEq, Eq, Debug)]
pub enum FfiPeerState {
    Unreachable,
    Connecting,
    Reachable,
}

impl From<acrylius_core::peer::PeerState> for FfiPeerState {
    fn from(s: acrylius_core::peer::PeerState) -> Self {
        use acrylius_core::peer::PeerState as P;
        match s {
            P::Unreachable => Self::Unreachable,
            P::Connecting => Self::Connecting,
            P::Reachable => Self::Reachable,
        }
    }
}

/// What is carrying a session, so a UI can show which transport took over.
impl From<cl::TransportKind> for FfiTransportKind {
    fn from(k: cl::TransportKind) -> Self {
        match k {
            cl::TransportKind::TcpLan => Self::TcpLan,
            cl::TransportKind::UnixLoopback => Self::UnixLoopback,
            cl::TransportKind::BleGatt => Self::BleGatt,
            cl::TransportKind::Custom(name) => Self::Custom {
                name: name.to_string(),
            },
        }
    }
}

#[derive(uniffi::Enum, Clone, Copy, Debug)]
pub enum FfiLatency {
    Loopback,
    Lan,
    Ble,
    Wan,
}

#[derive(uniffi::Enum, Clone, Copy, Debug)]
pub enum FfiBulk {
    None,
    SideChannel,
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiLinkAttrs {
    pub transport: u16,
    pub kind: FfiTransportKind,
    pub max_message: u32,
    pub reliable: bool,
    pub ordered: bool,
    pub latency: FfiLatency,
    pub bulk: FfiBulk,
}

impl From<FfiLinkAttrs> for cl::LinkAttrs {
    fn from(a: FfiLinkAttrs) -> Self {
        Self {
            transport: cl::TransportId(a.transport),
            kind: match a.kind {
                FfiTransportKind::TcpLan => cl::TransportKind::TcpLan,
                FfiTransportKind::UnixLoopback => cl::TransportKind::UnixLoopback,
                FfiTransportKind::BleGatt => cl::TransportKind::BleGatt,
                // Leaked deliberately: core's variant needs &'static str; bounded
                // by the fixed set of transports a host defines at startup.
                FfiTransportKind::Custom { name } => {
                    cl::TransportKind::Custom(Box::leak(name.into_boxed_str()))
                }
            },
            max_message: a.max_message,
            reliable: a.reliable,
            ordered: a.ordered,
            latency: match a.latency {
                FfiLatency::Loopback => cl::LatencyClass::Loopback,
                FfiLatency::Lan => cl::LatencyClass::Lan,
                FfiLatency::Ble => cl::LatencyClass::Ble,
                FfiLatency::Wan => cl::LatencyClass::Wan,
            },
            bulk: match a.bulk {
                FfiBulk::None => cl::BulkSupport::None,
                FfiBulk::SideChannel => cl::BulkSupport::SideChannel,
            },
        }
    }
}

/// Attributes of a BLE link, so Swift doesn't get `max_message`/`bulk` wrong
/// (a BLE link cannot carry a side channel).
#[uniffi::export]
#[must_use]
pub fn ble_attrs(transport: u16) -> FfiLinkAttrs {
    let a = cl::LinkAttrs::ble(cl::TransportId(transport));
    FfiLinkAttrs {
        transport,
        kind: FfiTransportKind::BleGatt,
        max_message: a.max_message,
        reliable: a.reliable,
        ordered: a.ordered,
        latency: FfiLatency::Ble,
        bulk: FfiBulk::None,
    }
}

/// Mint a link id for a transport's own counter, so a Swift transport calling
/// this with `1, 2, 3…` can't collide with the Rust side doing the same.
#[uniffi::export]
#[must_use]
pub fn link_id(transport: u16, counter: u64) -> u64 {
    cl::LinkId::new(cl::TransportId(transport), counter).0
}

/// Attributes of an ordinary LAN TCP link, so a host doesn't spell them out
/// and get one wrong.
#[uniffi::export]
#[must_use]
pub fn tcp_lan_attrs(transport: u16) -> FfiLinkAttrs {
    let a = cl::LinkAttrs::tcp_lan(cl::TransportId(transport));
    FfiLinkAttrs {
        transport,
        kind: FfiTransportKind::TcpLan,
        max_message: a.max_message,
        reliable: a.reliable,
        ordered: a.ordered,
        latency: FfiLatency::Lan,
        bulk: FfiBulk::SideChannel,
    }
}

#[derive(uniffi::Enum, Clone, Debug)]
pub enum FfiLinkDown {
    Closed,
    Transport { detail: String },
    Protocol { code: String },
}

// ---------------------------------------------------------------------- events

#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiDiscoveredPeer {
    pub fingerprint: Option<String>,
    pub name: String,
    pub addr: String,
    pub pairing: bool,
}

#[derive(uniffi::Enum, Clone, Debug)]
pub enum FfiEvent {
    LinkUp {
        link: u64,
        attrs: FfiLinkAttrs,
        dial: Option<u64>,
    },
    LinkRecv {
        link: u64,
        msg: Vec<u8>,
    },
    LinkDown {
        link: u64,
        reason: FfiLinkDown,
    },
    DialFailed {
        dial: u64,
        reason: String,
    },
    Discovered {
        transport: u16,
        peer: FfiDiscoveredPeer,
    },
    /// Something discovery had found is gone. See
    /// [`acrylius_core::vocab::Event::Undiscovered`].
    Undiscovered {
        transport: u16,
        addr: String,
    },
    Tick,
    /// Dial `addr` and try to pair with whatever answers.
    RequestPairing {
        transport: u16,
        addr: String,
    },
    ConfirmPairing {
        accept: bool,
    },
    SetPeerAddress {
        peer: String,
        transport: u16,
        addr: String,
    },
    Connect {
        peer: String,
    },
    /// The network changed; try every peer again. See
    /// [`acrylius_core::vocab::LocalCommand::ReconsiderRoutes`].
    ReconsiderRoutes,
    Disconnect {
        peer: String,
    },
    Revoke {
        peer: String,
    },
    PluginCommand {
        peer: String,
        cap: String,
        ty: String,
        body: Vec<u8>,
    },
    EffectDone {
        token: u64,
        result: FfiEffectResult,
    },
    BulkListening {
        transfer: u64,
        endpoint: String,
    },
    /// The far end connected — sent between `accept` and `receive`, the only
    /// moment either is known.
    BulkStarted {
        transfer: u64,
    },
    BulkFinished {
        transfer: u64,
        ok: bool,
        detail: String,
    },
}

#[derive(uniffi::Enum, Clone, Debug)]
pub enum FfiEffectResult {
    Ok { data: Vec<u8> },
    Failed { detail: String },
    Unsupported,
}

impl From<FfiEffectResult> for cv::EffectResult {
    fn from(r: FfiEffectResult) -> Self {
        match r {
            FfiEffectResult::Ok { data } => Self::Ok(data),
            FfiEffectResult::Failed { detail } => Self::Failed(detail),
            FfiEffectResult::Unsupported => Self::Unsupported,
        }
    }
}

/// Malformed input (e.g. a device id) is refused here, not turned into a
/// silent no-match lookup.
#[derive(uniffi::Error, Debug, thiserror::Error)]
pub enum FfiError {
    #[error("{detail}")]
    BadInput { detail: String },
    /// Something the host attempted and couldn't do — not the caller's fault:
    /// a missing file, a refused socket, a full disk.
    #[error("{detail}")]
    Effect { detail: String },
}

fn peer(s: &str) -> Result<acrylius_proto::ids::DeviceId, FfiError> {
    acrylius_proto::ids::DeviceId::parse(s).map_err(|e| FfiError::BadInput {
        detail: format!("device id {s:?}: {e}"),
    })
}

impl TryFrom<FfiEvent> for cv::Event {
    type Error = FfiError;

    fn try_from(e: FfiEvent) -> Result<Self, FfiError> {
        use cv::LocalCommand as L;
        Ok(match e {
            FfiEvent::LinkUp { link, attrs, dial } => Self::LinkUp {
                link: cl::LinkId(link),
                attrs: attrs.into(),
                dial: dial.map(cv::DialToken),
            },
            FfiEvent::LinkRecv { link, msg } => Self::LinkRecv {
                link: cl::LinkId(link),
                msg,
            },
            FfiEvent::LinkDown { link, reason } => Self::LinkDown {
                link: cl::LinkId(link),
                reason: match reason {
                    FfiLinkDown::Closed | FfiLinkDown::Protocol { .. } => {
                        cl::LinkDownReason::Closed
                    }
                    FfiLinkDown::Transport { detail } => cl::LinkDownReason::Transport(detail),
                },
            },
            FfiEvent::DialFailed { dial, reason } => Self::DialFailed {
                dial: cv::DialToken(dial),
                reason,
            },
            FfiEvent::Discovered { transport, peer } => Self::Discovered {
                transport: cl::TransportId(transport),
                peer: cv::DiscoveredPeer {
                    fingerprint: peer
                        .fingerprint
                        .and_then(|f| acrylius_proto::ids::Fingerprint::parse(&f).ok()),
                    name: peer.name,
                    addr: peer.addr,
                    pairing: peer.pairing,
                },
            },
            FfiEvent::Undiscovered { transport, addr } => Self::Undiscovered {
                transport: cl::TransportId(transport),
                addr,
            },
            FfiEvent::Tick => Self::Tick,
            FfiEvent::RequestPairing { transport, addr } => Self::Local(L::RequestPairing {
                transport: cl::TransportId(transport),
                addr,
            }),
            FfiEvent::ConfirmPairing { accept } => Self::Local(L::ConfirmPairing { accept }),
            FfiEvent::SetPeerAddress {
                peer: p,
                transport,
                addr,
            } => Self::Local(L::SetPeerAddress {
                peer: peer(&p)?,
                transport: cl::TransportId(transport),
                addr,
            }),
            FfiEvent::Connect { peer: p } => Self::Local(L::Connect { peer: peer(&p)? }),
            FfiEvent::ReconsiderRoutes => Self::Local(L::ReconsiderRoutes),
            FfiEvent::Disconnect { peer: p } => Self::Local(L::Disconnect { peer: peer(&p)? }),
            FfiEvent::Revoke { peer: p } => Self::Local(L::Revoke { peer: peer(&p)? }),
            FfiEvent::PluginCommand {
                peer: p,
                cap,
                ty,
                body,
            } => Self::Local(L::Plugin {
                peer: peer(&p)?,
                cap,
                ty,
                body,
            }),
            FfiEvent::EffectDone { token, result } => Self::EffectDone {
                token: cv::EffectToken(token),
                result: result.into(),
            },
            FfiEvent::BulkListening { transfer, endpoint } => Self::BulkListening {
                transfer: cv::TransferId(transfer),
                endpoint,
            },
            FfiEvent::BulkStarted { transfer } => Self::BulkStarted {
                transfer: cv::TransferId(transfer),
            },
            FfiEvent::BulkFinished {
                transfer,
                ok,
                detail,
            } => Self::BulkFinished {
                transfer: cv::TransferId(transfer),
                ok,
                detail,
            },
        })
    }
}

// --------------------------------------------------------------------- actions

/// What a host can carry out. A plugin whose effect is missing still loads
/// and can still send; it just can't serve.
#[derive(uniffi::Enum, Clone, Copy, Debug)]
pub enum FfiEffectKind {
    Session,
    Clipboard,
    Command,
    Wol,
    Media,
    Share,
    Touchpad,
    Custom,
}

impl From<FfiEffectKind> for cv::EffectKind {
    fn from(k: FfiEffectKind) -> Self {
        match k {
            FfiEffectKind::Session => Self::Session,
            FfiEffectKind::Clipboard => Self::Clipboard,
            FfiEffectKind::Command => Self::Command,
            FfiEffectKind::Wol => Self::Wol,
            FfiEffectKind::Media => Self::Media,
            FfiEffectKind::Share => Self::Share,
            FfiEffectKind::Touchpad => Self::Touchpad,
            FfiEffectKind::Custom => Self::Custom,
        }
    }
}

#[derive(uniffi::Enum, Clone, Debug)]
pub enum FfiEffect {
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
    MediaQuery,
    /// Empty `player` means active; `value` is ms for seek/position, percent
    /// for volume (already range-checked).
    MediaControl {
        player: String,
        verb: String,
        value: i64,
    },
    SendMagicPacket {
        macs: Vec<String>,
        dests: Vec<String>,
        port: u16,
    },
    /// Never reached on a phone, which serves no touchpad; here so the
    /// conversion below stays total.
    Touchpad {
        verb: String,
        order: u32,
        w_mm: u16,
        h_mm: u16,
        points: Vec<FfiTouchPoint>,
    },
    Custom {
        ns: String,
        verb: String,
        payload: Vec<u8>,
    },
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiTouchPoint {
    pub id: u8,
    pub x: u16,
    pub y: u16,
}

impl From<cv::Effect> for FfiEffect {
    fn from(e: cv::Effect) -> Self {
        match e {
            cv::Effect::LockSession => Self::LockSession,
            cv::Effect::UnlockSession => Self::UnlockSession,
            cv::Effect::QuerySession => Self::QuerySession,
            cv::Effect::ClipboardRead => Self::ClipboardRead,
            cv::Effect::ClipboardWrite { mime, data } => Self::ClipboardWrite { mime, data },
            cv::Effect::ListCommands => Self::ListCommands,
            cv::Effect::RunCommand { id } => Self::RunCommand { id },
            // Flattened to a verb + value rather than mirroring the nested
            // enum — one enum to keep in sync, not two.
            cv::Effect::MediaQuery => Self::MediaQuery,
            cv::Effect::MediaControl { player, action } => {
                use cv::MediaAction as A;
                let (verb, value) = match action {
                    A::Play => ("play", 0),
                    A::Pause => ("pause", 0),
                    A::PlayPause => ("playpause", 0),
                    A::Next => ("next", 0),
                    A::Previous => ("previous", 0),
                    A::Stop => ("stop", 0),
                    A::Seek { offset_ms } => ("seek", offset_ms),
                    A::SetPosition { ms } => ("position", i64::try_from(ms).unwrap_or(i64::MAX)),
                    A::SetVolume { percent } => ("volume", i64::from(percent)),
                };
                Self::MediaControl {
                    player,
                    verb: verb.to_string(),
                    value,
                }
            }
            cv::Effect::SendMagicPacket { macs, dests, port } => {
                Self::SendMagicPacket { macs, dests, port }
            }
            cv::Effect::Touchpad { order, op } => {
                use cv::TouchpadOp as T;
                let (verb, w_mm, h_mm, points) = match op {
                    T::Begin { w_mm, h_mm } => ("begin", w_mm, h_mm, Vec::new()),
                    T::Frame { points } => (
                        "frame",
                        0,
                        0,
                        points
                            .into_iter()
                            .map(|p| FfiTouchPoint {
                                id: p.id,
                                x: p.x,
                                y: p.y,
                            })
                            .collect(),
                    ),
                    T::End => ("end", 0, 0, Vec::new()),
                };
                Self::Touchpad {
                    verb: verb.to_string(),
                    order,
                    w_mm,
                    h_mm,
                    points,
                }
            }
            cv::Effect::Custom { ns, verb, payload } => Self::Custom { ns, verb, payload },
        }
    }
}

#[derive(uniffi::Enum, Clone, Debug)]
pub enum FfiUiEvent {
    PairingSas {
        name: String,
        fingerprint: String,
        sas: String,
    },
    PairingComplete {
        peer: String,
        name: String,
    },
    /// A peer has been forgotten. See [`acrylius_core::vocab::UiEvent::Revoked`].
    Revoked {
        peer: String,
    },
    PairingFailed {
        reason: String,
    },
    /// A device nearby that this one is not paired with. See
    /// [`acrylius_core::vocab::UiEvent::Discovered`].
    Discovered {
        fingerprint: String,
        name: String,
        addr: String,
        /// `u16`, like every other transport id across this boundary.
        ///
        /// It was widened to `u32` while nothing read it. A tap now pairs over
        /// whichever transport saw the machine, so this is handed straight back
        /// as `FfiEvent::RequestPairing`'s `transport` — and a type that did not
        /// match its only destination is a cast waiting to be written wrong.
        /// `u16`, like every other transport id across this boundary.
        transport: u16,
        pairing: bool,
    },
    /// A device that was nearby is not any more. See
    /// [`acrylius_core::vocab::UiEvent::Undiscovered`].
    Undiscovered {
        fingerprint: String,
    },
    PeerReachable {
        peer: String,
        name: String,
    },
    PeerUnreachable {
        peer: String,
    },
    Plugin {
        peer: String,
        cap: String,
        ty: String,
        body: Vec<u8>,
    },
    Error {
        /// Which peer this is about, when it is about one. See
        /// [`acrylius_core::vocab::UiEvent::Error`].
        peer: Option<String>,
        code: String,
        detail: String,
    },
}

impl From<cv::UiEvent> for FfiUiEvent {
    fn from(e: cv::UiEvent) -> Self {
        match e {
            cv::UiEvent::Discovered {
                fingerprint,
                name,
                addr,
                transport,
                pairing,
            } => Self::Discovered {
                fingerprint: fingerprint.to_string(),
                name,
                addr,
                transport: transport.0,
                pairing,
            },
            cv::UiEvent::Undiscovered { fingerprint } => Self::Undiscovered {
                fingerprint: fingerprint.to_string(),
            },
            cv::UiEvent::PairingSas {
                name,
                fingerprint,
                sas,
            } => Self::PairingSas {
                name,
                fingerprint: fingerprint.to_string(),
                sas,
            },
            cv::UiEvent::PairingComplete { peer, name } => Self::PairingComplete {
                peer: peer.to_string(),
                name,
            },
            cv::UiEvent::Revoked { peer } => Self::Revoked {
                peer: peer.to_string(),
            },
            cv::UiEvent::PairingFailed { reason } => Self::PairingFailed { reason },
            cv::UiEvent::PeerReachable { peer, name } => Self::PeerReachable {
                peer: peer.to_string(),
                name,
            },
            cv::UiEvent::PeerUnreachable { peer } => Self::PeerUnreachable {
                peer: peer.to_string(),
            },
            cv::UiEvent::Plugin {
                peer,
                cap,
                ty,
                body,
            } => Self::Plugin {
                peer: peer.to_string(),
                cap,
                ty,
                body,
            },
            cv::UiEvent::Error { peer, code, detail } => Self::Error {
                peer: peer.map(|p| p.to_string()),
                code: code.as_str().to_string(),
                detail,
            },
        }
    }
}

/// Where the host must persist a value. `Secret` means the iOS Keychain
/// (`WhenUnlockedThisDeviceOnly`) or a `0600` file on Linux — never a plain
/// file, log, or backup.
#[derive(uniffi::Enum, Clone, Copy, Debug)]
pub enum FfiSensitivity {
    Secret,
    Ordinary,
}

#[derive(uniffi::Enum, Clone, Debug)]
pub enum FfiAction {
    Dial {
        transport: u16,
        addr: String,
        dial: u64,
    },
    LinkSend {
        link: u64,
        msg: Vec<u8>,
    },
    Close {
        link: u64,
    },
    Effect {
        token: u64,
        effect: FfiEffect,
    },
    Persist {
        key: String,
        value: Option<Vec<u8>>,
        sensitivity: FfiSensitivity,
    },
    Advertise {
        transport: u16,
        enable: bool,
        txt: Vec<FfiTxt>,
    },
    Discover {
        transport: u16,
        enable: bool,
    },
    Ui {
        event: FfiUiEvent,
    },
    /// Send a file: host looks up the transfer, calls `bulk_send`. Key comes
    /// from the core (session-derived), never computed by the host.
    BulkSend {
        transfer: u64,
        endpoint: String,
        key: Vec<u8>,
    },
    /// Accept a file. The host binds, answers with `BulkListening` and its
    /// endpoint, then writes what arrives.
    ///
    /// `expect_bytes` is what the far end claims to send, so the host can
    /// decide before anything hits disk.
    BulkListen {
        transfer: u64,
        /// The number the sender will put in its greeting; check the greeting
        /// against this, but key the listener on `transfer`.
        offered_as: u64,
        key: Vec<u8>,
        expect_bytes: u64,
    },
    /// A bulk transfer this host can't carry out; answers with a failed
    /// `BulkFinished` rather than leaving the peer waiting.
    BulkUnsupported {
        transfer: u64,
    },
}

/// UniFFI has no tuple type, so a TXT pair is a record.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiTxt {
    pub key: String,
    pub value: String,
}

impl From<cv::Action> for FfiAction {
    fn from(a: cv::Action) -> Self {
        match a {
            cv::Action::Dial {
                transport,
                addr,
                dial,
            } => Self::Dial {
                transport: transport.0,
                addr,
                dial: dial.0,
            },
            cv::Action::LinkSend { link, msg } => Self::LinkSend { link: link.0, msg },
            cv::Action::Close { link, .. } => Self::Close { link: link.0 },
            cv::Action::Effect { token, effect } => Self::Effect {
                token: token.0,
                effect: effect.into(),
            },
            cv::Action::Persist {
                key,
                value,
                sensitivity,
            } => Self::Persist {
                key,
                value,
                sensitivity: match sensitivity {
                    cv::Sensitivity::Secret => FfiSensitivity::Secret,
                    cv::Sensitivity::Ordinary => FfiSensitivity::Ordinary,
                },
            },
            cv::Action::Advertise {
                transport,
                enable,
                txt,
            } => Self::Advertise {
                transport: transport.0,
                enable,
                txt: txt
                    .into_iter()
                    .map(|(key, value)| FfiTxt { key, value })
                    .collect(),
            },
            cv::Action::Discover { transport, enable } => Self::Discover {
                transport: transport.0,
                enable,
            },
            cv::Action::BulkSend {
                transfer,
                endpoint,
                key,
            } => Self::BulkSend {
                transfer: transfer.0,
                endpoint,
                key,
            },
            // Reached only if the host declared the Share effect — the plugin
            // refuses an offer otherwise.
            cv::Action::BulkListen {
                transfer,
                offered_as,
                key,
                expect_bytes,
            } => Self::BulkListen {
                transfer: transfer.0,
                offered_as,
                key,
                expect_bytes,
            },
            cv::Action::BulkCancel { transfer } => Self::BulkUnsupported {
                transfer: transfer.0,
            },
            cv::Action::Ui(e) => Self::Ui { event: e.into() },
        }
    }
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiOutcome {
    pub actions: Vec<FfiAction>,
    /// Absolute monotonic milliseconds. The host arms one timer and re-arms
    /// it on every outcome.
    pub next_deadline_ms: Option<u64>,
}

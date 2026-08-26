//! The FFI mirror of the core's vocabulary.
//!
//! These types exist only to cross the language boundary. They carry no logic,
//! and every conversion below is a mechanical, exhaustively-matched mapping, so
//! adding a variant to the core is a compile error here rather than a silent
//! omission. That is what keeps this a translation layer and not a second
//! implementation of the protocol.
//!
//! They are mirrored rather than derived directly on the core's types so that
//! `acrylius-core` keeps no dependency on `uniffi`: the core is the crate that
//! must stay trivially testable on a Linux box, and it should not carry a
//! bindings generator to do it.

use acrylius_core::link as cl;
use acrylius_core::vocab as cv;

// ------------------------------------------------------------------ transport

#[derive(uniffi::Enum, Clone, Debug)]
pub enum FfiTransportKind {
    TcpLan,
    UnixLoopback,
    BleL2cap,
    Custom { name: String },
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
                FfiTransportKind::BleL2cap => cl::TransportKind::BleL2cap,
                // Leaked deliberately: the core's variant is `&'static str`, and
                // a host-supplied name cannot be one. Hosts define a bounded set
                // of transports at startup, so this does not grow without bound.
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

/// The attributes of an ordinary LAN TCP link, so a host does not have to spell
/// them out and get one wrong.
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
    Tick,
    OpenPairingWindow {
        code: String,
    },
    RequestPairing {
        transport: u16,
        addr: String,
        code: String,
    },
    ConfirmPairing {
        accept: bool,
    },
    ClosePairingWindow,
    SetPeerAddress {
        peer: String,
        transport: u16,
        addr: String,
    },
    Connect {
        peer: String,
    },
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

/// A device id that failed to parse is refused here rather than turned into a
/// lookup that quietly matches nothing.
#[derive(uniffi::Error, Debug, thiserror::Error)]
pub enum FfiError {
    #[error("{detail}")]
    BadInput { detail: String },
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
            FfiEvent::Tick => Self::Tick,
            FfiEvent::OpenPairingWindow { code } => Self::Local(L::OpenPairingWindow { code }),
            FfiEvent::RequestPairing {
                transport,
                addr,
                code,
            } => Self::Local(L::RequestPairing {
                transport: cl::TransportId(transport),
                addr,
                code,
            }),
            FfiEvent::ConfirmPairing { accept } => Self::Local(L::ConfirmPairing { accept }),
            FfiEvent::ClosePairingWindow => Self::Local(L::ClosePairingWindow),
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

/// What a host can carry out. Declared at construction; a plugin whose effects
/// are missing still loads and can still send, it simply cannot serve.
#[derive(uniffi::Enum, Clone, Copy, Debug)]
pub enum FfiEffectKind {
    Session,
    Clipboard,
    Command,
    Wol,
    Media,
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
    /// An empty `player` means whichever is active. `value` is milliseconds for
    /// a seek or a position and a whole percent for a volume, already
    /// range-checked by the plugin.
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
    Custom {
        ns: String,
        verb: String,
        payload: Vec<u8>,
    },
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
            // Flattened to a verb and one number rather than mirroring the
            // nested enum. A host acting on this switches on a string either
            // way, and a second enum across the boundary would be a second
            // thing to keep in step for no gain.
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
            cv::Effect::Custom { ns, verb, payload } => Self::Custom { ns, verb, payload },
        }
    }
}

#[derive(uniffi::Enum, Clone, Debug)]
pub enum FfiUiEvent {
    PairingWindowOpen {
        code: String,
        expires_in_ms: u64,
    },
    PairingSas {
        name: String,
        fingerprint: String,
        sas: String,
    },
    PairingComplete {
        peer: String,
        name: String,
    },
    PairingFailed {
        reason: String,
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
        code: String,
        detail: String,
    },
}

impl From<cv::UiEvent> for FfiUiEvent {
    fn from(e: cv::UiEvent) -> Self {
        match e {
            cv::UiEvent::PairingWindowOpen {
                code,
                expires_in_ms,
            } => Self::PairingWindowOpen {
                code,
                expires_in_ms,
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
            cv::UiEvent::Error { code, detail } => Self::Error {
                code: code.as_str().to_string(),
                detail,
            },
        }
    }
}

/// Where the host must put a persisted value.
///
/// `Secret` means the iOS Keychain with `WhenUnlockedThisDeviceOnly`, and a
/// `0600` file on Linux. It must never reach a plain file, a log or a backup.
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
    /// A bulk transfer this host has no way to carry out. The host answers with
    /// a failed `BulkFinished`, so the far end is told rather than left waiting.
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
            // Bulk actions do not cross to iOS yet. Sending a file from a phone
            // needs a document picker and receiving one needs somewhere to put
            // it, and neither exists; a host that cannot serve the capability
            // refuses an offer rather than half-honouring it. Mapped rather
            // than ignored so the day it does exist, this is a compile error
            // and not a silent gap.
            cv::Action::BulkListen { transfer, .. } => Self::BulkUnsupported {
                transfer: transfer.0,
            },
            cv::Action::BulkSend { transfer, .. } => Self::BulkUnsupported {
                transfer: transfer.0,
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
    /// Absolute monotonic milliseconds. The host arms exactly one timer and
    /// re-arms it on every outcome.
    pub next_deadline_ms: Option<u64>,
}

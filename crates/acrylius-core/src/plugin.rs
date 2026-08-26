//! The plugin seam.
//!
//! A plugin here is only the protocol half of a feature, the part that is
//! identical on every device and therefore belongs in the shared artifact. The
//! other two thirds live outside: the effector half is whatever the host does
//! for [`Effect`] (zbus on Linux, mostly nothing on iOS), and the UI half is
//! SwiftUI or `acryliusctl`. Keeping the protocol half here is the whole reason
//! there is one implementation rather than five.
//!
//! Plugins cannot do IO. They cannot read a clock. Everything they want to
//! happen goes through [`Cx`], which accumulates intentions that the core turns
//! into actions once the plugin returns. So a plugin is as testable as the core
//! is, and a misbehaving one cannot reach a socket.

use crate::proto::envelope::{Envelope, ErrorBody, ErrorCode};
use crate::proto::ids::DeviceId;
use crate::vocab::{Effect, EffectKind, EffectResult, EffectToken, UiEvent};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PluginManifest {
    /// Reverse-DNS, no version: `"org.acrylius.clipboard"`.
    pub id: &'static str,
    /// Capabilities this side may send. Advertised as `caps_out`.
    pub outgoing: &'static [&'static str],
    /// Capabilities this side can handle. Advertised as `caps_in`.
    pub incoming: &'static [&'static str],
    /// Effects this plugin needs in order to serve requests.
    ///
    /// It does not gate registration or advertising. A device that cannot lock
    /// a session can still ask another one to, and must still be able to
    /// receive the reply, which arrives under the same capability as the
    /// request. A host that cannot serve a verb answers `not_allowed`.
    ///
    /// What a peer can actually do is discovered from what it announces on
    /// connect, not from what it lists in the handshake.
    pub requires: &'static [EffectKind],
}

#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum PluginError {
    #[error("body did not decode")]
    BadBody,
    #[error("refused by policy")]
    NotAllowed,
    #[error("unknown message type {0:?}")]
    UnknownType(String),
    #[error("too large")]
    TooLarge,
    #[error("{0}")]
    Internal(String),
}

impl PluginError {
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::BadBody => ErrorCode::BadBody,
            Self::NotAllowed => ErrorCode::NotAllowed,
            Self::UnknownType(_) => ErrorCode::UnknownType,
            Self::TooLarge => ErrorCode::TooLarge,
            Self::Internal(_) => ErrorCode::Internal,
        }
    }
}

/// A message a plugin wants sent. The core encrypts and frames it after the
/// plugin returns; a plugin never holds a session or a link.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PendingSend {
    pub peer: DeviceId,
    pub cap: String,
    pub ty: String,
    pub body: Vec<u8>,
    pub re: Option<u32>,
}

/// The only way a plugin affects anything.
pub struct Cx {
    now_ms: u64,
    /// Owned rather than borrowed from the core: a `&mut` here would conflict
    /// with the core's own borrow of the plugin it is calling.
    pub(crate) next_token: u64,
    pub(crate) sends: Vec<PendingSend>,
    pub(crate) effects: Vec<(EffectToken, Effect)>,
    pub(crate) ui: Vec<UiEvent>,
    pub(crate) wake_at: Option<u64>,
    /// Bulk transfers this plugin asked to start, filled in with a key by the
    /// core once it knows which session they belong to.
    pub(crate) bulk: Vec<BulkRequest>,
    /// What this host can carry out. See [`Cx::serves`].
    serves: crate::vocab::EffectSet,
    /// What the link to the peer this dispatch concerns can carry, when there
    /// is one and the core knows. See [`Cx::peer_can_carry_bulk`].
    peer_bulk: Option<crate::link::BulkSupport>,
}

/// A plugin asking for a side channel.
///
/// It names a peer and a transfer and nothing else. The key is the core's to
/// supply, because the session secret is the core's alone — a plugin that could
/// derive one could derive any of them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BulkRequest {
    /// Accept a connection for this transfer, and say where.
    Listen {
        peer: DeviceId,
        transfer: crate::vocab::TransferId,
        expect_bytes: u64,
    },
    /// Connect to somewhere the far end named, and send.
    Send {
        peer: DeviceId,
        transfer: crate::vocab::TransferId,
        endpoint: String,
    },
    Cancel {
        transfer: crate::vocab::TransferId,
    },
}

impl Cx {
    pub(crate) fn new(now_ms: u64, next_token: u64, serves: crate::vocab::EffectSet) -> Self {
        Self {
            now_ms,
            next_token,
            sends: Vec::new(),
            effects: Vec::new(),
            ui: Vec::new(),
            wake_at: None,
            bulk: Vec::new(),
            serves,
            peer_bulk: None,
        }
    }

    /// Tell this context what the link to the peer being dispatched for can
    /// carry. Set only where a dispatch concerns one peer.
    pub(crate) fn for_peer_link(mut self, bulk: Option<crate::link::BulkSupport>) -> Self {
        self.peer_bulk = bulk;
        self
    }

    /// Whether a side channel to this peer could carry bytes at all.
    ///
    /// The companion to [`Cx::serves`], and needed for the same reason: that
    /// one answers "can this machine do it", this one answers "can the link
    /// get there". A plugin that offers a file over a link which cannot carry
    /// one leaves the far end waiting on a transfer that will never start,
    /// because the refusal would otherwise happen on the *receiving* side —
    /// when a person there accepts — and never travels back to the sender.
    ///
    /// Unknown counts as yes. Nothing should be blocked because the core has
    /// not learned an answer yet; only a link that has positively said it
    /// carries nothing is refused.
    #[must_use]
    pub fn peer_can_carry_bulk(&self) -> bool {
        self.peer_bulk != Some(crate::link::BulkSupport::None)
    }

    /// Whether this host can carry out an effect, as opposed to only ask
    /// another device for it.
    ///
    /// A plugin registers on every device — that is what makes the plugin set
    /// platform-independent — so this is how one tells "I am on a machine that
    /// cannot do this" from "nobody has asked yet". A request that can never be
    /// served should be refused while the far end is still listening.
    #[must_use]
    pub fn serves(&self, kind: crate::vocab::EffectKind) -> bool {
        self.serves.contains(kind)
    }

    /// Milliseconds on the host's monotonic clock, handed in rather than read.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        self.now_ms
    }

    pub fn send(&mut self, peer: &DeviceId, cap: &str, ty: &str, body: Vec<u8>) {
        self.sends.push(PendingSend {
            peer: peer.clone(),
            cap: cap.to_string(),
            ty: ty.to_string(),
            body,
            re: None,
        });
    }

    /// Reply within the capability of `to`, correlated to its id.
    pub fn reply(&mut self, peer: &DeviceId, to: &Envelope<'_>, ty: &str, body: Vec<u8>) {
        self.sends.push(PendingSend {
            peer: peer.clone(),
            cap: to.cap.to_string(),
            ty: ty.to_string(),
            body,
            re: Some(to.id),
        });
    }

    /// Reply to a request whose envelope is no longer in hand.
    ///
    /// A plugin that asked the host to do something gets the answer back later,
    /// by which time the request is a stored id rather than a borrowed
    /// `Envelope`.
    pub fn send_reply(&mut self, peer: &DeviceId, cap: &str, ty: &str, body: Vec<u8>, re: u32) {
        self.sends.push(PendingSend {
            peer: peer.clone(),
            cap: cap.to_string(),
            ty: ty.to_string(),
            body,
            re: Some(re),
        });
    }

    /// Answer a request with a named error from the closed vocabulary.
    pub fn send_error(&mut self, peer: &DeviceId, cap: &str, re: u32, code: &str, message: &str) {
        let body = minicbor::to_vec(ErrorBody {
            code: code.to_string(),
            message: message.to_string(),
        })
        .unwrap_or_default();
        self.send_reply(peer, cap, "err", body, re);
    }

    pub fn effect(&mut self, e: Effect) -> EffectToken {
        self.next_token += 1;
        let t = EffectToken(self.next_token);
        self.effects.push((t, e));
        t
    }

    pub fn ui(&mut self, e: UiEvent) {
        self.ui.push(e);
    }

    /// Accept a bulk connection for `transfer` and tell the peer where.
    ///
    /// The host answers with an endpoint, or with a failure if it cannot listen
    /// — a phone cannot, which is why an endpoint is negotiated rather than
    /// assumed by whichever side happens to be sending.
    pub fn bulk_listen(
        &mut self,
        peer: &DeviceId,
        transfer: crate::vocab::TransferId,
        expect_bytes: u64,
    ) {
        self.bulk.push(BulkRequest::Listen {
            peer: peer.clone(),
            transfer,
            expect_bytes,
        });
    }

    /// Connect to an endpoint the peer named, and send.
    pub fn bulk_send(
        &mut self,
        peer: &DeviceId,
        transfer: crate::vocab::TransferId,
        endpoint: &str,
    ) {
        self.bulk.push(BulkRequest::Send {
            peer: peer.clone(),
            transfer,
            endpoint: endpoint.to_string(),
        });
    }

    pub fn bulk_cancel(&mut self, transfer: crate::vocab::TransferId) {
        self.bulk.push(BulkRequest::Cancel { transfer });
    }

    /// Ask to be woken no later than `ms` from now. The core folds this into the
    /// single deadline it hands the host.
    pub fn wake_in(&mut self, ms: u64) {
        let at = self.now_ms.saturating_add(ms);
        self.wake_at = Some(self.wake_at.map_or(at, |w| w.min(at)));
    }
}

/// The protocol half of a feature.
pub trait Plugin: Send {
    fn manifest(&self) -> &'static PluginManifest;

    /// A negotiated message arrived. The core has already checked that `cap` was
    /// in the intersection for this direction, so a plugin never has to.
    fn on_message(
        &mut self,
        cx: &mut Cx,
        peer: &DeviceId,
        env: &Envelope<'_>,
    ) -> Result<(), PluginError>;

    fn on_peer_connected(&mut self, cx: &mut Cx, peer: &DeviceId) {
        let _ = (cx, peer);
    }

    fn on_peer_disconnected(&mut self, cx: &mut Cx, peer: &DeviceId) {
        let _ = (cx, peer);
    }

    /// A local UI or CLI addressed this plugin.
    fn on_local(
        &mut self,
        cx: &mut Cx,
        peer: &DeviceId,
        ty: &str,
        body: &[u8],
    ) -> Result<(), PluginError> {
        let _ = (cx, peer, ty, body);
        Err(PluginError::UnknownType(ty.to_string()))
    }

    fn on_effect_result(&mut self, cx: &mut Cx, token: EffectToken, result: &EffectResult) {
        let _ = (cx, token, result);
    }

    /// The host has somewhere for the far end to connect.
    fn on_bulk_listening(
        &mut self,
        cx: &mut Cx,
        transfer: crate::vocab::TransferId,
        endpoint: &str,
    ) {
        let _ = (cx, transfer, endpoint);
    }

    /// A bulk transfer ended. `detail` is empty on success.
    fn on_bulk_finished(
        &mut self,
        cx: &mut Cx,
        transfer: crate::vocab::TransferId,
        ok: bool,
        detail: &str,
    ) {
        let _ = (cx, transfer, ok, detail);
    }

    fn on_tick(&mut self, cx: &mut Cx) {
        let _ = cx;
    }
}

/// Which capability prefix routes to which plugin.
///
/// Matching is on the capability id including its major version, so
/// `org.acrylius.clipboard/2` does not accidentally reach a plugin that only
/// declared `/1`.
#[must_use]
pub fn handles(manifest: &PluginManifest, cap: &str) -> bool {
    manifest.incoming.contains(&cap) || manifest.outgoing.contains(&cap)
}

/// Test scaffolding for plugin authors.
///
/// A plugin is a pure function of its inputs, so testing one needs no core, no
/// sockets and no clock: hand it a `Cx`, call a method, read what it wanted to
/// happen.
#[cfg(test)]
pub(crate) mod harness {
    use super::{BulkRequest, Cx, PendingSend};
    use crate::proto::envelope::Envelope;
    use crate::vocab::{Effect, EffectToken, UiEvent};

    pub struct Ran {
        pub sends: Vec<PendingSend>,
        pub effects: Vec<(EffectToken, Effect)>,
        #[allow(
            dead_code,
            reason = "read by tests that assert what a plugin surfaced locally"
        )]
        pub ui: Vec<UiEvent>,
        #[allow(
            dead_code,
            reason = "read by tests that assert a plugin asked for a side channel"
        )]
        pub bulk: Vec<BulkRequest>,
        pub next_token: u64,
    }

    impl Ran {
        pub fn one_effect(&self) -> &Effect {
            assert_eq!(self.effects.len(), 1, "expected exactly one effect");
            &self.effects[0].1
        }

        pub fn token(&self) -> EffectToken {
            self.effects.first().expect("expected an effect").0
        }

        pub fn sent(&self, ty: &str) -> Option<&PendingSend> {
            self.sends.iter().find(|s| s.ty == ty)
        }
    }

    /// Run one plugin interaction and collect everything it asked for.
    ///
    /// On a host that serves everything. Use [`run_on`] for a plugin whose
    /// behaviour depends on what the machine under it can do.
    pub fn run(next_token: u64, f: impl FnOnce(&mut Cx)) -> Ran {
        run_on(next_token, crate::vocab::EffectSet::all(), f)
    }

    /// The same, on a host that can carry out only the effects named.
    pub fn run_on(
        next_token: u64,
        serves: crate::vocab::EffectSet,
        f: impl FnOnce(&mut Cx),
    ) -> Ran {
        let mut cx = Cx::new(1_000, next_token, serves);
        f(&mut cx);
        Ran {
            sends: cx.sends,
            effects: cx.effects,
            ui: cx.ui,
            bulk: cx.bulk,
            next_token: cx.next_token,
        }
    }

    /// A request as it would arrive over a session.
    pub fn envelope<'a>(id: u32, cap: &'a str, ty: &'a str, body: &'a [u8]) -> Envelope<'a> {
        Envelope::new(id, cap, ty, body)
    }
}

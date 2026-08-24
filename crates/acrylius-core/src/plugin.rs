//! The plugin seam.
//!
//! A plugin here is only the **protocol half** of a feature — the part that is
//! identical on every device and therefore belongs in the shared artifact. The
//! other two thirds live outside: the *effector* half is whatever the host does
//! for [`Effect`] (zbus on Linux, mostly nothing on iOS), and the *UI* half is
//! SwiftUI or `acryliusctl`. Keeping the protocol half here is the whole reason
//! there is one implementation rather than five.
//!
//! Plugins cannot do IO. They cannot read a clock. Everything they want to
//! happen goes through [`Cx`], which accumulates intentions that the core turns
//! into actions once the plugin returns — so a plugin is as testable as the
//! core is, and a misbehaving one cannot reach a socket.

use crate::proto::envelope::{Envelope, ErrorCode};
use crate::proto::ids::DeviceId;
use crate::vocab::{Effect, EffectKind, EffectResult, EffectToken, UiEvent};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PluginManifest {
    /// Reverse-DNS, no version: `"org.acrylius.clipboard"`.
    pub id: &'static str,
    /// Capabilities this side may **send**. Advertised as `caps_out`.
    pub outgoing: &'static [&'static str],
    /// Capabilities this side can **handle**. Advertised as `caps_in`.
    pub incoming: &'static [&'static str],
    /// Effects this plugin cannot work without.
    ///
    /// A host that provides none of these has the plugin disabled and its
    /// capabilities left out of the handshake entirely — which is how iOS and
    /// Linux register the identical plugin set and negotiate down, instead of
    /// growing two divergent plugin lists.
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
/// plugin returns — a plugin never holds a session or a link.
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
}

impl Cx {
    pub(crate) fn new(now_ms: u64, next_token: u64) -> Self {
        Self {
            now_ms,
            next_token,
            sends: Vec::new(),
            effects: Vec::new(),
            ui: Vec::new(),
            wake_at: None,
        }
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

    pub fn effect(&mut self, e: Effect) -> EffectToken {
        self.next_token += 1;
        let t = EffectToken(self.next_token);
        self.effects.push((t, e));
        t
    }

    pub fn ui(&mut self, e: UiEvent) {
        self.ui.push(e);
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

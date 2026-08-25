//! `org.acrylius.clipboard/1` — share the clipboard.
//!
//! The whole difficulty is loops. Two peers that forward every change they see
//! will echo a single paste at each other forever, and the failure is not
//! obvious in testing because it needs two live devices to appear.
//!
//! The fix is to remember hashes. Each side keeps the hash of the last value it
//! set locally and the last it received, and forwards a local change only when
//! it matches neither. `clipboard_flapping_does_not_loop` pins this.
//!
//! The two directions are separately switchable, and that is not only a
//! preference. iOS cannot read its own pasteboard silently: since iOS 16 a
//! programmatic read of content that came from another app raises a system
//! prompt, and only a paste button, the paste menu, or the keyboard shortcut
//! are exempt. `changeCount` can be polled without a prompt, so an iOS host can
//! notice a change and offer a button, but it cannot sync silently. Computer to
//! phone is unaffected.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::plugin::{Cx, Plugin, PluginError, PluginManifest};
use crate::proto::envelope::Envelope;
use crate::proto::ids::DeviceId;
use crate::vocab::{Effect, EffectKind, EffectResult, EffectToken};

pub const CAP: &str = "org.acrylius.clipboard/1";

/// The only type version 1 carries.
pub const TEXT_PLAIN: &str = "text/plain;charset=utf-8";

/// Anything larger is refused rather than truncated. A silently shortened
/// clipboard is worse than one that did not sync.
pub const MAX_INLINE: usize = 128 * 1024;

#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct ClipboardSet {
    #[n(0)]
    pub mime: String,
    #[cbor(n(1), with = "minicbor::bytes")]
    pub data: Vec<u8>,
    /// SHA-256 of `data`. Carried so a receiver need not rehash to compare.
    #[cbor(n(2), with = "minicbor::bytes")]
    pub hash: Vec<u8>,
}

#[must_use]
pub fn hash(data: &[u8]) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().to_vec()
}

static MANIFEST: PluginManifest = PluginManifest {
    id: "org.acrylius.clipboard",
    outgoing: &[CAP],
    incoming: &[CAP],
    requires: &[EffectKind::Clipboard],
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Directions {
    /// Forward what happens here to peers.
    pub send: bool,
    /// Apply what peers send.
    pub receive: bool,
}

impl Default for Directions {
    fn default() -> Self {
        Self {
            send: true,
            receive: true,
        }
    }
}

#[derive(Default)]
pub struct ClipboardPlugin {
    pub directions: Directions,
    connected: Vec<DeviceId>,
    /// The last value this side put on its own clipboard.
    last_local: Option<Vec<u8>>,
    /// The last value a peer sent us.
    last_remote: Option<Vec<u8>>,
    pending: BTreeMap<EffectToken, Option<(DeviceId, u32)>>,
}

impl ClipboardPlugin {
    #[must_use]
    pub fn new(directions: Directions) -> Self {
        Self {
            directions,
            ..Self::default()
        }
    }

    /// Whether a locally observed value is worth telling anyone about.
    fn is_echo(&self, h: &[u8]) -> bool {
        self.last_remote.as_deref() == Some(h) || self.last_local.as_deref() == Some(h)
    }
}

impl Plugin for ClipboardPlugin {
    fn manifest(&self) -> &'static PluginManifest {
        &MANIFEST
    }

    fn on_peer_connected(&mut self, _cx: &mut Cx, peer: &DeviceId) {
        if !self.connected.contains(peer) {
            self.connected.push(peer.clone());
        }
    }

    fn on_peer_disconnected(&mut self, _cx: &mut Cx, peer: &DeviceId) {
        self.connected.retain(|p| p != peer);
    }

    fn on_message(
        &mut self,
        cx: &mut Cx,
        peer: &DeviceId,
        env: &Envelope<'_>,
    ) -> Result<(), PluginError> {
        match env.ty {
            "set" => {
                let msg: ClipboardSet =
                    minicbor::decode(env.body).map_err(|_| PluginError::BadBody)?;
                if !self.directions.receive {
                    return Err(PluginError::NotAllowed);
                }
                if msg.data.len() > MAX_INLINE {
                    return Err(PluginError::TooLarge);
                }
                if msg.mime != TEXT_PLAIN {
                    return Err(PluginError::NotAllowed);
                }
                let h = hash(&msg.data);
                if msg.hash != h {
                    return Err(PluginError::BadBody);
                }
                // Remember before applying. The host will observe this value on
                // its own clipboard a moment later, and must not send it back.
                self.last_remote = Some(h);
                cx.effect(Effect::ClipboardWrite {
                    mime: msg.mime,
                    data: msg.data,
                });
                Ok(())
            }
            "get" => {
                let token = cx.effect(Effect::ClipboardRead);
                self.pending.insert(token, Some((peer.clone(), env.id)));
                Ok(())
            }
            other => Err(PluginError::UnknownType(other.to_string())),
        }
    }

    fn on_local(
        &mut self,
        cx: &mut Cx,
        peer: &DeviceId,
        ty: &str,
        body: &[u8],
    ) -> Result<(), PluginError> {
        match ty {
            // The host noticed a local change. The peer argument is ignored.
            "changed" => {
                if !self.directions.send {
                    return Ok(());
                }
                if body.len() > MAX_INLINE {
                    return Err(PluginError::TooLarge);
                }
                let h = hash(body);
                if self.is_echo(&h) {
                    return Ok(());
                }
                self.last_local = Some(h.clone());
                let msg = ClipboardSet {
                    mime: TEXT_PLAIN.to_string(),
                    data: body.to_vec(),
                    hash: h,
                };
                let Ok(encoded) = minicbor::to_vec(&msg) else {
                    return Err(PluginError::Internal("encode failed".to_string()));
                };
                for p in &self.connected {
                    cx.send(p, CAP, "set", encoded.clone());
                }
                Ok(())
            }
            "get" => {
                cx.send(peer, CAP, "get", Vec::new());
                Ok(())
            }
            other => Err(PluginError::UnknownType(other.to_string())),
        }
    }

    fn on_effect_result(&mut self, cx: &mut Cx, token: EffectToken, result: &EffectResult) {
        let Some(who) = self.pending.remove(&token) else {
            return;
        };
        let Some((peer, request)) = who else { return };
        match result {
            EffectResult::Ok(data) => {
                let msg = ClipboardSet {
                    mime: TEXT_PLAIN.to_string(),
                    data: data.clone(),
                    hash: hash(data),
                };
                self.last_local = Some(msg.hash.clone());
                match minicbor::to_vec(&msg) {
                    Ok(body) => cx.send_reply(&peer, CAP, "set", body, request),
                    Err(_) => cx.send_error(&peer, CAP, request, "internal", "encode failed"),
                }
            }
            EffectResult::Failed(detail) => {
                cx.send_error(&peer, CAP, request, "effect_failed", detail);
            }
            EffectResult::Unsupported => {
                cx.send_error(&peer, CAP, request, "not_allowed", "no clipboard here");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::harness::{envelope, run};

    fn peer() -> DeviceId {
        DeviceId::of(&[2u8; 32])
    }

    fn set_message(text: &str) -> Vec<u8> {
        minicbor::to_vec(ClipboardSet {
            mime: TEXT_PLAIN.to_string(),
            data: text.as_bytes().to_vec(),
            hash: hash(text.as_bytes()),
        })
        .unwrap()
    }

    #[test]
    fn a_local_change_is_offered_to_every_peer() {
        let mut p = ClipboardPlugin::default();
        run(0, |cx| p.on_peer_connected(cx, &peer()));
        let r = run(0, |cx| {
            p.on_local(cx, &peer(), "changed", b"hello").unwrap()
        });
        let sent = r.sent("set").expect("it should be offered");
        let msg: ClipboardSet = minicbor::decode(&sent.body).unwrap();
        assert_eq!(msg.data, b"hello");
        assert_eq!(msg.hash, hash(b"hello"));
    }

    #[test]
    fn clipboard_flapping_does_not_loop() {
        // The failure this prevents needs two live devices to show up, so it is
        // pinned here instead: A tells B, B applies it, B's host then observes
        // the very same value on its own clipboard. B must say nothing.
        let mut a = ClipboardPlugin::default();
        let mut b = ClipboardPlugin::default();
        run(0, |cx| a.on_peer_connected(cx, &peer()));
        run(0, |cx| b.on_peer_connected(cx, &peer()));

        let mut messages = 0;
        let mut value = "first".to_string();

        for round in 0..50 {
            // A's clipboard changed; it offers the value.
            let ra = run(0, |cx| {
                a.on_local(cx, &peer(), "changed", value.as_bytes())
                    .unwrap()
            });
            let Some(sent) = ra.sent("set") else { continue };
            messages += 1;

            // B receives it and writes it to its own clipboard.
            let env = envelope(round, CAP, "set", &sent.body);
            let rb = run(0, |cx| b.on_message(cx, &peer(), &env).unwrap());
            assert!(matches!(rb.one_effect(), Effect::ClipboardWrite { .. }));

            // B's host now observes that value locally. This is the echo.
            let echo = run(0, |cx| {
                b.on_local(cx, &peer(), "changed", value.as_bytes())
                    .unwrap()
            });
            assert!(
                echo.sent("set").is_none(),
                "B echoed back a value it had just been given (round {round})"
            );

            // And A observing its own value must not resend it either.
            let self_echo = run(0, |cx| {
                a.on_local(cx, &peer(), "changed", value.as_bytes())
                    .unwrap()
            });
            assert!(self_echo.sent("set").is_none(), "A resent its own value");

            value = format!("value-{round}");
        }

        // One message per genuinely new value, and nothing else.
        assert_eq!(
            messages, 50,
            "expected exactly one message per distinct value"
        );
    }

    #[test]
    fn oversized_content_is_refused_not_truncated() {
        let mut p = ClipboardPlugin::default();
        let big = vec![b'x'; MAX_INLINE + 1];
        let body = minicbor::to_vec(ClipboardSet {
            mime: TEXT_PLAIN.to_string(),
            data: big.clone(),
            hash: hash(&big),
        })
        .unwrap();
        let env = envelope(1, CAP, "set", &body);
        let r = run(0, |cx| {
            assert_eq!(
                p.on_message(cx, &peer(), &env).unwrap_err(),
                PluginError::TooLarge
            );
        });
        assert!(r.effects.is_empty(), "nothing should reach the clipboard");
    }

    #[test]
    fn a_hash_that_does_not_match_the_data_is_refused() {
        // Cheap, and it catches a peer whose own loop prevention is broken
        // before its bad state becomes ours.
        let mut p = ClipboardPlugin::default();
        let body = minicbor::to_vec(ClipboardSet {
            mime: TEXT_PLAIN.to_string(),
            data: b"hello".to_vec(),
            hash: hash(b"goodbye"),
        })
        .unwrap();
        let env = envelope(1, CAP, "set", &body);
        run(0, |cx| {
            assert_eq!(
                p.on_message(cx, &peer(), &env).unwrap_err(),
                PluginError::BadBody
            );
        });
    }

    #[test]
    fn an_unknown_mime_is_refused() {
        let mut p = ClipboardPlugin::default();
        let body = minicbor::to_vec(ClipboardSet {
            mime: "image/png".to_string(),
            data: b"..".to_vec(),
            hash: hash(b".."),
        })
        .unwrap();
        let env = envelope(1, CAP, "set", &body);
        run(0, |cx| {
            assert_eq!(
                p.on_message(cx, &peer(), &env).unwrap_err(),
                PluginError::NotAllowed
            );
        });
    }

    #[test]
    fn each_direction_can_be_switched_off_independently() {
        let mut receive_only = ClipboardPlugin::new(Directions {
            send: false,
            receive: true,
        });
        run(0, |cx| receive_only.on_peer_connected(cx, &peer()));
        let r = run(0, |cx| {
            receive_only.on_local(cx, &peer(), "changed", b"x").unwrap()
        });
        assert!(r.sent("set").is_none(), "send is off");

        let mut send_only = ClipboardPlugin::new(Directions {
            send: true,
            receive: false,
        });
        let body = set_message("y");
        let env = envelope(1, CAP, "set", &body);
        run(0, |cx| {
            assert_eq!(
                send_only.on_message(cx, &peer(), &env).unwrap_err(),
                PluginError::NotAllowed
            );
        });
    }

    #[test]
    fn get_answers_with_the_current_contents() {
        let mut p = ClipboardPlugin::default();
        let env = envelope(77, CAP, "get", b"");
        let r = run(0, |cx| p.on_message(cx, &peer(), &env).unwrap());
        assert_eq!(r.one_effect(), &Effect::ClipboardRead);
        let r2 = run(r.next_token, |cx| {
            p.on_effect_result(cx, r.token(), &EffectResult::Ok(b"current".to_vec()));
        });
        let sent = r2.sent("set").expect("an answer");
        assert_eq!(sent.re, Some(77));
        let msg: ClipboardSet = minicbor::decode(&sent.body).unwrap();
        assert_eq!(msg.data, b"current");
    }
}

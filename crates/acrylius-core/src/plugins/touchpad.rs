//! `org.acrylius.touchpad/1`: replay a phone's fingers onto a desktop's virtual
//! touchpad, and let libinput and the compositor derive every gesture.

use crate::plugin::{Cx, Plugin, PluginError, PluginManifest};
use crate::proto::envelope::Envelope;
use crate::proto::ids::DeviceId;
use crate::vocab::{Effect, EffectKind, TouchPoint, TouchpadOp, UiEvent};

pub const CAP: &str = "org.acrylius.touchpad/1";

/// Slots on the virtual device. A frame naming more fingers is refused rather
/// than truncated, so a peer cannot quietly lose one.
pub const MAX_POINTS: usize = 10;

/// The payload of `begin`.
#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct Begin {
    #[n(0)]
    pub w_mm: u16,
    #[n(1)]
    pub h_mm: u16,
}

/// One finger, normalised over the surface to `0..=65535`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct Point {
    #[n(0)]
    pub id: u8,
    #[n(1)]
    pub x: u16,
    #[n(2)]
    pub y: u16,
}

/// The payload of `frame`.
#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct Frame {
    #[n(0)]
    pub seq: u32,
    /// Every finger down right now, not a delta.
    #[n(1)]
    pub points: Vec<Point>,
}

static MANIFEST: PluginManifest = PluginManifest {
    id: "org.acrylius.touchpad",
    outgoing: &[CAP],
    incoming: &[CAP],
    requires: &[EffectKind::Touchpad],
};

#[derive(Default)]
pub struct TouchpadPlugin {
    order: u32,
    /// The peer whose stream is currently open, if any: a disconnect from
    /// anyone else must not touch this desktop's pointer.
    owner: Option<DeviceId>,
}

impl Plugin for TouchpadPlugin {
    fn manifest(&self) -> &'static PluginManifest {
        &MANIFEST
    }

    fn on_peer_connected(&mut self, cx: &mut Cx, peer: &DeviceId) {
        // Advertising says the capability is understood; this says it can be
        // served, which is the only thing a remote can draw a button from.
        if cx.serves(EffectKind::Touchpad) {
            cx.send(peer, CAP, "avail", Vec::new());
        }
    }

    fn on_peer_disconnected(&mut self, cx: &mut Cx, peer: &DeviceId) {
        // Only the stream's own peer disconnecting lifts it.
        if self.owner.as_ref() == Some(peer) {
            self.owner = None;
            self.order += 1;
            cx.effect(Effect::Touchpad {
                order: self.order,
                op: TouchpadOp::End,
            });
        }
    }

    fn on_message(
        &mut self,
        cx: &mut Cx,
        peer: &DeviceId,
        env: &Envelope<'_>,
    ) -> Result<(), PluginError> {
        if env.ty == "avail" {
            cx.ui(UiEvent::Plugin {
                peer: peer.clone(),
                cap: CAP.to_string(),
                ty: "avail".to_string(),
                body: Vec::new(),
            });
            return Ok(());
        }

        if !cx.serves(EffectKind::Touchpad) {
            return Err(PluginError::NotAllowed);
        }

        let op = match env.ty {
            "begin" => {
                let b: Begin = minicbor::decode(env.body).map_err(|_| PluginError::BadBody)?;
                if b.w_mm == 0 || b.h_mm == 0 {
                    return Err(PluginError::BadBody);
                }
                self.owner = Some(peer.clone());
                TouchpadOp::Begin {
                    w_mm: b.w_mm,
                    h_mm: b.h_mm,
                }
            }
            "frame" => {
                let f: Frame = minicbor::decode(env.body).map_err(|_| PluginError::BadBody)?;
                if f.points.len() > MAX_POINTS {
                    return Err(PluginError::TooLarge);
                }
                TouchpadOp::Frame {
                    points: f
                        .points
                        .into_iter()
                        .map(|p| TouchPoint {
                            id: p.id,
                            x: p.x,
                            y: p.y,
                        })
                        .collect(),
                }
            }
            "end" => {
                self.owner = None;
                TouchpadOp::End
            }
            other => return Err(PluginError::UnknownType(other.to_string())),
        };

        // No pending entry and no `on_effect_result`: a frame has no reply, so
        // there is nothing to correlate a result with.
        self.order += 1;
        cx.effect(Effect::Touchpad {
            order: self.order,
            op,
        });
        Ok(())
    }

    fn on_local(
        &mut self,
        cx: &mut Cx,
        peer: &DeviceId,
        ty: &str,
        body: &[u8],
    ) -> Result<(), PluginError> {
        match ty {
            "begin" | "frame" | "end" => {
                cx.send(peer, CAP, ty, body.to_vec());
                Ok(())
            }
            other => Err(PluginError::UnknownType(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::harness::{envelope, run, run_on};
    use crate::vocab::{EffectResult, EffectSet};

    fn peer(byte: u8) -> DeviceId {
        DeviceId::of(&[byte; 32])
    }

    #[test]
    fn begin_decodes_dimensions_into_the_effect() {
        let mut p = TouchpadPlugin::default();
        let body = minicbor::to_vec(Begin {
            w_mm: 70,
            h_mm: 150,
        })
        .unwrap();
        let env = envelope(1, CAP, "begin", &body);
        let r = run(0, |cx| p.on_message(cx, &peer(1), &env).unwrap());
        assert_eq!(
            r.one_effect(),
            &Effect::Touchpad {
                order: 1,
                op: TouchpadOp::Begin {
                    w_mm: 70,
                    h_mm: 150
                },
            }
        );
    }

    #[test]
    fn a_frame_needs_no_reply_whatever_the_effect_answers() {
        let mut p = TouchpadPlugin::default();
        let body = minicbor::to_vec(Frame {
            seq: 1,
            points: vec![],
        })
        .unwrap();
        let env = envelope(1, CAP, "frame", &body);
        let r = run(0, |cx| p.on_message(cx, &peer(1), &env).unwrap());
        assert!(r.sends.is_empty());
        for result in [
            EffectResult::Ok(Vec::new()),
            EffectResult::Failed("no".to_string()),
            EffectResult::Unsupported,
        ] {
            let r2 = run(r.next_token, |cx| {
                p.on_effect_result(cx, r.token(), &result);
            });
            assert!(r2.sends.is_empty());
        }
    }

    #[test]
    fn an_empty_points_array_is_still_one_effect_not_zero() {
        let mut p = TouchpadPlugin::default();
        let body = minicbor::to_vec(Frame {
            seq: 1,
            points: vec![],
        })
        .unwrap();
        let env = envelope(1, CAP, "frame", &body);
        let r = run(0, |cx| p.on_message(cx, &peer(1), &env).unwrap());
        assert_eq!(r.effects.len(), 1);
    }

    #[test]
    fn a_frame_naming_more_points_than_slots_is_too_large() {
        let mut p = TouchpadPlugin::default();
        let points = (0..=MAX_POINTS as u8)
            .map(|id| Point { id, x: 0, y: 0 })
            .collect();
        let body = minicbor::to_vec(Frame { seq: 1, points }).unwrap();
        let env = envelope(1, CAP, "frame", &body);
        let r = run(0, |cx| {
            let e = p.on_message(cx, &peer(1), &env).unwrap_err();
            assert_eq!(e, PluginError::TooLarge);
        });
        assert!(r.effects.is_empty());
    }

    #[test]
    fn a_malformed_body_is_bad_body() {
        let mut p = TouchpadPlugin::default();
        let env = envelope(1, CAP, "begin", b"\x00not cbor");
        run(0, |cx| {
            let e = p.on_message(cx, &peer(1), &env).unwrap_err();
            assert_eq!(e, PluginError::BadBody);
        });
    }

    #[test]
    fn an_unknown_verb_is_named_in_the_error() {
        let mut p = TouchpadPlugin::default();
        let env = envelope(1, CAP, "wiggle", b"");
        run(0, |cx| {
            let e = p.on_message(cx, &peer(1), &env).unwrap_err();
            assert_eq!(e, PluginError::UnknownType("wiggle".to_string()));
        });
    }

    #[test]
    fn a_host_that_cannot_serve_refuses_before_decoding() {
        let mut p = TouchpadPlugin::default();
        let env = envelope(1, CAP, "begin", b"garbage");
        let r = run_on(0, EffectSet::new([]), |cx| {
            let e = p.on_message(cx, &peer(1), &env).unwrap_err();
            assert_eq!(e, PluginError::NotAllowed);
        });
        assert!(r.effects.is_empty());
    }

    #[test]
    fn avail_is_announced_on_connect_only_when_served() {
        let mut p = TouchpadPlugin::default();
        let r = run(0, |cx| p.on_peer_connected(cx, &peer(1)));
        assert!(r.sent("avail").is_some());

        let mut p2 = TouchpadPlugin::default();
        let r2 = run_on(0, EffectSet::new([]), |cx| {
            p2.on_peer_connected(cx, &peer(1))
        });
        assert!(r2.sent("avail").is_none());
    }

    #[test]
    fn order_is_strictly_increasing_across_begin_frame_and_end() {
        let mut p = TouchpadPlugin::default();
        let begin = minicbor::to_vec(Begin { w_mm: 1, h_mm: 1 }).unwrap();
        let frame = minicbor::to_vec(Frame {
            seq: 1,
            points: vec![],
        })
        .unwrap();

        let e1 = envelope(1, CAP, "begin", &begin);
        let r1 = run(0, |cx| p.on_message(cx, &peer(1), &e1).unwrap());
        let Effect::Touchpad { order: o1, .. } = r1.one_effect() else {
            panic!("expected a touchpad effect")
        };

        let e2 = envelope(2, CAP, "frame", &frame);
        let r2 = run(r1.next_token, |cx| p.on_message(cx, &peer(1), &e2).unwrap());
        let Effect::Touchpad { order: o2, .. } = r2.one_effect() else {
            panic!("expected a touchpad effect")
        };

        let e3 = envelope(3, CAP, "end", b"");
        let r3 = run(r2.next_token, |cx| p.on_message(cx, &peer(1), &e3).unwrap());
        let Effect::Touchpad { order: o3, .. } = r3.one_effect() else {
            panic!("expected a touchpad effect")
        };

        assert!(o1 < o2 && o2 < o3);
    }

    #[test]
    fn a_disconnect_from_a_stranger_does_not_touch_the_open_stream() {
        let mut p = TouchpadPlugin::default();
        let begin = minicbor::to_vec(Begin { w_mm: 1, h_mm: 1 }).unwrap();
        let e1 = envelope(1, CAP, "begin", &begin);
        run(0, |cx| p.on_message(cx, &peer(1), &e1).unwrap());

        let r = run(0, |cx| p.on_peer_disconnected(cx, &peer(2)));
        assert!(r.effects.is_empty());
    }

    #[test]
    fn a_disconnect_from_the_stream_owner_lifts_every_finger() {
        let mut p = TouchpadPlugin::default();
        let begin = minicbor::to_vec(Begin { w_mm: 1, h_mm: 1 }).unwrap();
        let e1 = envelope(1, CAP, "begin", &begin);
        run(0, |cx| p.on_message(cx, &peer(1), &e1).unwrap());

        let r = run(0, |cx| p.on_peer_disconnected(cx, &peer(1)));
        assert_eq!(
            r.one_effect(),
            &Effect::Touchpad {
                order: 2,
                op: TouchpadOp::End,
            }
        );

        // Disconnecting the same peer twice must not lift an already-closed
        // stream a second time.
        let r2 = run(r.next_token, |cx| p.on_peer_disconnected(cx, &peer(1)));
        assert!(r2.effects.is_empty());
    }

    #[test]
    fn only_one_dimension_being_zero_is_still_a_bad_body() {
        let mut p = TouchpadPlugin::default();
        for body in [
            minicbor::to_vec(Begin { w_mm: 0, h_mm: 5 }).unwrap(),
            minicbor::to_vec(Begin { w_mm: 5, h_mm: 0 }).unwrap(),
        ] {
            let env = envelope(1, CAP, "begin", &body);
            run(0, |cx| {
                let e = p.on_message(cx, &peer(1), &env).unwrap_err();
                assert_eq!(e, PluginError::BadBody);
            });
        }
    }

    #[test]
    fn exactly_max_points_is_accepted_one_more_is_too_large() {
        let mut p = TouchpadPlugin::default();
        let at_max: Vec<_> = (0..MAX_POINTS as u8)
            .map(|id| Point { id, x: 0, y: 0 })
            .collect();
        let body = minicbor::to_vec(Frame {
            seq: 1,
            points: at_max,
        })
        .unwrap();
        let env = envelope(1, CAP, "frame", &body);
        let r = run(0, |cx| p.on_message(cx, &peer(1), &env).unwrap());
        assert_eq!(r.effects.len(), 1);

        let over: Vec<_> = (0..=MAX_POINTS as u8)
            .map(|id| Point { id, x: 0, y: 0 })
            .collect();
        let body2 = minicbor::to_vec(Frame {
            seq: 2,
            points: over,
        })
        .unwrap();
        let env2 = envelope(2, CAP, "frame", &body2);
        run(r.next_token, |cx| {
            let e = p.on_message(cx, &peer(1), &env2).unwrap_err();
            assert_eq!(e, PluginError::TooLarge);
        });
    }

    #[test]
    fn on_local_passes_the_body_straight_through_to_the_peer() {
        let mut p = TouchpadPlugin::default();
        for ty in ["begin", "frame", "end"] {
            let r = run(0, |cx| {
                p.on_local(cx, &peer(1), ty, b"payload").unwrap();
            });
            let sent = r.sent(ty).expect("expected a send");
            assert_eq!(sent.body, b"payload");
        }
    }

    #[test]
    fn on_local_refuses_an_unknown_verb() {
        let mut p = TouchpadPlugin::default();
        run(0, |cx| {
            let e = p.on_local(cx, &peer(1), "wiggle", b"").unwrap_err();
            assert_eq!(e, PluginError::UnknownType("wiggle".to_string()));
        });
    }
}

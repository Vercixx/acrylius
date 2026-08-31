//! `org.acrylius.session/1`: lock and unlock a desktop session.
//!
//! Protocol half only. Two protocol promises: both verbs are idempotent
//! (locking a locked session succeeds with `was_locked = true`), and `locked`
//! in a reply is read back afterwards, never inferred from an exit status.

use std::collections::BTreeMap;

use crate::plugin::{Cx, Plugin, PluginError, PluginManifest};
use crate::proto::envelope::Envelope;
use crate::proto::ids::DeviceId;
use crate::vocab::{Effect, EffectKind, EffectResult, EffectToken, UiEvent};

pub const CAP: &str = "org.acrylius.session/1";

/// How long a host may spend confirming a lock before it answers anyway;
/// lockers act on logind's signal asynchronously.
pub const LOCK_CONFIRM_MS: u64 = 8_000;

/// See [`LOCK_CONFIRM_MS`].
pub const UNLOCK_CONFIRM_MS: u64 = 5_000;

/// Client-side wait: the host's budget plus [`crate::plugin::REPLY_SLACK_MS`].
/// A client waiting less than the host may spend calls a lock that worked a
/// failure, intermittently.
pub const LOCK_REPLY_BUDGET_MS: u64 = LOCK_CONFIRM_MS + crate::plugin::REPLY_SLACK_MS;

/// See [`LOCK_REPLY_BUDGET_MS`].
pub const UNLOCK_REPLY_BUDGET_MS: u64 = UNLOCK_CONFIRM_MS + crate::plugin::REPLY_SLACK_MS;

// The two budgets were once picked independently in two languages, and the
// client gave up before the host was allowed to answer.
const _: () = assert!(
    LOCK_REPLY_BUDGET_MS > LOCK_CONFIRM_MS,
    "a client that gives up before the host may answer reports failures that did not happen"
);
const _: () = assert!(UNLOCK_REPLY_BUDGET_MS > UNLOCK_CONFIRM_MS);

/// The host's answer to [`Effect::QuerySession`], and the payload of `state`.
#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct SessionState {
    #[n(0)]
    pub locked: bool,
    #[n(1)]
    pub session_id: String,
    /// `wayland` or `x11`.
    #[n(2)]
    pub kind: String,
    #[n(3)]
    pub active: bool,
}

/// The host's answer to a lock or unlock, and the payload of `result`.
#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct SessionOutcome {
    #[n(0)]
    pub was_locked: bool,
    /// Read back after the operation, never inferred.
    #[n(1)]
    pub locked: bool,
    #[n(2)]
    pub session_id: String,
}

static MANIFEST: PluginManifest = PluginManifest {
    id: "org.acrylius.session",
    // A device that cannot lock still sends verbs and receives notifications.
    outgoing: &[CAP],
    incoming: &[CAP],
    requires: &[EffectKind::Session],
};

/// What a request was, so its answer can be routed back.
struct Pending {
    peer: DeviceId,
    request: u32,
    /// `result` for lock and unlock, `state` for a query.
    reply: &'static str,
}

#[derive(Default)]
pub struct SessionPlugin {
    pending: BTreeMap<EffectToken, Pending>,
    /// Peers to notify on a state change, which arrives with no peer attached.
    connected: Vec<DeviceId>,
    last: Option<SessionState>,
}

impl SessionPlugin {
    fn broadcast_state(&mut self, cx: &mut Cx, state: &SessionState) {
        self.broadcast_state_except(cx, state, None);
    }

    /// Tell everyone except `already_told`, who is getting it as a reply.
    fn broadcast_state_except(
        &mut self,
        cx: &mut Cx,
        state: &SessionState,
        already_told: Option<&DeviceId>,
    ) {
        let Ok(body) = minicbor::to_vec(state) else {
            return;
        };
        for peer in &self.connected {
            if Some(peer) == already_told {
                continue;
            }
            cx.send(peer, CAP, "state", body.clone());
        }
    }
}

impl Plugin for SessionPlugin {
    fn manifest(&self) -> &'static PluginManifest {
        &MANIFEST
    }

    fn on_peer_connected(&mut self, cx: &mut Cx, peer: &DeviceId) {
        if !self.connected.contains(peer) {
            self.connected.push(peer.clone());
        }
        // Tell a peer where things stand without it having to ask.
        if let Some(state) = self.last.clone()
            && let Ok(body) = minicbor::to_vec(&state)
        {
            cx.send(peer, CAP, "state", body);
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
        let (effect, reply) = match env.ty {
            "query" => (Effect::QuerySession, "state"),
            "lock" => (Effect::LockSession, "result"),
            "unlock" => (Effect::UnlockSession, "result"),
            // Not an error: a peer that only sends may receive and ignore these.
            "state" | "result" => {
                cx.ui(UiEvent::Plugin {
                    peer: peer.clone(),
                    cap: CAP.to_string(),
                    ty: env.ty.to_string(),
                    body: env.body.to_vec(),
                });
                return Ok(());
            }
            other => return Err(PluginError::UnknownType(other.to_string())),
        };
        let token = cx.effect(effect);
        self.pending.insert(
            token,
            Pending {
                peer: peer.clone(),
                request: env.id,
                reply,
            },
        );
        Ok(())
    }

    fn on_local(
        &mut self,
        cx: &mut Cx,
        peer: &DeviceId,
        ty: &str,
        _body: &[u8],
    ) -> Result<(), PluginError> {
        match ty {
            // A broadcast: the host noticed a change; the peer arg is ignored.
            "notify" => {
                let token = cx.effect(Effect::QuerySession);
                self.pending.insert(
                    token,
                    Pending {
                        peer: peer.clone(),
                        request: 0,
                        reply: "broadcast",
                    },
                );
                Ok(())
            }
            "query" | "lock" | "unlock" => {
                cx.send(peer, CAP, ty, Vec::new());
                Ok(())
            }
            other => Err(PluginError::UnknownType(other.to_string())),
        }
    }

    fn on_effect_result(&mut self, cx: &mut Cx, token: EffectToken, result: &EffectResult) {
        let Some(p) = self.pending.remove(&token) else {
            return;
        };
        match result {
            EffectResult::Ok(bytes) => {
                if p.reply == "broadcast" {
                    if let Ok(state) = minicbor::decode::<SessionState>(bytes) {
                        // Only say something when something changed.
                        if self.last.as_ref() != Some(&state) {
                            self.last = Some(state.clone());
                            self.broadcast_state(cx, &state);
                        }
                    }
                    return;
                }
                if p.reply == "state"
                    && let Ok(state) = minicbor::decode::<SessionState>(bytes)
                    && self.last.as_ref() != Some(&state)
                {
                    // A reply is also a fresh reading that updates the dedupe
                    // cache; the other peers must still hear about it.
                    self.last = Some(state.clone());
                    self.broadcast_state_except(cx, &state, Some(&p.peer));
                }
                cx.send_reply(&p.peer, CAP, p.reply, bytes.clone(), p.request);
            }
            // A broadcast's peer is a placeholder for "everyone" and its
            // request id is zero: nobody asked, so nobody is answered.
            EffectResult::Failed(detail) if p.reply != "broadcast" => {
                cx.send_error(&p.peer, CAP, p.request, "effect_failed", detail);
            }
            EffectResult::Unsupported if p.reply != "broadcast" => {
                cx.send_error(
                    &p.peer,
                    CAP,
                    p.request,
                    "not_allowed",
                    "no session on this device",
                );
            }
            EffectResult::Failed(_) | EffectResult::Unsupported => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::harness::{envelope, run};

    fn peer() -> DeviceId {
        DeviceId::of(&[1u8; 32])
    }

    #[test]
    fn lock_asks_the_host_and_answers_the_request() {
        let mut p = SessionPlugin::default();
        let body = Vec::new();
        let env = envelope(42, CAP, "lock", &body);
        let r = run(0, |cx| p.on_message(cx, &peer(), &env).unwrap());
        assert_eq!(r.one_effect(), &Effect::LockSession);

        let outcome = SessionOutcome {
            was_locked: false,
            locked: true,
            session_id: "1".to_string(),
        };
        let bytes = minicbor::to_vec(&outcome).unwrap();
        let r2 = run(r.next_token, |cx| {
            p.on_effect_result(cx, r.token(), &EffectResult::Ok(bytes));
        });
        let sent = r2.sent("result").expect("a result should go back");
        // Correlated to the request, or the caller cannot tell which answer it is.
        assert_eq!(sent.re, Some(42));
        assert_eq!(
            minicbor::decode::<SessionOutcome>(&sent.body).unwrap(),
            outcome
        );
    }

    #[test]
    fn locking_an_already_locked_session_is_a_success() {
        // Idempotence is a protocol promise: `was_locked` passes back as a
        // result, never an error.
        let mut p = SessionPlugin::default();
        let env = envelope(1, CAP, "lock", b"");
        let r = run(0, |cx| p.on_message(cx, &peer(), &env).unwrap());
        let outcome = SessionOutcome {
            was_locked: true,
            locked: true,
            session_id: "1".to_string(),
        };
        let r2 = run(r.next_token, |cx| {
            p.on_effect_result(
                cx,
                r.token(),
                &EffectResult::Ok(minicbor::to_vec(&outcome).unwrap()),
            );
        });
        assert!(r2.sent("result").is_some());
        assert!(r2.sent("err").is_none());
    }

    #[test]
    fn a_host_without_a_session_answers_not_allowed() {
        let mut p = SessionPlugin::default();
        let env = envelope(9, CAP, "unlock", b"");
        let r = run(0, |cx| p.on_message(cx, &peer(), &env).unwrap());
        let r2 = run(r.next_token, |cx| {
            p.on_effect_result(cx, r.token(), &EffectResult::Unsupported);
        });
        let err = r2.sent("err").expect("an error should go back");
        assert_eq!(err.re, Some(9));
    }

    #[test]
    fn an_unknown_verb_is_named_in_the_error() {
        let mut p = SessionPlugin::default();
        let env = envelope(1, CAP, "reboot", b"");
        let r = run(0, |cx| {
            let e = p.on_message(cx, &peer(), &env).unwrap_err();
            assert_eq!(e, PluginError::UnknownType("reboot".to_string()));
        });
        assert!(
            r.effects.is_empty(),
            "an unknown verb must not reach the host"
        );
    }

    #[test]
    fn one_device_asking_does_not_make_every_other_view_stale() {
        // A query that quietly updates the dedupe cache leaves every other
        // device's view stale.
        let mut p = SessionPlugin::default();
        let asker = peer();
        let other = DeviceId::of(&[4u8; 32]);
        run(0, |cx| p.on_peer_connected(cx, &asker));
        run(0, |cx| p.on_peer_connected(cx, &other));

        let state = SessionState {
            locked: true,
            session_id: "1".to_string(),
            kind: "wayland".to_string(),
            active: true,
        };
        let env = envelope(3, CAP, "query", b"");
        let r = run(0, |cx| p.on_message(cx, &asker, &env).unwrap());
        let r2 = run(r.next_token, |cx| {
            p.on_effect_result(
                cx,
                r.token(),
                &EffectResult::Ok(minicbor::to_vec(&state).unwrap()),
            );
        });

        let told: Vec<&DeviceId> = r2
            .sends
            .iter()
            .filter(|s| s.ty == "state")
            .map(|s| &s.peer)
            .collect();
        assert!(
            told.contains(&&other),
            "the device that did not ask still has to be told"
        );
        assert_eq!(
            told.iter().filter(|d| ***d == asker).count(),
            1,
            "and the one that asked hears it once, as its reply"
        );
    }

    #[test]
    fn a_background_poll_that_fails_answers_nobody() {
        // The poll's peer is a placeholder for "everyone" and its request id
        // is zero; a failed poll must answer nobody.
        let mut p = SessionPlugin::default();
        run(0, |cx| p.on_peer_connected(cx, &peer()));

        let r = run(0, |cx| p.on_local(cx, &peer(), "notify", b"").unwrap());
        let r2 = run(r.next_token, |cx| {
            p.on_effect_result(
                cx,
                r.token(),
                &EffectResult::Failed("no graphical session".to_string()),
            );
        });
        assert!(r2.sends.is_empty(), "nothing was asked, so nothing answers");

        // An unsupported host is the same question with a different answer.
        let r3 = run(0, |cx| p.on_local(cx, &peer(), "notify", b"").unwrap());
        let r4 = run(r3.next_token, |cx| {
            p.on_effect_result(cx, r3.token(), &EffectResult::Unsupported);
        });
        assert!(r4.sends.is_empty());

        // A request from a real peer is still answered.
        let env = envelope(5, CAP, "lock", b"");
        let r5 = run(0, |cx| p.on_message(cx, &peer(), &env).unwrap());
        let r6 = run(r5.next_token, |cx| {
            p.on_effect_result(cx, r5.token(), &EffectResult::Unsupported);
        });
        assert_eq!(
            r6.sent("err").map(|s| s.re),
            Some(Some(5)),
            "somebody did ask, so they hear about it"
        );

        // The Failed arm is separate and needs its own assertion.
        let env = envelope(6, CAP, "lock", b"");
        let r7 = run(0, |cx| p.on_message(cx, &peer(), &env).unwrap());
        let r8 = run(r7.next_token, |cx| {
            p.on_effect_result(
                cx,
                r7.token(),
                &EffectResult::Failed("logind said no".to_string()),
            );
        });
        assert_eq!(r8.sent("err").map(|s| s.re), Some(Some(6)));
    }

    #[test]
    fn a_broadcast_skips_the_peer_that_left_and_reaches_the_one_that_stayed() {
        // Mutation testing: `retain`'s `!=` could flip to `==` with no test
        // objecting. Both halves asserted, or an emptied list also passes.
        let mut p = SessionPlugin::default();
        let gone = peer();
        let stayed = DeviceId::of(&[2u8; 32]);
        run(0, |cx| p.on_peer_connected(cx, &gone));
        run(0, |cx| p.on_peer_connected(cx, &stayed));
        run(0, |cx| p.on_peer_disconnected(cx, &gone));

        let state = SessionState {
            locked: true,
            session_id: "1".to_string(),
            kind: "wayland".to_string(),
            active: true,
        };
        let r = run(0, |cx| p.on_local(cx, &gone, "notify", b"").unwrap());
        let r2 = run(r.next_token, |cx| {
            p.on_effect_result(
                cx,
                r.token(),
                &EffectResult::Ok(minicbor::to_vec(&state).unwrap()),
            );
        });

        let told: Vec<&DeviceId> = r2
            .sends
            .iter()
            .filter(|s| s.ty == "state")
            .map(|s| &s.peer)
            .collect();
        assert_eq!(
            told,
            vec![&stayed],
            "exactly the peer still connected, and only it"
        );
    }

    #[test]
    fn a_state_change_is_broadcast_only_when_it_changed() {
        let mut p = SessionPlugin::default();
        run(0, |cx| p.on_peer_connected(cx, &peer()));

        let state = SessionState {
            locked: true,
            session_id: "1".to_string(),
            kind: "wayland".to_string(),
            active: true,
        };
        let bytes = minicbor::to_vec(&state).unwrap();

        let r = run(0, |cx| p.on_local(cx, &peer(), "notify", b"").unwrap());
        let r2 = run(r.next_token, |cx| {
            p.on_effect_result(cx, r.token(), &EffectResult::Ok(bytes.clone()));
        });
        assert!(r2.sent("state").is_some(), "the first observation is news");

        let r3 = run(0, |cx| p.on_local(cx, &peer(), "notify", b"").unwrap());
        let r4 = run(r3.next_token, |cx| {
            p.on_effect_result(cx, r3.token(), &EffectResult::Ok(bytes));
        });
        assert!(
            r4.sent("state").is_none(),
            "an unchanged poll must stay quiet"
        );
    }
}

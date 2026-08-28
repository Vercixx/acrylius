//! `org.acrylius.session/1`: lock and unlock a desktop session.
//!
//! The protocol half only. Deciding which session, and reading back whether
//! it actually locked, is the host's job (see `acrylius-linux`), because both
//! answers depend on logind, the compositor, and which screen locker is
//! running.
//!
//! Two invariants live here rather than in the host, because they are protocol
//! promises and a second host must keep them too:
//!
//! * Both verbs are idempotent. Locking an already-locked session is a success
//!   with `was_locked = true`, not an error.
//! * `locked` in a reply is what the host read back afterwards, never an
//!   inference from an exit status. The lockers that matter act on a signal
//!   asynchronously, so a zero exit says only that the signal was sent.

use std::collections::BTreeMap;

use crate::plugin::{Cx, Plugin, PluginError, PluginManifest};
use crate::proto::envelope::Envelope;
use crate::proto::ids::DeviceId;
use crate::vocab::{Effect, EffectKind, EffectResult, EffectToken, UiEvent};

pub const CAP: &str = "org.acrylius.session/1";

/// How long a host may spend confirming a lock before it answers anyway.
///
/// logind only emits a signal; whether anything acts on it is the screen
/// locker's choice, so the host watches the state until it moves. Locking is
/// given longer than unlocking because a locker that has to tear down a session
/// is slower than one that is handed a password.
pub const LOCK_CONFIRM_MS: u64 = 8_000;

/// See [`LOCK_CONFIRM_MS`].
pub const UNLOCK_CONFIRM_MS: u64 = 5_000;

/// How long a client waits for the answer to a lock before calling it a failure.
///
/// Not a number anyone picked: it is the host's budget plus
/// [`crate::plugin::REPLY_SLACK_MS`]. A client that waits less than the host is
/// allowed to spend will call a lock that worked a failure, and it will do it
/// intermittently, depending on how quick the locker is that day.
pub const LOCK_REPLY_BUDGET_MS: u64 = LOCK_CONFIRM_MS + crate::plugin::REPLY_SLACK_MS;

/// See [`LOCK_REPLY_BUDGET_MS`].
pub const UNLOCK_REPLY_BUDGET_MS: u64 = UNLOCK_CONFIRM_MS + crate::plugin::REPLY_SLACK_MS;

// The bug these constants exist to prevent, refused at compile time rather than
// by a test. `LOCK_CONFIRM` was eight seconds and the phone's wait was eight
// seconds, picked independently in two languages with nothing relating them, so
// the reply could not arrive before the client had stopped listening and a lock
// that worked was reported as a failure. Unlocking only ever worked because its
// host budget happened to be three seconds shorter — luck, not design.
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
    // A device that cannot lock a session still wants to hear about one, so it
    // may send the verbs and receive the notifications.
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
    /// Peers to notify when the state changes. Tracked here because a change
    /// arrives with no peer attached to it.
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
            // A peer that only sends is allowed to receive these and ignore
            // them, so they are not an error.
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
            // `notify` is a broadcast: the host noticed the session state
            // change and the peer argument is ignored.
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
                        // Only say something when something changed. A locker
                        // that reports every poll would otherwise flood the
                        // session.
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
                    // A reply is not a broadcast, but it *is* a fresh reading,
                    // and letting it quietly update the dedupe cache meant the
                    // next poll found nothing changed and told nobody. One
                    // device asking made every other device's view stale, until
                    // something happened to move the state again.
                    self.last = Some(state.clone());
                    self.broadcast_state_except(cx, &state, Some(&p.peer));
                }
                cx.send_reply(&p.peer, CAP, p.reply, bytes.clone(), p.request);
            }
            // Nobody asked, so there is nobody to answer.
            //
            // A broadcast's `peer` is the placeholder the host uses to mean
            // "everyone" and its request id is zero. Answering it sent an `err`
            // addressed to a device that does not exist, every three seconds,
            // for as long as the machine had no session to read — and the core
            // reported each one as a peer it could not reach. The media plugin
            // has guarded this since it was written; this one never did.
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
        // Idempotence is a protocol promise, not host behaviour: the host says
        // was_locked, and the plugin must pass that back as a result rather
        // than turning it into an error.
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
        // The dedupe cache is for broadcasts. A `query` answered only the device
        // that asked, but updated the cache anyway — so the next poll compared
        // the new state against itself, found nothing to say, and left every
        // other paired device showing what it had before.
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
        // The poll is the host talking to itself: its peer is a placeholder for
        // "everyone" and its request id is zero. A machine with no session to
        // read failed one of these every three seconds, and each failure was
        // answered with an `err` addressed to a device that does not exist.
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

        // And the other failure shape, which is a separate arm and so needs
        // saying separately.
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
        // Mutation testing found this: `retain(|p| p != peer)` could be flipped
        // to `==` — keeping only the peer that had just gone and dropping every
        // other — without one test objecting. Both halves are asserted here,
        // because a test that only checks the departed peer is gone passes just
        // as happily when the list has been emptied.
        //
        // This is the guard for "losing one route is not losing the device": a
        // phone that moves from Wi-Fi to Bluetooth must not stop being told
        // things because the link it arrived on died.
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

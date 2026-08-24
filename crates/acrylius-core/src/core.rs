//! The state machine.
//!
//! Everything a device does to another device happens here, and none of it
//! touches the world. `handle()` takes the host's monotonic clock as a
//! parameter, returns [`Outcome`], and that is the entire interface.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::CoreConfig;
use crate::link::{LinkAttrs, LinkDownReason, LinkId, TransportId};
use crate::noise::{Handshake, Identity, Session};
use crate::peer::{PeerRecord, PeerState};
use crate::plugin::{Cx, PendingSend, Plugin};
use crate::proto::envelope::{Envelope, ErrorCode};
use crate::proto::frame::{self, FrameKind};
use crate::proto::handshake::{GreatestSeen, Hello};
use crate::proto::ids::{DeviceId, Fingerprint};
use crate::proto::pairing;
use crate::vocab::{
    Action, DialToken, EffectKind, EffectResult, EffectToken, Event, LocalCommand, Outcome,
    Sensitivity, UiEvent,
};

/// A link that exists but has not said anything yet. We do not know whether it
/// wants to pair or to resume until its first frame arrives.
struct PendingLink {
    attrs: LinkAttrs,
    deadline: u64,
}

struct HandshakingLink {
    hs: Handshake,
    attrs: LinkAttrs,
    deadline: u64,
    /// Set when we dialled with a specific peer in mind.
    expect: Option<DeviceId>,
    /// Retained so an `IKpsk2` responder can replay message 1 into the real
    /// handshake once it has chosen a PSK.
    pairing_flow: bool,
}

struct UpLink {
    session: Session,
    peer: DeviceId,
    /// Our `caps_out` ∩ their `caps_in` — what we may send.
    can_send: Vec<String>,
    /// Their `caps_out` ∩ our `caps_in` — what we will accept.
    can_recv: Vec<String>,
}

enum LinkState {
    Pending(PendingLink),
    Handshaking(Box<HandshakingLink>),
    Up(UpLink),
}

/// A pairing handshake that finished and is waiting on a human.
struct AwaitingConfirm {
    link: LinkId,
    record: PeerRecord,
    sas: String,
}

struct PairingWindow {
    psk: [u8; 32],
    deadline: u64,
    attempts: u8,
    awaiting: Option<AwaitingConfirm>,
}

pub struct Core {
    identity: Identity,
    config: CoreConfig,
    peers: BTreeMap<DeviceId, PeerRecord>,
    links: BTreeMap<LinkId, LinkState>,
    /// Last address discovery offered for a peer. Untrusted, and only ever used
    /// to decide where to dial — never to decide who answered.
    addrs: BTreeMap<DeviceId, (TransportId, String)>,
    pairing: Option<PairingWindow>,
    /// Dials we started for pairing, and the PSK to use when they land.
    pending_pair_dials: BTreeMap<DialToken, [u8; 32]>,
    /// Dials we started to reach a known peer.
    pending_peer_dials: BTreeMap<DialToken, DeviceId>,
    plugins: Vec<Box<dyn Plugin>>,
    /// Which plugin asked for an outstanding effect.
    effect_owner: BTreeMap<EffectToken, usize>,
    caps_out: Vec<String>,
    caps_in: Vec<String>,
    next_token: u64,
    next_dial: u64,
    next_msg_id: u32,
    plugin_wake: Option<u64>,
}

impl Core {
    #[must_use]
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    #[must_use]
    pub fn fingerprint(&self) -> Fingerprint {
        self.identity.fingerprint()
    }

    #[must_use]
    pub fn device_id(&self) -> DeviceId {
        self.identity.device_id()
    }

    pub fn peers(&self) -> impl Iterator<Item = &PeerRecord> {
        self.peers.values()
    }

    /// What we advertise we can send, after disabling plugins the host cannot
    /// serve.
    #[must_use]
    pub fn caps_out(&self) -> &[String] {
        &self.caps_out
    }

    #[must_use]
    pub fn caps_in(&self) -> &[String] {
        &self.caps_in
    }

    /// The code currently awaiting confirmation, if any.
    ///
    /// A UI that was backgrounded and came back needs to re-read this rather
    /// than rely on having caught the `PairingSas` event.
    #[must_use]
    pub fn pending_sas(&self) -> Option<&str> {
        self.pairing
            .as_ref()?
            .awaiting
            .as_ref()
            .map(|a| a.sas.as_str())
    }

    #[must_use]
    pub fn peer_state(&self, peer: &DeviceId) -> PeerState {
        for st in self.links.values() {
            match st {
                LinkState::Up(u) if &u.peer == peer => return PeerState::Reachable,
                LinkState::Handshaking(h) if h.expect.as_ref() == Some(peer) => {
                    return PeerState::Connecting;
                }
                _ => {}
            }
        }
        PeerState::Unreachable
    }

    /// The single entry point.
    pub fn handle(&mut self, now_ms: u64, ev: Event) -> Outcome {
        let mut out = Outcome::default();
        match ev {
            Event::LinkUp { link, attrs, dial } => {
                self.on_link_up(now_ms, link, attrs, dial, &mut out)
            }
            Event::LinkRecv { link, msg } => self.on_link_recv(now_ms, link, &msg, &mut out),
            Event::LinkDown { link, .. } => self.on_link_down(now_ms, link, &mut out),
            Event::DialFailed { dial, reason } => {
                self.pending_pair_dials.remove(&dial);
                if let Some(p) = self.pending_peer_dials.remove(&dial) {
                    out.ui(UiEvent::PeerUnreachable { peer: p });
                } else {
                    out.ui(UiEvent::PairingFailed { reason });
                }
            }
            Event::Discovered { transport, peer } => {
                if let Some(fp) = &peer.fingerprint
                    && let Some(rec) = self
                        .peers
                        .values()
                        .find(|r| r.fingerprint().as_ref() == Some(fp))
                    && let Some(id) = rec.id()
                {
                    self.addrs.insert(id, (transport, peer.addr));
                }
            }
            Event::Tick => self.on_tick(now_ms, &mut out),
            Event::Local(cmd) => self.on_local(now_ms, cmd, &mut out),
            Event::EffectDone { token, result } => {
                self.on_effect_done(now_ms, token, &result, &mut out)
            }
        }
        out.next_deadline_ms = self.next_deadline();
        out
    }

    fn next_deadline(&self) -> Option<u64> {
        let mut best: Option<u64> = self.plugin_wake;
        let mut consider = |d: u64| best = Some(best.map_or(d, |b: u64| b.min(d)));
        if let Some(p) = &self.pairing {
            consider(p.deadline);
        }
        for st in self.links.values() {
            match st {
                LinkState::Pending(p) => consider(p.deadline),
                LinkState::Handshaking(h) => consider(h.deadline),
                LinkState::Up(_) => {}
            }
        }
        best
    }

    fn hello(&self, now_ms: u64) -> Hello {
        Hello {
            v: crate::proto::WIRE_VERSION,
            ts_ms: now_ms,
            device_id: self.device_id().to_string(),
            name: self.config.name.clone(),
            platform: self.config.platform.clone(),
            caps_out: self.caps_out.clone(),
            caps_in: self.caps_in.clone(),
        }
    }
}

// ---------------------------------------------------------------- link events

impl Core {
    fn on_link_up(
        &mut self,
        now_ms: u64,
        link: LinkId,
        attrs: LinkAttrs,
        dial: Option<DialToken>,
        out: &mut Outcome,
    ) {
        let deadline = now_ms + self.config.handshake_timeout_ms;

        // A link we dialled to pair: we speak first, with XXpsk3.
        if let Some(d) = dial
            && let Some(psk) = self.pending_pair_dials.remove(&d)
        {
            match Handshake::pair_initiator(&self.identity, &psk) {
                Ok(mut hs) => {
                    // XX message 1 is unencrypted, so it carries nothing.
                    match hs.write(b"") {
                        Ok(m) => {
                            out.push(Action::LinkSend {
                                link,
                                msg: frame::join(FrameKind::PairHandshake, &m),
                            });
                            self.links.insert(
                                link,
                                LinkState::Handshaking(Box::new(HandshakingLink {
                                    hs,
                                    attrs,
                                    deadline,
                                    expect: None,
                                    pairing_flow: true,
                                })),
                            );
                        }
                        Err(e) => self.fail_link(link, &e.to_string(), out),
                    }
                }
                Err(e) => self.fail_link(link, &e.to_string(), out),
            }
            return;
        }

        // A link we dialled to reach a known peer: IKpsk2, we speak first.
        if let Some(d) = dial
            && let Some(peer) = self.pending_peer_dials.remove(&d)
        {
            self.start_session_initiator(now_ms, link, attrs, peer, deadline, out);
            return;
        }

        // Someone dialled us. We do not know yet what they want.
        self.links
            .insert(link, LinkState::Pending(PendingLink { attrs, deadline }));
    }

    fn start_session_initiator(
        &mut self,
        now_ms: u64,
        link: LinkId,
        attrs: LinkAttrs,
        peer: DeviceId,
        deadline: u64,
        out: &mut Outcome,
    ) {
        let Some(rec) = self.peers.get(&peer) else {
            self.fail_link(link, "no such peer", out);
            return;
        };
        let (Some(pk), Some(psk)) = (rec.public_key_array(), rec.session_psk_array()) else {
            self.fail_link(link, "corrupt peer record", out);
            return;
        };
        match Handshake::session_initiator(&self.identity, &psk, &pk) {
            Ok(mut hs) => {
                // Identity and capabilities only — never a command. IK's first
                // payload is not forward-secret.
                let hello = minicbor::to_vec(self.hello(now_ms)).expect("hello encodes");
                match hs.write(&hello) {
                    Ok(m) => {
                        out.push(Action::LinkSend {
                            link,
                            msg: frame::join(FrameKind::SessionHandshake, &m),
                        });
                        self.links.insert(
                            link,
                            LinkState::Handshaking(Box::new(HandshakingLink {
                                hs,
                                attrs,
                                deadline,
                                expect: Some(peer),
                                pairing_flow: false,
                            })),
                        );
                    }
                    Err(e) => self.fail_link(link, &e.to_string(), out),
                }
            }
            Err(e) => self.fail_link(link, &e.to_string(), out),
        }
    }

    fn on_link_recv(&mut self, now_ms: u64, link: LinkId, msg: &[u8], out: &mut Outcome) {
        let Ok((kind, body)) = frame::split(msg) else {
            self.fail_link(link, "malformed frame", out);
            return;
        };

        match self.links.remove(&link) {
            Some(LinkState::Pending(p)) => self.on_first_frame(now_ms, link, p, kind, body, out),
            Some(LinkState::Handshaking(h)) => {
                self.on_handshake_frame(now_ms, link, h, kind, body, out);
            }
            Some(LinkState::Up(u)) => self.on_transport_frame(now_ms, link, u, kind, body, out),
            None => self.fail_link(link, "frame on an unknown link", out),
        }
    }

    /// The first thing an inbound peer says decides which handshake this is.
    fn on_first_frame(
        &mut self,
        now_ms: u64,
        link: LinkId,
        p: PendingLink,
        kind: FrameKind,
        body: &[u8],
        out: &mut Outcome,
    ) {
        match kind {
            FrameKind::PairHandshake => {
                let Some(window) = &mut self.pairing else {
                    // No window open. Refusing here is what makes a pairing
                    // window a *window* rather than a permanently reachable
                    // endpoint that merely usually rejects you.
                    self.fail_link(link, "no pairing window is open", out);
                    return;
                };
                let psk = window.psk;
                match Handshake::pair_responder(&self.identity, &psk) {
                    Ok(mut hs) => {
                        if hs.read(body).is_err() {
                            self.pairing_attempt_failed(link, out);
                            return;
                        }
                        let hello = minicbor::to_vec(self.hello(now_ms)).expect("hello encodes");
                        match hs.write(&hello) {
                            Ok(m) => {
                                out.push(Action::LinkSend {
                                    link,
                                    msg: frame::join(FrameKind::PairHandshake, &m),
                                });
                                self.links.insert(
                                    link,
                                    LinkState::Handshaking(Box::new(HandshakingLink {
                                        hs,
                                        attrs: p.attrs,
                                        deadline: p.deadline,
                                        expect: None,
                                        pairing_flow: true,
                                    })),
                                );
                            }
                            Err(e) => self.fail_link(link, &e.to_string(), out),
                        }
                    }
                    Err(e) => self.fail_link(link, &e.to_string(), out),
                }
            }
            FrameKind::SessionHandshake => {
                // Learn who is calling so we can choose their PSK, then replay
                // this very message into a real handshake. See
                // `Handshake::session_identify`.
                let Ok(pk) = Handshake::session_identify(&self.identity, body) else {
                    self.fail_link(link, "unreadable session opener", out);
                    return;
                };
                let id = DeviceId::of(&pk);
                let Some(rec) = self.peers.get(&id) else {
                    self.fail_link(link, "unknown device", out);
                    return;
                };
                let Some(psk) = rec.session_psk_array() else {
                    self.fail_link(link, "corrupt peer record", out);
                    return;
                };
                match Handshake::session_responder(&self.identity, &psk) {
                    Ok(mut hs) => match hs.read(body) {
                        Ok(payload) => {
                            if !self.accept_hello(now_ms, &id, &payload, out) {
                                self.fail_link(link, "stale or replayed opener", out);
                                return;
                            }
                            let hello =
                                minicbor::to_vec(self.hello(now_ms)).expect("hello encodes");
                            match hs.write(&hello) {
                                Ok(m) => {
                                    out.push(Action::LinkSend {
                                        link,
                                        msg: frame::join(FrameKind::SessionHandshake, &m),
                                    });
                                    self.finish_session(now_ms, link, hs, p.attrs, id, out);
                                }
                                Err(e) => self.fail_link(link, &e.to_string(), out),
                            }
                        }
                        Err(e) => self.fail_link(link, &e.to_string(), out),
                    },
                    Err(e) => self.fail_link(link, &e.to_string(), out),
                }
            }
            FrameKind::Transport => self.fail_link(link, "transport frame before a handshake", out),
        }
    }
}

// ------------------------------------------------------- handshake completion

impl Core {
    fn on_handshake_frame(
        &mut self,
        now_ms: u64,
        link: LinkId,
        mut h: Box<HandshakingLink>,
        kind: FrameKind,
        body: &[u8],
        out: &mut Outcome,
    ) {
        let expected = if h.pairing_flow {
            FrameKind::PairHandshake
        } else {
            FrameKind::SessionHandshake
        };
        if kind != expected {
            self.fail_link(link, "handshake frame of the wrong kind", out);
            return;
        }
        let payload = match h.hs.read(body) {
            Ok(p) => p,
            Err(e) => {
                if h.pairing_flow {
                    self.pairing_attempt_failed(link, out);
                } else {
                    self.fail_link(link, &e.to_string(), out);
                }
                return;
            }
        };

        // A pairing initiator owes one more message before either side is done.
        if h.pairing_flow && h.hs.is_my_turn() {
            let hello = minicbor::to_vec(self.hello(now_ms)).expect("hello encodes");
            match h.hs.write(&hello) {
                Ok(m) => out.push(Action::LinkSend {
                    link,
                    msg: frame::join(FrameKind::PairHandshake, &m),
                }),
                Err(e) => {
                    self.fail_link(link, &e.to_string(), out);
                    return;
                }
            }
        }

        if !h.hs.is_complete() {
            self.links.insert(link, LinkState::Handshaking(h));
            return;
        }

        if h.pairing_flow {
            self.pairing_completed(link, h, &payload, out);
        } else {
            // A session initiator learns the peer's Hello from message 2.
            let Some(peer) = h.expect.clone() else {
                self.fail_link(link, "session handshake with no expected peer", out);
                return;
            };
            if !self.accept_hello(now_ms, &peer, &payload, out) {
                self.fail_link(link, "stale or replayed response", out);
                return;
            }
            self.finish_session(now_ms, link, h.hs, h.attrs, peer, out);
        }
    }

    /// Validate a peer's Hello and advance its replay watermark.
    fn accept_hello(
        &mut self,
        now_ms: u64,
        peer: &DeviceId,
        payload: &[u8],
        out: &mut Outcome,
    ) -> bool {
        let Ok(hello) = minicbor::decode::<Hello>(payload) else {
            out.ui(UiEvent::Error {
                code: ErrorCode::BadBody,
                detail: "handshake payload did not decode".to_string(),
            });
            return false;
        };
        let Some(rec) = self.peers.get_mut(peer) else {
            return false;
        };
        match hello.check_freshness(now_ms, GreatestSeen(rec.greatest_seen)) {
            Ok(seen) => {
                rec.greatest_seen = seen.0;
                rec.name = hello.name;
                rec.platform = hello.platform;
                rec.caps_out = hello.caps_out;
                rec.caps_in = hello.caps_in;
                let (key, value) = Self::peer_blob(rec);
                out.push(Action::Persist {
                    key,
                    value,
                    sensitivity: Sensitivity::Secret,
                });
                true
            }
            Err(e) => {
                out.ui(UiEvent::Error {
                    code: ErrorCode::NotAllowed,
                    detail: e.to_string(),
                });
                false
            }
        }
    }

    fn finish_session(
        &mut self,
        now_ms: u64,
        link: LinkId,
        hs: Handshake,
        attrs: LinkAttrs,
        peer: DeviceId,
        out: &mut Outcome,
    ) {
        let session = match hs.into_session(&attrs) {
            Ok(s) => s,
            Err(e) => {
                self.fail_link(link, &e.to_string(), out);
                return;
            }
        };
        let Some(rec) = self.peers.get(&peer) else {
            self.fail_link(link, "peer vanished mid-handshake", out);
            return;
        };
        // Directional, and deliberately not symmetric: what we may send is our
        // outgoing set against their incoming set, and vice versa. Conflating
        // the two would let a peer receive a capability it only said it could
        // send.
        let can_send = crate::proto::handshake::negotiate(&self.caps_out, &rec.caps_in);
        let can_recv = crate::proto::handshake::negotiate(&rec.caps_out, &self.caps_in);
        let name = rec.name.clone();

        self.links.insert(
            link,
            LinkState::Up(UpLink {
                session,
                peer: peer.clone(),
                can_send,
                can_recv,
            }),
        );
        out.ui(UiEvent::PeerReachable {
            peer: peer.clone(),
            name,
        });

        let mut cx = Cx::new(now_ms, self.next_token);
        for p in &mut self.plugins {
            p.on_peer_connected(&mut cx, &peer);
        }
        self.next_token = cx.next_token;
        self.drain_cx(cx, out);
    }
}

// --------------------------------------------------------------------- pairing

impl Core {
    fn pairing_completed(
        &mut self,
        link: LinkId,
        h: Box<HandshakingLink>,
        payload: &[u8],
        out: &mut Outcome,
    ) {
        let (Ok(hash), Some(pk)) = (h.hs.handshake_hash(), h.hs.peer_static()) else {
            self.fail_link(link, "handshake completed without a peer key", out);
            return;
        };
        let Ok(hello) = minicbor::decode::<Hello>(payload) else {
            self.pairing_attempt_failed(link, out);
            return;
        };

        let record = PeerRecord {
            device_id: DeviceId::of(&pk).to_string(),
            public_key: pk.to_vec(),
            name: hello.name,
            platform: hello.platform,
            session_psk: pairing::session_psk(&hash).to_vec(),
            // A fresh peer starts at zero; its first session opener sets the mark.
            greatest_seen: 0,
            caps_out: hello.caps_out,
            caps_in: hello.caps_in,
        };
        let sas = pairing::sas(&hash);

        // The handshake is cryptographically done, but nothing is written until
        // a human agrees. Hold the link open in `Pending` so a denial can still
        // close it cleanly.
        self.links.insert(
            link,
            LinkState::Pending(PendingLink {
                attrs: h.attrs,
                deadline: h.deadline,
            }),
        );

        out.ui(UiEvent::PairingSas {
            name: record.name.clone(),
            fingerprint: Fingerprint::of(&pk),
            sas: sas.clone(),
        });
        if let Some(w) = &mut self.pairing {
            w.awaiting = Some(AwaitingConfirm { link, record, sas });
        } else {
            // We initiated; there is no window, so keep the same state inline.
            self.pairing = Some(PairingWindow {
                psk: [0u8; 32],
                deadline: h.deadline,
                attempts: 0,
                awaiting: Some(AwaitingConfirm { link, record, sas }),
            });
        }
    }

    fn confirm_pairing(&mut self, accept: bool, out: &mut Outcome) {
        let Some(w) = &mut self.pairing else { return };
        let Some(a) = w.awaiting.take() else { return };
        if !accept {
            // A refused SAS is a hostile handshake, not a typo: close the link
            // and burn the window rather than inviting another attempt.
            out.push(Action::Close {
                link: a.link,
                reason: LinkDownReason::Protocol(ErrorCode::NotAllowed),
            });
            self.links.remove(&a.link);
            self.pairing = None;
            out.ui(UiEvent::PairingFailed {
                reason: "the codes did not match".to_string(),
            });
            return;
        }
        let Some(id) = a.record.id() else { return };
        let (key, value) = Self::peer_blob(&a.record);
        out.push(Action::Persist {
            key,
            value,
            sensitivity: Sensitivity::Secret,
        });
        out.ui(UiEvent::PairingComplete {
            peer: id.clone(),
            name: a.record.name.clone(),
        });
        self.peers.insert(id, a.record);
        self.pairing = None;
        // The link stays up but unnegotiated. The peer will open a session when
        // it wants one; keeping pairing and session strictly separate means
        // there is exactly one code path that establishes a session.
        out.push(Action::Close {
            link: a.link,
            reason: LinkDownReason::Closed,
        });
        self.links.remove(&a.link);
    }

    fn pairing_attempt_failed(&mut self, link: LinkId, out: &mut Outcome) {
        let burned = if let Some(w) = &mut self.pairing {
            w.attempts += 1;
            w.attempts >= self.config.max_pairing_attempts
        } else {
            false
        };
        self.fail_link(link, "pairing handshake failed", out);
        if burned {
            self.pairing = None;
            out.ui(UiEvent::PairingFailed {
                reason: "too many failed attempts; the window is closed".to_string(),
            });
        }
    }

    fn peer_blob(rec: &PeerRecord) -> (String, Option<Vec<u8>>) {
        (
            format!("peer/{}", rec.device_id),
            Some(minicbor::to_vec(rec).expect("peer record encodes")),
        )
    }
}

// ------------------------------------------------------------ transport frames

impl Core {
    fn on_transport_frame(
        &mut self,
        now_ms: u64,
        link: LinkId,
        mut u: UpLink,
        kind: FrameKind,
        body: &[u8],
        out: &mut Outcome,
    ) {
        if kind != FrameKind::Transport {
            self.fail_link(link, "handshake frame on an established link", out);
            return;
        }
        let plaintext = match u.session.decrypt(body) {
            Ok(p) => p,
            Err(e) => {
                self.fail_link(link, &e.to_string(), out);
                return;
            }
        };
        let peer = u.peer.clone();
        let allowed = u.can_recv.clone();
        self.links.insert(link, LinkState::Up(u));

        let Ok(env) = Envelope::decode(&plaintext) else {
            out.ui(UiEvent::Error {
                code: ErrorCode::BadBody,
                detail: "envelope did not decode".to_string(),
            });
            return;
        };

        // Checked once, here, so no plugin has to remember to. A capability the
        // peer never declared it would send does not reach a handler at all.
        if !allowed.iter().any(|c| c == env.cap) {
            self.send_error(now_ms, &peer, &env, ErrorCode::CapNotNegotiated, out);
            return;
        }

        let Some(idx) = self
            .plugins
            .iter()
            .position(|p| crate::plugin::handles(p.manifest(), env.cap))
        else {
            self.send_error(now_ms, &peer, &env, ErrorCode::CapNotNegotiated, out);
            return;
        };

        let mut cx = Cx::new(now_ms, self.next_token);
        let result = self.plugins[idx].on_message(&mut cx, &peer, &env);
        self.next_token = cx.next_token;
        for (t, _) in &cx.effects {
            self.effect_owner.insert(*t, idx);
        }
        self.drain_cx(cx, out);

        if let Err(e) = result {
            self.send_error(now_ms, &peer, &env, e.code(), out);
        }
    }

    fn send_error(
        &mut self,
        now_ms: u64,
        peer: &DeviceId,
        to: &Envelope<'_>,
        code: ErrorCode,
        out: &mut Outcome,
    ) {
        let body = minicbor::to_vec(code.as_str()).unwrap_or_default();
        self.dispatch_send(
            now_ms,
            PendingSend {
                peer: peer.clone(),
                cap: to.cap.to_string(),
                ty: "err".to_string(),
                body,
                re: Some(to.id),
            },
            out,
        );
    }

    /// Encrypt and emit one plugin message.
    fn dispatch_send(&mut self, _now_ms: u64, s: PendingSend, out: &mut Outcome) {
        let Some((&link, _)) = self
            .links
            .iter()
            .find(|(_, st)| matches!(st, LinkState::Up(u) if u.peer == s.peer))
        else {
            // Unreachable is an ordinary outcome, not an error. On iOS this is
            // simply what a peer is whenever the app is not in the foreground.
            out.ui(UiEvent::PeerUnreachable { peer: s.peer });
            return;
        };
        let Some(LinkState::Up(u)) = self.links.get_mut(&link) else {
            return;
        };

        if !u.can_send.contains(&s.cap) {
            out.ui(UiEvent::Error {
                code: ErrorCode::CapNotNegotiated,
                detail: format!("{} is not negotiated with this peer", s.cap),
            });
            return;
        }

        self.next_msg_id = self.next_msg_id.wrapping_add(1);
        let env = Envelope {
            v: crate::proto::WIRE_VERSION,
            id: self.next_msg_id,
            re: s.re,
            cap: &s.cap,
            ty: &s.ty,
            body: &s.body,
            flags: 0,
            bulk: None,
        };
        let Ok(plaintext) = env.encode() else { return };
        match u.session.encrypt(&plaintext) {
            Ok(ct) => out.push(Action::LinkSend {
                link,
                msg: frame::join(FrameKind::Transport, &ct),
            }),
            Err(e) => {
                out.ui(UiEvent::Error {
                    code: ErrorCode::Internal,
                    detail: e.to_string(),
                });
            }
        }
    }

    fn drain_cx(&mut self, cx: Cx, out: &mut Outcome) {
        let Cx {
            sends,
            effects,
            ui,
            wake_at,
            ..
        } = cx;
        for e in ui {
            out.ui(e);
        }
        for (token, effect) in effects {
            out.push(Action::Effect { token, effect });
        }
        for s in sends {
            self.dispatch_send(0, s, out);
        }
        if let Some(w) = wake_at {
            self.plugin_wake = Some(self.plugin_wake.map_or(w, |p: u64| p.min(w)));
        }
    }
}

// ------------------------------------------------- local commands, effects, tick

impl Core {
    fn on_local(&mut self, now_ms: u64, cmd: LocalCommand, out: &mut Outcome) {
        match cmd {
            LocalCommand::OpenPairingWindow { code } => match pairing::normalize(&code) {
                Ok(norm) => {
                    let deadline = now_ms + self.config.pairing_window_ms;
                    self.pairing = Some(PairingWindow {
                        psk: pairing::psk(&norm),
                        deadline,
                        attempts: 0,
                        awaiting: None,
                    });
                    out.ui(UiEvent::PairingWindowOpen {
                        code: norm,
                        expires_in_ms: self.config.pairing_window_ms,
                    });
                }
                Err(e) => out.ui(UiEvent::PairingFailed {
                    reason: e.to_string(),
                }),
            },
            LocalCommand::RequestPairing {
                transport,
                addr,
                code,
            } => match pairing::normalize(&code) {
                Ok(norm) => {
                    self.next_dial += 1;
                    let d = DialToken(self.next_dial);
                    self.pending_pair_dials.insert(d, pairing::psk(&norm));
                    out.push(Action::Dial {
                        transport,
                        addr,
                        dial: d,
                    });
                }
                Err(e) => out.ui(UiEvent::PairingFailed {
                    reason: e.to_string(),
                }),
            },
            LocalCommand::ConfirmPairing { accept } => self.confirm_pairing(accept, out),
            LocalCommand::ClosePairingWindow => {
                if let Some(w) = self.pairing.take()
                    && let Some(a) = w.awaiting
                {
                    out.push(Action::Close {
                        link: a.link,
                        reason: LinkDownReason::Closed,
                    });
                    self.links.remove(&a.link);
                }
            }
            LocalCommand::Connect { peer } => {
                if self.peer_state(&peer) != PeerState::Unreachable {
                    return;
                }
                let Some((transport, addr)) = self.addrs.get(&peer).cloned() else {
                    out.ui(UiEvent::PeerUnreachable { peer });
                    return;
                };
                self.next_dial += 1;
                let d = DialToken(self.next_dial);
                self.pending_peer_dials.insert(d, peer);
                out.push(Action::Dial {
                    transport,
                    addr,
                    dial: d,
                });
            }
            LocalCommand::Disconnect { peer } => {
                let links: Vec<LinkId> = self
                    .links
                    .iter()
                    .filter(|(_, st)| matches!(st, LinkState::Up(u) if u.peer == peer))
                    .map(|(k, _)| *k)
                    .collect();
                for l in links {
                    out.push(Action::Close {
                        link: l,
                        reason: LinkDownReason::Closed,
                    });
                    self.on_link_down(now_ms, l, out);
                }
            }
            LocalCommand::Revoke { peer } => {
                self.handle_revoke(now_ms, &peer, out);
            }
            LocalCommand::Plugin {
                peer,
                cap,
                ty,
                body,
            } => {
                let Some(idx) = self
                    .plugins
                    .iter()
                    .position(|p| crate::plugin::handles(p.manifest(), &cap))
                else {
                    out.ui(UiEvent::Error {
                        code: ErrorCode::UnknownType,
                        detail: format!("no plugin handles {cap}"),
                    });
                    return;
                };
                let mut cx = Cx::new(now_ms, self.next_token);
                let r = self.plugins[idx].on_local(&mut cx, &peer, &ty, &body);
                self.next_token = cx.next_token;
                for (t, _) in &cx.effects {
                    self.effect_owner.insert(*t, idx);
                }
                self.drain_cx(cx, out);
                if let Err(e) = r {
                    out.ui(UiEvent::Error {
                        code: e.code(),
                        detail: e.to_string(),
                    });
                }
            }
        }
    }

    fn handle_revoke(&mut self, now_ms: u64, peer: &DeviceId, out: &mut Outcome) {
        // Drop the record first, so a race cannot leave a live session against a
        // peer we have decided to forget.
        self.peers.remove(peer);
        self.addrs.remove(peer);
        out.push(Action::Persist {
            key: format!("peer/{peer}"),
            value: None,
            sensitivity: Sensitivity::Secret,
        });
        let links: Vec<LinkId> = self
            .links
            .iter()
            .filter(|(_, st)| matches!(st, LinkState::Up(u) if &u.peer == peer))
            .map(|(k, _)| *k)
            .collect();
        for l in links {
            out.push(Action::Close {
                link: l,
                reason: LinkDownReason::Protocol(ErrorCode::NotPaired),
            });
            self.on_link_down(now_ms, l, out);
        }
    }

    fn on_effect_done(
        &mut self,
        now_ms: u64,
        token: EffectToken,
        result: &EffectResult,
        out: &mut Outcome,
    ) {
        let Some(idx) = self.effect_owner.remove(&token) else {
            return;
        };
        let mut cx = Cx::new(now_ms, self.next_token);
        self.plugins[idx].on_effect_result(&mut cx, token, result);
        self.next_token = cx.next_token;
        for (tok, _) in &cx.effects {
            self.effect_owner.insert(*tok, idx);
        }
        self.drain_cx(cx, out);
    }

    fn on_tick(&mut self, now_ms: u64, out: &mut Outcome) {
        // Pairing window expiry. Measured against the host's monotonic clock,
        // so moving the wall clock cannot extend it.
        if let Some(w) = &self.pairing
            && now_ms >= w.deadline
        {
            let w = self.pairing.take().expect("just checked");
            if let Some(a) = w.awaiting {
                out.push(Action::Close {
                    link: a.link,
                    reason: LinkDownReason::Closed,
                });
                self.links.remove(&a.link);
            }
            out.ui(UiEvent::PairingFailed {
                reason: "the pairing window expired".to_string(),
            });
        }

        let stale: Vec<LinkId> = self
            .links
            .iter()
            .filter(|(_, st)| match st {
                LinkState::Pending(p) => now_ms >= p.deadline,
                LinkState::Handshaking(h) => now_ms >= h.deadline,
                LinkState::Up(_) => false,
            })
            .map(|(k, _)| *k)
            .collect();
        for l in stale {
            self.links.remove(&l);
            out.push(Action::Close {
                link: l,
                reason: LinkDownReason::Closed,
            });
        }

        if let Some(w) = self.plugin_wake
            && now_ms >= w
        {
            self.plugin_wake = None;
            let mut cx = Cx::new(now_ms, self.next_token);
            for p in &mut self.plugins {
                p.on_tick(&mut cx);
            }
            self.next_token = cx.next_token;
            self.drain_cx(cx, out);
        }
    }

    fn on_link_down(&mut self, now_ms: u64, link: LinkId, out: &mut Outcome) {
        if let Some(LinkState::Up(u)) = self.links.remove(&link) {
            out.ui(UiEvent::PeerUnreachable {
                peer: u.peer.clone(),
            });
            let mut cx = Cx::new(now_ms, self.next_token);
            for p in &mut self.plugins {
                p.on_peer_disconnected(&mut cx, &u.peer);
            }
            self.next_token = cx.next_token;
            self.drain_cx(cx, out);
        }
    }

    fn fail_link(&mut self, link: LinkId, detail: &str, out: &mut Outcome) {
        self.links.remove(&link);
        out.push(Action::Close {
            link,
            reason: LinkDownReason::Protocol(ErrorCode::NotAllowed),
        });
        out.ui(UiEvent::Error {
            code: ErrorCode::NotAllowed,
            detail: detail.to_string(),
        });
    }
}

// ---------------------------------------------------------------------- builder

/// Explicit, compile-time plugin registration.
///
/// Deliberately not `inventory`, `ctor` or `linkme`: static linking into an iOS
/// binary with dead-strip enabled removes constructor-registered symbols, and
/// the failure mode is an app that silently has *zero* plugins. There is nothing
/// to gain here from magic.
pub struct CoreBuilder {
    identity: Identity,
    config: CoreConfig,
    effects: BTreeSet<EffectKind>,
    plugins: Vec<Box<dyn Plugin>>,
    peers: Vec<PeerRecord>,
}

impl CoreBuilder {
    #[must_use]
    pub fn new(identity: Identity, config: CoreConfig) -> Self {
        Self {
            identity,
            config,
            effects: BTreeSet::new(),
            plugins: Vec::new(),
            peers: Vec::new(),
        }
    }

    /// Declare which effect kinds this host can actually carry out.
    #[must_use]
    pub fn effects(mut self, kinds: impl IntoIterator<Item = EffectKind>) -> Self {
        self.effects.extend(kinds);
        self
    }

    #[must_use]
    pub fn plugin(mut self, p: impl Plugin + 'static) -> Self {
        self.plugins.push(Box::new(p));
        self
    }

    /// Restore peers the host loaded from storage.
    #[must_use]
    pub fn restore(mut self, peers: impl IntoIterator<Item = PeerRecord>) -> Self {
        self.peers.extend(peers);
        self
    }

    #[must_use]
    pub fn build(self) -> Core {
        let mut caps_out = Vec::new();
        let mut caps_in = Vec::new();
        let mut plugins = Vec::new();
        for p in self.plugins {
            let m = p.manifest();
            // A plugin whose effects this host cannot serve is dropped entirely
            // and its capabilities never advertised, so iOS and Linux can
            // register the same set and simply negotiate down.
            if !m.requires.iter().all(|k| self.effects.contains(k)) {
                continue;
            }
            caps_out.extend(m.outgoing.iter().map(|s| (*s).to_string()));
            caps_in.extend(m.incoming.iter().map(|s| (*s).to_string()));
            plugins.push(p);
        }
        caps_out.sort_unstable();
        caps_out.dedup();
        caps_in.sort_unstable();
        caps_in.dedup();

        let peers = self
            .peers
            .into_iter()
            .filter_map(|r| Some((r.id()?, r)))
            .collect();

        Core {
            identity: self.identity,
            config: self.config,
            peers,
            links: BTreeMap::new(),
            addrs: BTreeMap::new(),
            pairing: None,
            pending_pair_dials: BTreeMap::new(),
            pending_peer_dials: BTreeMap::new(),
            plugins,
            effect_owner: BTreeMap::new(),
            caps_out,
            caps_in,
            next_token: 0,
            next_dial: 0,
            next_msg_id: 0,
            plugin_wake: None,
        }
    }
}

//! The pure state machine: `handle()` takes the host's monotonic clock,
//! returns [`Outcome`], and never touches the world.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::CoreConfig;
use crate::link::{
    BulkSupport, LinkAttrs, LinkDownReason, LinkId, Routes, TransportId, TransportKind,
};
use crate::noise::{Handshake, Identity, Session};
use crate::peer::{PeerRecord, PeerState};
use crate::plugin::{BulkRequest, Cx, PendingSend, Plugin};
use crate::proto::envelope::{Envelope, ErrorCode};
use crate::proto::frame::{self, FrameKind};
use crate::proto::handshake::{GreatestSeen, Hello};
use crate::proto::ids::{DeviceId, Fingerprint};
use crate::proto::pairing;
use crate::vocab::{
    Action, DialToken, EffectKind, EffectResult, EffectToken, Event, LocalCommand, Now, Outcome,
    Sensitivity, TransferId, UiEvent,
};

/// A dial in flight: target, routes not yet tried, whether a person asked.
type PeerDial = (DeviceId, Vec<(TransportId, String)>, bool);

/// Cap on remembered unpaired sightings, so the map cannot leak on a
/// long-running daemon.
const MAX_SEEN: usize = 256;

/// How long to hold a bulk listener open for a sender that never dials;
/// cleared by [`Event::BulkStarted`], so it bounds the wait, not the transfer.
pub const BULK_DIAL_WAIT_MS: u64 = 30_000;

/// What a link we dialled brings with it into its handshake.
struct Dialled {
    attrs: LinkAttrs,
    deadline: u64,
    /// Routes not tried yet. See [`HandshakingLink::fallback`].
    fallback: Option<PeerDial>,
}

/// A link whose first frame has not arrived; pair-or-resume still unknown.
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
    /// Where we dialled, when we are the one who dialled; a pairing that
    /// succeeds keeps this as the peer's known-good address.
    via: Option<(TransportId, String)>,
    /// Routes not tried yet, kept until the session is up: a connected dial
    /// proves a socket, not a peer, and a stalled handshake emits no
    /// `DialFailed` to walk the rest.
    fallback: Option<PeerDial>,
}

struct UpLink {
    session: Session,
    peer: DeviceId,
    /// Every bulk key is derived from this; the cipher itself no longer needs it.
    handshake_hash: Vec<u8>,
    bulk: BulkSupport,
    max_message: u32,
    /// When something last arrived over this link (the handshake counts); the
    /// only liveness signal the core has. See [`Core::best_link`].
    last_recv_ms: u64,
    /// Transport carrying this session; ascending id is preference order.
    transport: TransportId,
    /// Display only; decides nothing.
    kind: TransportKind,
    /// Our `caps_out` ∩ their `caps_in`, which is what we may send.
    can_send: Vec<String>,
    /// Their `caps_out` ∩ our `caps_in`, which is what we will accept.
    can_recv: Vec<String>,
}

enum LinkState {
    Pending(PendingLink),
    Handshaking(Box<HandshakingLink>),
    /// Boxed: a live session is by far the largest variant.
    Up(Box<UpLink>),
}

/// A pairing handshake that finished and is waiting on a human.
struct AwaitingConfirm {
    link: LinkId,
    record: PeerRecord,
    sas: String,
    /// The address we dialled to reach them, if we dialled.
    via: Option<(TransportId, String)>,
}

/// A pairing in flight. A pending confirmation is never replaced: a second
/// handshake must not swap the SAS a human is comparing. See [`Core::why_not_pair`].
struct PairingWindow {
    deadline: u64,
    awaiting: Option<AwaitingConfirm>,
}

/// Somebody waiting on a person to compare six digits.
#[derive(Clone, Debug)]
pub struct PendingPairing<'a> {
    pub name: &'a str,
    pub fingerprint: Fingerprint,
    pub sas: &'a str,
}

/// A machine on the network that this one is not paired with.
#[derive(Clone, Debug)]
pub struct Nearby<'a> {
    pub fingerprint: &'a Fingerprint,
    pub name: &'a str,
    /// Transport-defined and opaque; the best route known.
    pub addr: String,
    pub transport: TransportId,
    pub pairing: bool,
}

/// A machine discovery has shown us, and what it said about itself.
#[derive(Clone, Debug)]
struct Sighting {
    routes: Routes,
    /// Advertised, therefore untrusted; reaches a screen and nothing else.
    name: String,
    /// Whether it said it was already busy pairing with somebody.
    pairing: bool,
}

pub struct Core {
    identity: Identity,
    config: CoreConfig,
    peers: BTreeMap<DeviceId, PeerRecord>,
    links: BTreeMap<LinkId, LinkState>,
    /// Last address discovery offered for a peer. Untrusted, and only ever used
    /// to decide where to dial, never to decide who answered.
    addrs: BTreeMap<DeviceId, Routes>,
    /// Every address discovery has ever shown, keyed by advertised
    /// fingerprint; kept even before pairing, since discovery resolves once.
    seen: BTreeMap<Fingerprint, Sighting>,
    pairing: Option<PairingWindow>,
    /// Monotonic time before which no pairing handshake is answered;
    /// rate-limits a hostile device to one dialog.
    pair_quiet_until: u64,
    /// Dials we started for pairing, and where each was aimed.
    pending_pair_dials: BTreeMap<DialToken, (TransportId, String)>,
    /// Dials to known peers: deadline (see [`crate::link::DIAL_TIMEOUT_MS`]),
    /// routes not yet tried, and whether a person asked.
    pending_peer_dials: BTreeMap<DialToken, (u64, PeerDial)>,
    /// Why the last attempt to reach a peer failed. State, not a `UiEvent`, so
    /// automatic dials cannot flicker errors; dropped on reach or forget.
    dial_trouble: BTreeMap<DeviceId, String>,
    /// When to retry unreachable peers. See
    /// [`crate::config::CoreConfig::reconnect_every_ms`].
    reconnect_at: Option<u64>,
    plugins: Vec<Box<dyn Plugin>>,
    /// Which plugin asked for an outstanding effect.
    effect_owner: BTreeMap<EffectToken, usize>,
    /// Which plugin owns a bulk transfer, so its answer reaches the right one.
    bulk_owner: BTreeMap<TransferId, usize>,
    /// This device's transfer numbering. See [`Cx::new_transfer`].
    next_transfer: u64,
    /// Transfers a host is listening for; removed by [`Event::BulkStarted`],
    /// so the deadline bounds the wait, not the file. See [`BULK_DIAL_WAIT_MS`].
    bulk_wait: BTreeMap<TransferId, u64>,
    caps_out: Vec<String>,
    caps_in: Vec<String>,
    /// Subset of `caps_in` this host can actually act on, not merely relay.
    caps_served: Vec<String>,
    /// `caps_served` as an `EffectSet`, handed to every `Cx`.
    serves: crate::vocab::EffectSet,
    next_token: u64,
    next_dial: u64,
    next_msg_id: u32,
    /// Refreshed at every entry to `handle`.
    wall_ms: u64,
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

    /// What we advertise we can send, minus plugins the host cannot serve.
    #[must_use]
    pub fn caps_out(&self) -> &[String] {
        &self.caps_out
    }

    #[must_use]
    pub fn caps_in(&self) -> &[String] {
        &self.caps_in
    }

    /// What this host can carry out itself, as opposed to only ask for.
    #[must_use]
    pub fn caps_served(&self) -> &[String] {
        &self.caps_served
    }

    /// The code awaiting confirmation; lets a backgrounded UI re-read it
    /// instead of relying on having caught `PairingSas`.
    #[must_use]
    pub fn pending_sas(&self) -> Option<&str> {
        self.pairing
            .as_ref()?
            .awaiting
            .as_ref()
            .map(|a| a.sas.as_str())
    }

    /// Everything [`UiEvent::PairingSas`] carries, readable at any moment by a
    /// UI that missed the event.
    #[must_use]
    pub fn pending_pairing(&self) -> Option<PendingPairing<'_>> {
        let a = self.pairing.as_ref()?.awaiting.as_ref()?;
        Some(PendingPairing {
            name: &a.record.name,
            fingerprint: a.record.fingerprint()?,
            sas: &a.sas,
        })
    }

    /// Whether a pairing is in flight: the `pair=` discovery key
    /// (`docs/PROTOCOL.md` § 4). Polled rather than announced, since a lapsed
    /// pairing has no event.
    #[must_use]
    pub fn pairing_open(&self) -> bool {
        self.pairing.is_some()
    }

    /// Why an inbound pairing handshake will not be answered, or `None` to
    /// answer it. The whole admission policy, in one place.
    fn why_not_pair(&self, now_ms: u64) -> Option<&'static str> {
        if !self.config.accept_pair_requests {
            return Some("this device is not accepting pairing requests");
        }
        // A pending confirmation is never replaced; see `PairingWindow`.
        if let Some(w) = &self.pairing
            && w.awaiting.is_some()
        {
            return Some("already confirming a pairing with somebody else");
        }
        if self.pairing.is_some() {
            return Some("a pairing is already in progress");
        }
        if now_ms < self.pair_quiet_until {
            return Some("too soon after the last pairing attempt");
        }
        None
    }

    /// Offer a machine discovery has seen, on the best route known for it.
    /// Does nothing for a machine already paired.
    fn announce(&self, fp: &Fingerprint, out: &mut Outcome) {
        let Some(s) = self.seen.get(fp) else { return };
        let Some((transport, addr)) = s.routes.in_preference_order().next() else {
            return;
        };
        if self
            .peers
            .values()
            .any(|r| r.fingerprint().as_ref() == Some(fp))
        {
            return;
        }
        out.ui(UiEvent::Discovered {
            fingerprint: fp.clone(),
            name: s.name.clone(),
            addr,
            transport,
            pairing: s.pairing,
        });
    }

    /// Machines discovery can see that this one is not paired with, each on
    /// its best known route.
    pub fn nearby(&self) -> impl Iterator<Item = Nearby<'_>> {
        self.seen.iter().filter_map(|(fp, s)| {
            if self
                .peers
                .values()
                .any(|r| r.fingerprint().as_ref() == Some(fp))
            {
                return None;
            }
            let (transport, addr) = s.routes.in_preference_order().next()?;
            Some(Nearby {
                fingerprint: fp,
                name: &s.name,
                addr,
                transport,
                pairing: s.pairing,
            })
        })
    }

    /// Claim the pairing slot before the first frame, so [`Self::why_not_pair`]
    /// can refuse a second handshake.
    fn begin_pairing(&mut self, now_ms: u64) {
        self.pairing = Some(PairingWindow {
            deadline: now_ms + self.config.pairing_window_ms,
            awaiting: None,
        });
    }

    /// Drop the pairing and start the cooldown; `denied` marks a person saying
    /// the digits differ, not a mere lapse.
    fn end_pairing(&mut self, now_ms: u64, denied: bool) {
        self.pairing = None;
        let cooldown = if denied {
            self.config.pair_denied_cooldown_ms
        } else {
            self.config.pair_cooldown_ms
        };
        self.pair_quiet_until = now_ms + cooldown;
    }

    /// Why the last attempt to reach this peer failed; only meaningful
    /// alongside [`PeerState::Unreachable`].
    #[must_use]
    pub fn dial_trouble(&self, peer: &DeviceId) -> Option<&str> {
        self.dial_trouble.get(peer).map(String::as_str)
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
        // A dial in flight counts as Connecting, not Unreachable.
        if self
            .pending_peer_dials
            .values()
            .any(|(_, (p, _, _))| p == peer)
        {
            return PeerState::Connecting;
        }
        PeerState::Unreachable
    }

    /// The single entry point.
    pub fn handle(&mut self, now: Now, ev: Event) -> Outcome {
        // Everything below is monotonic; the wall clock is stashed for `Now`.
        self.wall_ms = now.wall_ms;
        let now_ms = now.monotonic_ms;
        let mut out = Outcome::default();
        match ev {
            Event::LinkUp { link, attrs, dial } => {
                self.on_link_up(now_ms, link, attrs, dial, &mut out)
            }
            Event::LinkRecv { link, msg } => self.on_link_recv(now_ms, link, &msg, &mut out),
            Event::LinkDown { link, .. } => self.on_link_down(now_ms, link, &mut out),
            Event::DialFailed { dial, reason } => {
                // Answered only by the table holding the token: a late answer
                // must not report a pairing failure to someone who was not pairing.
                if self.pending_pair_dials.remove(&dial).is_some() {
                    // Give the pairing slot back, without cooldown: nobody was
                    // bothered by a dial that never came up.
                    self.pairing = None;
                    out.ui(UiEvent::PairingFailed { reason });
                } else if let Some((_, pending)) = self.pending_peer_dials.remove(&dial) {
                    self.try_next_route(now_ms, pending, &reason, &mut out);
                }
            }
            Event::Discovered { transport, peer } => {
                if let Some(fp) = peer.fingerprint {
                    // Cleared rather than evicted at the cap: anything still
                    // present is advertised again within seconds, and paired
                    // addresses live in `addrs`.
                    if self.seen.len() >= MAX_SEEN && !self.seen.contains_key(&fp) {
                        self.seen.clear();
                    }
                    let sighting = self.seen.entry(fp.clone()).or_insert_with(|| Sighting {
                        routes: Routes::default(),
                        name: peer.name.clone(),
                        pairing: peer.pairing,
                    });
                    sighting.routes.set(transport, peer.addr.clone());
                    // Newest answer wins: names and `pair=` both change.
                    sighting.name = peer.name.clone();
                    sighting.pairing = peer.pairing;
                    if let Some(rec) = self
                        .peers
                        .values()
                        .find(|r| r.fingerprint().as_ref() == Some(&fp))
                        && let Some(id) = rec.id()
                    {
                        self.addrs
                            .entry(id.clone())
                            .or_default()
                            .set(transport, peer.addr);
                        // Dial a paired peer the moment it is sighted; a peer
                        // already reachable is skipped, so a better transport
                        // only wins once the current session ends.
                        self.connect_peer(now_ms, id, &mut out, false);
                    } else {
                        // Announce the best route, not the one that just
                        // arrived: Bluetooth repeats, and would replace a
                        // working Wi-Fi address on screen.
                        self.announce(&fp, &mut out);
                    }
                }
            }
            Event::Undiscovered { transport, addr } => {
                // Clears `seen`, never `addrs`: a lapsed mDNS record does not
                // mean a paired peer's last-known address stopped working.
                let gone: Vec<Fingerprint> = self
                    .seen
                    .iter_mut()
                    .filter_map(|(fp, s)| {
                        (s.routes.forget(transport, &addr) && s.routes.is_empty())
                            .then(|| fp.clone())
                    })
                    .collect();
                for fp in gone {
                    self.seen.remove(&fp);
                    // Said only for machines that were offered; a paired peer
                    // never was.
                    if !self
                        .peers
                        .values()
                        .any(|r| r.fingerprint().as_ref() == Some(&fp))
                    {
                        out.ui(UiEvent::Undiscovered { fingerprint: fp });
                    }
                }
            }
            Event::BulkListening { transfer, endpoint } => {
                self.dispatch_to_transfer_owner(now_ms, transfer, &mut out, |p, cx| {
                    p.on_bulk_listening(cx, transfer, &endpoint);
                });
            }
            // Bytes are moving; nothing left to time out.
            Event::BulkStarted { transfer } => {
                self.bulk_wait.remove(&transfer);
            }
            Event::BulkFinished {
                transfer,
                ok,
                detail,
            } => {
                self.dispatch_to_transfer_owner(now_ms, transfer, &mut out, |p, cx| {
                    p.on_bulk_finished(cx, transfer, ok, &detail);
                });
                self.bulk_owner.remove(&transfer);
                self.bulk_wait.remove(&transfer);
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
        if let Some(r) = self.reconnect_at {
            consider(r);
        }
        for (deadline, _) in self.pending_peer_dials.values() {
            consider(*deadline);
        }
        if let Some(p) = &self.pairing {
            consider(p.deadline);
        }
        for d in self.bulk_wait.values() {
            consider(*d);
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

    fn hello(&self, _now_ms: u64) -> Hello {
        Hello {
            v: crate::proto::WIRE_VERSION,
            // Wall clock, not monotonic: the peer compares it against its own.
            ts_ms: self.wall_ms,
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

        // A link we dialled to pair: we speak first, with XX.
        if let Some(d) = dial
            && let Some(via) = self.pending_pair_dials.remove(&d)
        {
            match Handshake::pair_initiator(&self.identity) {
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
                                    via: Some(via),
                                    // Pairing walks no route list.
                                    fallback: None,
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
            && let Some((_, pending)) = self.pending_peer_dials.remove(&d)
        {
            // Untried routes travel with the link, not the dial token.
            let peer = pending.0.clone();
            self.start_session_initiator(
                now_ms,
                link,
                peer,
                Dialled {
                    attrs,
                    deadline,
                    fallback: Some(pending),
                },
                out,
            );
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
        peer: DeviceId,
        dialled: Dialled,
        out: &mut Outcome,
    ) {
        let Dialled {
            attrs,
            deadline,
            fallback,
        } = dialled;
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
                // Identity and capabilities only, never a command. IK's first
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
                                via: None,
                                fallback,
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
                if let Some(why) = self.why_not_pair(now_ms) {
                    self.fail_link(link, why, out);
                    return;
                }
                match Handshake::pair_responder(&self.identity) {
                    Ok(mut hs) => {
                        if hs.read(body).is_err() {
                            // Slot not claimed yet, but a bad message 1 still
                            // earns the cooldown.
                            self.pair_quiet_until = now_ms + self.config.pair_cooldown_ms;
                            self.fail_link(link, "malformed pairing handshake", out);
                            return;
                        }
                        let hello = minicbor::to_vec(self.hello(now_ms)).expect("hello encodes");
                        match hs.write(&hello) {
                            Ok(m) => {
                                // A real handshake, not a stray connection.
                                self.begin_pairing(now_ms);
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
                                        via: None,
                                        // Inbound: no route list of ours.
                                        fallback: None,
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
                // Learn who is calling to choose their PSK, then replay this
                // message into a real handshake; see `Handshake::session_identify`.
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
                            if !self.accept_hello(&id, &payload, out) {
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
                    self.pairing_attempt_failed(now_ms, link, out);
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
            self.pairing_completed(now_ms, link, h, &payload, out);
        } else {
            // A session initiator learns the peer's Hello from message 2.
            let Some(peer) = h.expect.clone() else {
                self.fail_link(link, "session handshake with no expected peer", out);
                return;
            };
            if !self.accept_hello(&peer, &payload, out) {
                self.fail_link(link, "stale or replayed response", out);
                return;
            }
            self.finish_session(now_ms, link, h.hs, h.attrs, peer, out);
        }
    }

    /// Validate a peer's Hello and advance its replay watermark.
    fn accept_hello(&mut self, peer: &DeviceId, payload: &[u8], out: &mut Outcome) -> bool {
        let Ok(hello) = minicbor::decode::<Hello>(payload) else {
            out.ui(UiEvent::Error {
                peer: Some(peer.clone()),
                code: ErrorCode::BadBody,
                detail: "handshake payload did not decode".to_string(),
            });
            return false;
        };
        let Some(rec) = self.peers.get_mut(peer) else {
            return false;
        };
        match hello.check_freshness(self.wall_ms, GreatestSeen(rec.greatest_seen)) {
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
                    peer: Some(peer.clone()),
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
        let handshake_hash = hs.handshake_hash().unwrap_or_default();
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
        // Directional on purpose: conflating the two sets would let a peer
        // receive a capability it only said it could send.
        let can_send = crate::proto::handshake::negotiate(&self.caps_out, &rec.caps_in);
        let can_recv = crate::proto::handshake::negotiate(&rec.caps_out, &self.caps_in);
        let name = rec.name.clone();

        // Once per session, at `info`: the journal's record of a route appearing.
        tracing::info!(
            %peer,
            transport = attrs.transport.0,
            kind = ?attrs.kind,
            "a route to a peer is up"
        );

        self.links.insert(
            link,
            LinkState::Up(Box::new(UpLink {
                session,
                peer: peer.clone(),
                handshake_hash,
                bulk: attrs.bulk,
                max_message: attrs.max_message,
                // The handshake just arrived here; starting at zero would make
                // every new session lose to whatever was already up.
                last_recv_ms: now_ms,
                transport: attrs.transport,
                kind: attrs.kind,
                can_send,
                can_recv,
            })),
        );
        self.dial_trouble.remove(&peer);
        out.ui(UiEvent::PeerReachable {
            peer: peer.clone(),
            name,
        });

        let mut cx = Cx::new(now_ms, self.next_token, self.next_transfer, self.serves);
        for p in &mut self.plugins {
            p.on_peer_connected(&mut cx, &peer);
        }
        self.next_token = cx.next_token;
        self.next_transfer = cx.next_transfer;
        self.drain_cx(cx, out);
    }
}

// --------------------------------------------------------------------- pairing

impl Core {
    fn pairing_completed(
        &mut self,
        now_ms: u64,
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
            self.pairing_attempt_failed(now_ms, link, out);
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

        // Nothing is written until a human agrees; hold the link in `Pending`
        // so a denial can still close it cleanly.
        self.links.insert(
            link,
            LinkState::Pending(PendingLink {
                attrs: h.attrs,
                deadline: h.deadline,
            }),
        );

        let Some(w) = &mut self.pairing else {
            self.fail_link(link, "no pairing is in progress", out);
            return;
        };
        // Backstop for the never-replace-a-confirmation rule; see `PairingWindow`.
        if w.awaiting.is_some() {
            self.fail_link(link, "a confirmation is already waiting", out);
            return;
        }
        // A person's clock for comparing digits, not the handshake timeout.
        w.deadline = now_ms + self.config.pairing_window_ms;
        w.awaiting = Some(AwaitingConfirm {
            link,
            record: record.clone(),
            sas: sas.clone(),
            via: h.via,
        });

        out.ui(UiEvent::PairingSas {
            name: record.name,
            fingerprint: Fingerprint::of(&pk),
            sas,
        });
    }

    fn confirm_pairing(&mut self, now_ms: u64, accept: bool, out: &mut Outcome) {
        let Some(w) = &mut self.pairing else { return };
        let Some(a) = w.awaiting.take() else { return };
        if !accept {
            // Mismatched digits are the one signal of a relay attack: close
            // the link and go quiet for a long time.
            out.push(Action::Close {
                link: a.link,
                reason: LinkDownReason::Protocol(ErrorCode::NotAllowed),
            });
            self.links.remove(&a.link);
            self.end_pairing(now_ms, true);
            out.ui(UiEvent::PairingFailed {
                reason: "the codes did not match".to_string(),
            });
            return;
        }
        let Some(id) = a.record.id() else { return };
        // The pairing just proved this address reachable; discovery may not
        // speak again for a long time.
        if let Some((transport, addr)) = a.via {
            self.addrs
                .entry(id.clone())
                .or_default()
                .set(transport, addr);
        } else if let Some(fp) = a.record.fingerprint()
            && let Some(via) = self.seen.get(&fp).cloned()
        {
            // They dialled us; discovery may still have shown us an address.
            self.addrs
                .entry(id.clone())
                .or_default()
                .merge_from(&via.routes);
        }
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
        self.peers.insert(id.clone(), a.record);
        self.pairing = None;
        // Pairing and session stay separate: exactly one code path
        // establishes a session.
        out.push(Action::Close {
            link: a.link,
            reason: LinkDownReason::Closed,
        });
        self.links.remove(&a.link);
        // Armed rather than dialled: the two ends confirm at different
        // moments, and a dial landing before the other side finishes pairing
        // is refused as a stranger. The retry loop keeps trying until it lands.
        self.reconnect_at = Some(0);
    }

    /// A pairing handshake that could not be completed. One strike: nothing is
    /// typed, so retrying cannot help, and the cooldown bounds the next try.
    fn pairing_attempt_failed(&mut self, now_ms: u64, link: LinkId, out: &mut Outcome) {
        self.fail_link(link, "pairing handshake failed", out);
        self.end_pairing(now_ms, false);
        out.ui(UiEvent::PairingFailed {
            reason: "the pairing handshake failed".to_string(),
        });
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
        // Passed as its stored box to avoid moving a session per frame.
        mut u: Box<UpLink>,
        kind: FrameKind,
        body: &[u8],
        out: &mut Outcome,
    ) {
        if kind != FrameKind::Transport {
            self.fail_up_link(
                now_ms,
                link,
                u,
                "handshake frame on an established link",
                out,
            );
            return;
        }
        let plaintext = match u.session.decrypt(body) {
            Ok(p) => p,
            Err(e) => {
                let detail = e.to_string();
                self.fail_up_link(now_ms, link, u, &detail, out);
                return;
            }
        };
        let peer = u.peer.clone();
        let allowed = u.can_recv.clone();
        // Recorded only after decryption, so socket noise cannot make a dead
        // route look alive.
        u.last_recv_ms = now_ms;
        self.links.insert(link, LinkState::Up(u));

        let Ok(env) = Envelope::decode(&plaintext) else {
            out.ui(UiEvent::Error {
                peer: Some(peer.clone()),
                code: ErrorCode::BadBody,
                detail: "envelope did not decode".to_string(),
            });
            return;
        };

        // Checked once, here, so no plugin has to remember to.
        if !allowed.iter().any(|c| c == env.cap) {
            self.send_error(now_ms, &peer, &env, ErrorCode::CapNotNegotiated, out);
            return;
        }

        // `err` stops here: routed onward it would be answered with another
        // `err`, endlessly.
        if env.ty == "err" {
            out.ui(UiEvent::Plugin {
                peer: peer.clone(),
                cap: env.cap.to_string(),
                ty: "err".to_string(),
                body: env.body.to_vec(),
            });
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

        let mut cx = Cx::new(now_ms, self.next_token, self.next_transfer, self.serves)
            .for_peer_link(self.bulk_support_for(&peer));
        let result = self.plugins[idx].on_message(&mut cx, &peer, &env);
        self.next_token = cx.next_token;
        self.next_transfer = cx.next_transfer;
        self.remember_owner(&cx, idx);
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
        tracing::debug!(peer = %s.peer, cap = %s.cap, ty = %s.ty, "sending");
        let Some(link) = self.best_link(&s.peer).map(|(id, _)| id) else {
            // Unreachable is an ordinary outcome, not an error.
            out.ui(UiEvent::PeerUnreachable { peer: s.peer });
            return;
        };
        let Some(LinkState::Up(u)) = self.links.get_mut(&link) else {
            return;
        };

        if !u.can_send.contains(&s.cap) {
            out.ui(UiEvent::Error {
                peer: Some(u.peer.clone()),
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
            Ok(ct) => {
                let msg = frame::join(FrameKind::Transport, &ct);
                // Enforces `LinkAttrs::max_message`; measured after sealing,
                // because the AEAD overhead travels too.
                if msg.len() > u.max_message as usize {
                    let (n, cap) = (msg.len(), u.max_message);
                    out.ui(UiEvent::Error {
                        peer: Some(u.peer.clone()),
                        code: ErrorCode::TooLarge,
                        detail: format!("{} bytes does not fit this link, which takes {cap}", n),
                    });
                    return;
                }
                out.push(Action::LinkSend { link, msg });
            }
            Err(e) => {
                out.ui(UiEvent::Error {
                    peer: Some(u.peer.clone()),
                    code: ErrorCode::Internal,
                    detail: e.to_string(),
                });
            }
        }
    }

    /// Record which plugin owns whatever a `Cx` just asked for; effects and
    /// transfers are answered later, and this map is the only way back.
    fn remember_owner(&mut self, cx: &Cx, idx: usize) {
        for (t, _) in &cx.effects {
            self.effect_owner.insert(*t, idx);
        }
        for b in &cx.bulk {
            if let BulkRequest::Listen { transfer, .. } | BulkRequest::Send { transfer, .. } = b {
                self.bulk_owner.insert(*transfer, idx);
            }
        }
    }

    fn drain_cx(&mut self, cx: Cx, out: &mut Outcome) {
        let Cx {
            now_ms,
            sends,
            effects,
            ui,
            wake_at,
            bulk,
            ..
        } = cx;
        for e in ui {
            out.ui(e);
        }
        for (token, effect) in effects {
            out.push(Action::Effect { token, effect });
        }
        for b in bulk {
            self.dispatch_bulk(now_ms, b, out);
        }
        for s in sends {
            self.dispatch_send(0, s, out);
        }
        if let Some(w) = wake_at {
            self.plugin_wake = Some(self.plugin_wake.map_or(w, |p: u64| p.min(w)));
        }
    }

    /// Try the next address for a peer, or record that it cannot be reached.
    /// Reached when a dial fails and when a connected dial's handshake times out.
    fn try_next_route(&mut self, now_ms: u64, pending: PeerDial, why: &str, out: &mut Outcome) {
        let (peer, mut routes, by_hand) = pending;
        // Something else got there while this was failing.
        if self.best_link(&peer).is_some() {
            return;
        }
        if routes.is_empty() {
            self.dial_trouble.insert(peer.clone(), why.to_string());
            // Announced only when a person asked; automatic attempts run on
            // every sighting and would flicker "unreachable".
            if by_hand {
                out.ui(UiEvent::PeerUnreachable { peer });
            }
            return;
        }
        let (transport, addr) = routes.remove(0);
        self.next_dial += 1;
        let next = DialToken(self.next_dial);
        self.pending_peer_dials.insert(
            next,
            (
                now_ms + self.config.dial_timeout_ms,
                (peer, routes, by_hand),
            ),
        );
        out.push(Action::Dial {
            transport,
            addr,
            dial: next,
        });
    }

    /// Make an attempt already under way count as one a person asked for, so
    /// its end is announced.
    fn adopt_attempt(&mut self, peer: &DeviceId) {
        for (_, (p, _, by_hand)) in self.pending_peer_dials.values_mut() {
            if p == peer {
                *by_hand = true;
            }
        }
        for st in self.links.values_mut() {
            if let LinkState::Handshaking(h) = st
                && let Some((p, _, by_hand)) = h.fallback.as_mut()
                && p == peer
            {
                *by_hand = true;
            }
        }
    }

    fn connect_peer(&mut self, now_ms: u64, peer: DeviceId, out: &mut Outcome, by_hand: bool) {
        if !by_hand
            && self
                .pending_peer_dials
                .values()
                .any(|(_, (p, _, _))| p == &peer)
        {
            return;
        }
        // Last-working address first, then whatever discovery has shown.
        let known = self.addrs.get(&peer).cloned().or_else(|| {
            let fp = self.peers.get(&peer)?.fingerprint()?;
            self.seen.get(&fp).map(|s| s.routes.clone())
        });
        let mut routes: Vec<(TransportId, String)> =
            known.iter().flat_map(Routes::in_preference_order).collect();

        match self.peer_state(&peer) {
            PeerState::Unreachable => {}
            // Let the attempt in flight finish, but adopt it for a person who
            // asked, so it says how it ended.
            PeerState::Connecting => {
                if by_hand {
                    self.adopt_attempt(&peer);
                }
                return;
            }
            PeerState::Reachable => {
                // A strictly better transport is still worth dialling (a phone
                // finds Bluetooth before mDNS resolves): the better link wins
                // by existing, the worse one stays as fallback.
                if by_hand {
                    let name = self
                        .peers
                        .get(&peer)
                        .map(|r| r.name.clone())
                        .unwrap_or_default();
                    out.ui(UiEvent::PeerReachable { peer, name });
                    return;
                }
                let Some(current) = self.best_link_transport(&peer) else {
                    return;
                };
                routes.retain(|(t, _)| *t < current);
                if routes.is_empty() {
                    return;
                }
            }
        }
        // Best first; the rest stay behind it for `DialFailed` to walk.
        let first = (!routes.is_empty()).then(|| routes.remove(0));
        let Some((transport, addr)) = first else {
            // Worded for a phone screen as much as a terminal.
            const NOWHERE: &str = "Device is asleep or unreachable. It might be off or on \
                                   another network.";
            self.dial_trouble.insert(peer.clone(), NOWHERE.to_string());
            if by_hand {
                out.ui(UiEvent::Error {
                    peer: Some(peer.clone()),
                    code: ErrorCode::NotAllowed,
                    detail: format!("no address known for {peer}. {NOWHERE}"),
                });
                out.ui(UiEvent::PeerUnreachable { peer });
            }
            return;
        };
        self.next_dial += 1;
        let d = DialToken(self.next_dial);
        self.pending_peer_dials.insert(
            d,
            (
                now_ms + self.config.dial_timeout_ms,
                (peer, routes, by_hand),
            ),
        );
        out.push(Action::Dial {
            transport,
            addr,
            dial: d,
        });
    }

    fn bulk_support_for(&self, peer: &DeviceId) -> Option<BulkSupport> {
        self.best_link(peer).map(|(_, u)| u.bulk)
    }

    /// The link a message to this peer would take: most recently heard from
    /// first, then the better transport, then the newer link. A preference,
    /// never a teardown: getting it wrong costs one message, not a session.
    fn best_link(&self, peer: &DeviceId) -> Option<(LinkId, &UpLink)> {
        self.links
            .iter()
            .filter_map(|(id, st)| match st {
                LinkState::Up(u) if &u.peer == peer => Some((*id, &**u)),
                _ => None,
            })
            // `Reverse`: a lower transport id is the better one; the link id
            // last, so equal links still order and the newer wins.
            .max_by_key(|(id, u)| (u.last_recv_ms, std::cmp::Reverse(u.transport), *id))
    }

    fn best_link_transport(&self, peer: &DeviceId) -> Option<TransportId> {
        self.best_link(peer).map(|(_, u)| u.transport)
    }

    #[must_use]
    pub fn transport_for(&self, peer: &DeviceId) -> Option<TransportKind> {
        self.best_link(peer).map(|(_, u)| u.kind.clone())
    }

    fn dispatch_bulk(&mut self, now_ms: u64, request: BulkRequest, out: &mut Outcome) {
        let (peer, transfer) = match &request {
            BulkRequest::Listen { peer, transfer, .. }
            | BulkRequest::Send { peer, transfer, .. } => (peer.clone(), *transfer),
            BulkRequest::Cancel { transfer } => {
                out.push(Action::BulkCancel {
                    transfer: *transfer,
                });
                return;
            }
        };

        // A link that cannot carry bulk must refuse here, not hand out an
        // endpoint the far end has no route to.
        if self.bulk_support_for(&peer) == Some(BulkSupport::None) {
            out.ui(UiEvent::Error {
                peer: Some(peer.clone()),
                code: ErrorCode::NotAllowed,
                detail: format!(
                    "transport for connection to {peer} does not support file transfers. \
                     change transports and try again."
                ),
            });
            // Reported finished-and-failed too, so the plugin unwinds like it
            // would for any other failure instead of waiting forever.
            out.push(Action::BulkCancel { transfer });
            self.bulk_owner.remove(&transfer);
            return;
        }

        let Some(hash) = self.handshake_hash_for(&peer) else {
            // No session, no key, no transfer; reported as finished-and-failed
            // so the plugin unwinds like it would for any other failure.
            out.ui(UiEvent::Error {
                peer: Some(peer.clone()),
                code: ErrorCode::NotAllowed,
                detail: format!("no session with {peer} to derive a transfer key from"),
            });
            return;
        };
        // The key is derived from the offerer's device id and its own transfer
        // number (`offered_as`), not ours: both ends can name it without
        // asking, since the dialing side is always the one that offered.
        let (offerer, numbered) = match &request {
            BulkRequest::Send { .. } => (self.device_id(), transfer.0),
            BulkRequest::Listen { offered_as, .. } => (peer.clone(), *offered_as),
            BulkRequest::Cancel { .. } => unreachable!("handled above"),
        };
        let key = crate::proto::bulk::key(&hash, offerer.as_str(), numbered).to_vec();

        match request {
            BulkRequest::Listen { expect_bytes, .. } => {
                // Starts here rather than at `BulkListening`, so a host that
                // never binds is bounded by the same deadline as a sender that
                // never dials.
                self.bulk_wait.insert(transfer, now_ms + BULK_DIAL_WAIT_MS);
                out.push(Action::BulkListen {
                    transfer,
                    // The peer's number: the same one the dialer will greet us with.
                    offered_as: numbered,
                    key,
                    expect_bytes,
                });
            }
            BulkRequest::Send { endpoint, .. } => out.push(Action::BulkSend {
                transfer,
                endpoint,
                key,
            }),
            BulkRequest::Cancel { .. } => unreachable!("handled above"),
        }
    }

    /// The session a bulk key is derived from. Uses `best_link`, not the first
    /// match: both ends must derive the key from the same session's handshake
    /// hash, and a peer reachable two ways has more than one to pick from.
    fn handshake_hash_for(&self, peer: &DeviceId) -> Option<Vec<u8>> {
        self.best_link(peer).map(|(_, u)| u.handshake_hash.clone())
    }
}

// ------------------------------------------------- local commands, effects, tick

impl Core {
    fn on_local(&mut self, now_ms: u64, cmd: LocalCommand, out: &mut Outcome) {
        match cmd {
            LocalCommand::RequestPairing { transport, addr } => {
                // Claim the slot before dialling, so a stranger's handshake
                // cannot land while a person here is waiting on their own.
                if let Some(why) = self.why_not_pair(now_ms) {
                    out.ui(UiEvent::PairingFailed {
                        reason: why.to_string(),
                    });
                    return;
                }
                self.begin_pairing(now_ms);
                self.next_dial += 1;
                let d = DialToken(self.next_dial);
                self.pending_pair_dials.insert(d, (transport, addr.clone()));
                out.push(Action::Dial {
                    transport,
                    addr,
                    dial: d,
                });
            }
            LocalCommand::ConfirmPairing { accept } => self.confirm_pairing(now_ms, accept, out),
            LocalCommand::SetPeerAddress {
                peer,
                transport,
                addr,
            } => {
                if self.peers.contains_key(&peer) {
                    self.addrs.entry(peer).or_default().set(transport, addr);
                } else {
                    out.ui(UiEvent::Error {
                        peer: Some(peer.clone()),
                        code: ErrorCode::NotPaired,
                        detail: format!("{peer} is not a paired device"),
                    });
                }
            }
            LocalCommand::Connect { peer } => self.connect_peer(now_ms, peer, out, true),
            LocalCommand::ReconsiderRoutes => {
                // Not `by_hand`: nobody pressed anything, and only the
                // automatic path may improve on a route already carrying.
                let known: Vec<DeviceId> = self.peers.keys().cloned().collect();
                for peer in known {
                    self.connect_peer(now_ms, peer, out, false);
                }
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
                        peer: Some(peer.clone()),
                        code: ErrorCode::UnknownType,
                        detail: format!("no plugin handles {cap}"),
                    });
                    return;
                };
                tracing::debug!(%peer, %cap, %ty, "local plugin command");
                let mut cx = Cx::new(now_ms, self.next_token, self.next_transfer, self.serves)
                    .for_peer_link(self.bulk_support_for(&peer));
                let r = self.plugins[idx].on_local(&mut cx, &peer, &ty, &body);
                self.next_token = cx.next_token;
                self.next_transfer = cx.next_transfer;
                self.remember_owner(&cx, idx);
                self.drain_cx(cx, out);
                if let Err(e) = r {
                    out.ui(UiEvent::Error {
                        peer: Some(peer.clone()),
                        code: e.code(),
                        detail: e.to_string(),
                    });
                }
            }
        }
    }

    fn handle_revoke(&mut self, now_ms: u64, peer: &DeviceId, out: &mut Outcome) {
        // Kept before the record goes: `announce` below needs it to find the
        // machine in `seen`, and by then there is nothing left to ask.
        let fingerprint = self.peers.get(peer).and_then(PeerRecord::fingerprint);
        // Drop the record first, so a race cannot leave a live session against a
        // peer we have decided to forget.
        self.peers.remove(peer);
        self.addrs.remove(peer);
        self.dial_trouble.remove(peer);
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

        // Said out loud, so a screen can stop drawing it without guessing
        // when the core got round to this.
        out.ui(UiEvent::Revoked { peer: peer.clone() });

        // Offer it again: discovery resolves once and stays quiet, so without
        // this a still-present machine would never be mentioned again.
        if let Some(fp) = fingerprint {
            self.announce(&fp, out);
        }
    }

    /// Hand something to the plugin that owns a transfer; one nobody owns has
    /// already finished, or was invented, so there is nothing to tell.
    fn dispatch_to_transfer_owner(
        &mut self,
        now_ms: u64,
        transfer: TransferId,
        out: &mut Outcome,
        f: impl FnOnce(&mut Box<dyn Plugin>, &mut Cx),
    ) {
        let Some(&idx) = self.bulk_owner.get(&transfer) else {
            return;
        };
        let mut cx = Cx::new(now_ms, self.next_token, self.next_transfer, self.serves);
        f(&mut self.plugins[idx], &mut cx);
        self.next_token = cx.next_token;
        self.next_transfer = cx.next_transfer;
        self.remember_owner(&cx, idx);
        self.drain_cx(cx, out);
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
        let mut cx = Cx::new(now_ms, self.next_token, self.next_transfer, self.serves);
        self.plugins[idx].on_effect_result(&mut cx, token, result);
        self.next_token = cx.next_token;
        self.next_transfer = cx.next_transfer;
        self.remember_owner(&cx, idx);
        self.drain_cx(cx, out);
    }

    fn on_tick(&mut self, now_ms: u64, out: &mut Outcome) {
        // A pairing nobody answered. Measured against the host's monotonic
        // clock, so moving the wall clock cannot extend it.
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
            // Quiet afterwards: without a cooldown, asking a machine with
            // nobody sitting at it would simply try again at once.
            self.end_pairing(now_ms, false);
            out.ui(UiEvent::PairingFailed {
                reason: "nobody confirmed the pairing in time".to_string(),
            });
        }

        // Senders that were told where to connect and never did; `BulkStarted`
        // has already taken out everything actually moving bytes.
        let gave_up: Vec<TransferId> = self
            .bulk_wait
            .iter()
            .filter(|(_, deadline)| now_ms >= **deadline)
            .map(|(t, _)| *t)
            .collect();
        for transfer in gave_up {
            self.bulk_wait.remove(&transfer);
            // Cancelled at the host first, so the port and the reserved
            // filename go back before anyone is told the transfer is over.
            out.push(Action::BulkCancel { transfer });
            // Reported as a failure, like every other bulk ending: the plugin
            // unwinds, the person who accepted is told, the far end answered.
            self.dispatch_to_transfer_owner(now_ms, transfer, out, |p, cx| {
                p.on_bulk_finished(cx, transfer, false, "the sender never connected");
            });
            self.bulk_owner.remove(&transfer);
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
            let was = self.links.remove(&l);
            out.push(Action::Close {
                link: l,
                reason: LinkDownReason::Closed,
            });
            // A handshake that ran out of time has not proved the device
            // unreachable, only this way in; carry on down the route list.
            if let Some(LinkState::Handshaking(h)) = was
                && let Some(pending) = h.fallback
            {
                self.try_next_route(now_ms, pending, "got answer that cut off half way", out);
            }
        }

        // Dials nobody ever answered: Network.framework does not always fail
        // a dial with no viable path, so this bounds the wait independently
        // of any host remembering to.
        let expired: Vec<DialToken> = self
            .pending_peer_dials
            .iter()
            .filter(|(_, (deadline, _))| now_ms >= *deadline)
            .map(|(d, _)| *d)
            .collect();
        for d in expired {
            let Some((_, pending)) = self.pending_peer_dials.remove(&d) else {
                continue;
            };
            self.try_next_route(now_ms, pending, "peer didn't answer", out);
        }

        // Retries peers with no live link: a sighting is not guaranteed to
        // repeat (mDNS resolves once, CoreBluetooth re-reports at its own
        // discretion), so nothing else would ever reconnect them.
        if self.reconnect_at.is_none_or(|at| now_ms >= at) {
            self.reconnect_at = Some(now_ms + self.config.reconnect_every_ms);
            // Only where there is somewhere to dial, or this would storm a
            // machine that is simply off.
            let waiting: Vec<DeviceId> = self
                .peers
                .keys()
                .filter(|id| self.best_link(id).is_none() && self.addrs.contains_key(*id))
                .cloned()
                .collect();
            for id in waiting {
                // Not `by_hand`: nobody asked, so a failure is state and not
                // news, and `connect_peer` drops it if a dial is already out.
                self.connect_peer(now_ms, id, out, false);
            }
        }

        if let Some(w) = self.plugin_wake
            && now_ms >= w
        {
            self.plugin_wake = None;
            let mut cx = Cx::new(now_ms, self.next_token, self.next_transfer, self.serves);
            for p in &mut self.plugins {
                p.on_tick(&mut cx);
            }
            self.next_token = cx.next_token;
            self.next_transfer = cx.next_transfer;
            self.drain_cx(cx, out);
        }
    }

    fn on_link_down(&mut self, now_ms: u64, link: LinkId, out: &mut Outcome) {
        let u = match self.links.remove(&link) {
            Some(LinkState::Up(u)) => u,
            // A dialled link that died before it was ever a session has not
            // proved the device unreachable, only this way in; carry on down
            // the route list rather than leaving the alternatives untried.
            Some(LinkState::Handshaking(h)) => {
                // Gives back the pairing slot: completing the handshake is
                // what parks a link in `Pending`, so one still handshaking
                // can never be the one awaiting confirmation. No cooldown,
                // since a refusal here isn't abuse.
                if h.pairing_flow {
                    self.pairing = None;
                }
                if let Some(pending) = h.fallback {
                    self.try_next_route(
                        now_ms,
                        pending,
                        "connection closed before session was established",
                        out,
                    );
                }
                return;
            }
            // Deliberately left alone: the two ends approve at different
            // moments, and closing here would cancel the second confirmation
            // the instant the first person pressed a button. The deadline
            // handles a peer that really did go away.
            _ => return,
        };

        // Losing a link is not losing a device: a peer reachable two ways must
        // not be announced unreachable, or lose its plugin state, just because
        // one route died.
        tracing::info!(
            peer = %u.peer,
            transport = u.transport.0,
            now_on = ?self.best_link(&u.peer).map(|(_, b)| b.transport.0),
            "lost route to peer"
        );

        if self.best_link(&u.peer).is_some() {
            // Still here, but possibly not over what it was; said rather than
            // left silent, since a host showing which transport carries it
            // would otherwise keep showing the one that just died.
            let name = self
                .peers
                .get(&u.peer)
                .map(|r| r.name.clone())
                .unwrap_or_default();
            out.ui(UiEvent::PeerReachable {
                peer: u.peer.clone(),
                name,
            });
            return;
        }

        out.ui(UiEvent::PeerUnreachable {
            peer: u.peer.clone(),
        });
        // Arms an immediate retry rather than dialling directly, so the
        // takeover still goes through the one path that knows about routes
        // already in flight; waiting for the next heartbeat would add its
        // whole interval to every radio switch.
        self.reconnect_at = Some(0);
        let mut cx = Cx::new(now_ms, self.next_token, self.next_transfer, self.serves);
        for p in &mut self.plugins {
            p.on_peer_disconnected(&mut cx, &u.peer);
        }
        self.next_token = cx.next_token;
        self.next_transfer = cx.next_transfer;
        self.drain_cx(cx, out);
    }

    /// Drop an established link and announce it like any other death.
    /// `fail_link` alone can't: `on_transport_frame` holds the `UpLink` out of
    /// the table, so it has to go back in before teardown runs.
    fn fail_up_link(
        &mut self,
        now_ms: u64,
        link: LinkId,
        u: Box<UpLink>,
        detail: &str,
        out: &mut Outcome,
    ) {
        // Taken before the link goes back in the table, which moves `u`.
        let peer = u.peer.clone();
        self.links.insert(link, LinkState::Up(u));
        out.push(Action::Close {
            link,
            reason: LinkDownReason::Protocol(ErrorCode::NotAllowed),
        });
        out.ui(UiEvent::Error {
            peer: Some(peer),
            code: ErrorCode::NotAllowed,
            detail: detail.to_string(),
        });
        self.on_link_down(now_ms, link, out);
    }

    fn fail_link(&mut self, link: LinkId, detail: &str, out: &mut Outcome) {
        // A link that never finished handshaking has no peer yet: naming the
        // device we hoped was there would attribute the failure on the
        // strength of an opener that never verified.
        let peer = match self.links.remove(&link) {
            Some(LinkState::Up(u)) => Some(u.peer.clone()),
            _ => None,
        };
        // Logged as well as surfaced in `UiEvent::Error`: a daemon has no
        // screen to show that error on.
        tracing::warn!(
            link = link.0,
            peer = peer.as_ref().map(ToString::to_string),
            detail,
            "closing a link"
        );
        out.push(Action::Close {
            link,
            reason: LinkDownReason::Protocol(ErrorCode::NotAllowed),
        });
        out.ui(UiEvent::Error {
            peer,
            code: ErrorCode::NotAllowed,
            detail: detail.to_string(),
        });
    }
}

// ---------------------------------------------------------------------- builder

/// Explicit, compile-time plugin registration; not `inventory`/`ctor`/`linkme`.
/// Static linking into an iOS binary with dead-strip enabled removes
/// constructor-registered symbols, silently leaving zero plugins.
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
        let mut caps_served = Vec::new();
        let mut plugins = Vec::new();
        for p in self.plugins {
            let m = p.manifest();
            // Advertised both ways regardless of `requires`: being unable to
            // serve a capability isn't being unable to use it, and a reply
            // shares the request's capability. Servability is found
            // separately, via connect-time announcements and `not_allowed`.
            if m.requires.iter().all(|k| self.effects.contains(k)) {
                caps_served.extend(m.incoming.iter().map(|s| (*s).to_string()));
            } else {
                tracing::debug!(
                    plugin = m.id,
                    "this host cannot serve requests for this capability"
                );
            }
            caps_out.extend(m.outgoing.iter().map(|s| (*s).to_string()));
            caps_in.extend(m.incoming.iter().map(|s| (*s).to_string()));
            plugins.push(p);
        }
        caps_out.sort_unstable();
        caps_out.dedup();
        caps_in.sort_unstable();
        caps_in.dedup();
        caps_served.sort_unstable();
        caps_served.dedup();

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
            seen: BTreeMap::new(),
            pairing: None,
            pair_quiet_until: 0,
            pending_pair_dials: BTreeMap::new(),
            pending_peer_dials: BTreeMap::new(),
            dial_trouble: BTreeMap::new(),
            reconnect_at: None,
            plugins,
            effect_owner: BTreeMap::new(),
            bulk_owner: BTreeMap::new(),
            next_transfer: 0,
            bulk_wait: BTreeMap::new(),
            caps_out,
            caps_in,
            caps_served,
            serves: crate::vocab::EffectSet::new(self.effects.iter().copied()),
            next_token: 0,
            next_dial: 0,
            next_msg_id: 0,
            wall_ms: 0,
            plugin_wake: None,
        }
    }
}

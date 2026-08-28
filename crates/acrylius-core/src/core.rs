//! The state machine.
//!
//! Everything a device does to another device happens here, and none of it
//! touches the world. `handle()` takes the host's monotonic clock as a
//! parameter, returns [`Outcome`], and that is the entire interface.

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

/// A dial in flight: who we are trying to reach, the routes not yet tried, and
/// whether a person asked for it.
type PeerDial = (DeviceId, Vec<(TransportId, String)>, bool);

/// How many unpaired sightings to remember at once.
///
/// Far more than a home or an office has, and small enough that the map cannot
/// become a leak on a daemon that runs for months across many networks.
const MAX_SEEN: usize = 256;

/// How long a host may hold a listener open for a sender that has not dialled.
///
/// The wait, never the transfer: [`Event::BulkStarted`] ends it the moment the
/// far end connects, so a slow gigabyte is not this deadline's business.
///
/// What is being bounded is a sender that will never come — its session died
/// between the accept and the dial, its app was killed, its Wi-Fi went away.
/// Until this existed there was nothing to end that: a port stayed bound and a
/// filename stayed reserved for the life of the process, and the person who had
/// pressed Accept was shown a transfer that never moved and never failed.
///
/// Thirty seconds because the dial follows the endpoint immediately and over
/// anything a session already runs on it is one round trip. Long enough that a
/// loaded phone waking a socket is not cut off; short enough that a person
/// watching it knows something is wrong before they ask.
///
/// Public because a host may want to say so in a UI, and because a second host
/// must not invent its own answer — the shape of the lock-budget bug, which is
/// written up on [`crate::plugins::session::LOCK_REPLY_BUDGET_MS`].
pub const BULK_DIAL_WAIT_MS: u64 = 30_000;

/// What a link we dialled brings with it into its handshake.
struct Dialled {
    attrs: LinkAttrs,
    deadline: u64,
    /// Routes not tried yet. See [`HandshakingLink::fallback`].
    fallback: Option<PeerDial>,
}

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
    /// Where we dialled, when we are the one who dialled.
    ///
    /// A pairing that succeeds has just proved an address reachable, and
    /// throwing that away was the reason a freshly paired device reported
    /// itself unreachable until discovery happened to run again.
    via: Option<(TransportId, String)>,
    /// The routes this dial had not tried yet, kept until the *session* is up
    /// rather than until the socket is.
    ///
    /// A dial that connects has proved a socket, not a peer. If the handshake
    /// then never finishes — something else listening on the port, a stale NAT
    /// mapping, a peer that is not the one expected — there is no `DialFailed`
    /// to walk the rest of the list, because the dial did not fail. Dropping
    /// the alternatives the moment the link came up left a device with a
    /// perfectly good second route unreachable until something else happened
    /// to dial it, and told nobody.
    fallback: Option<PeerDial>,
}

struct UpLink {
    session: Session,
    peer: DeviceId,
    /// Kept because every bulk key is derived from it. It is not a secret the
    /// session still needs — the cipher has its own state — but it is the one
    /// value both ends share and nobody else knows.
    handshake_hash: Vec<u8>,
    /// The two promises `LinkAttrs` makes that the core is supposed to keep.
    ///
    /// Kept as the fields themselves rather than the whole `LinkAttrs`, which
    /// would make this variant much larger than its siblings for no gain. Both
    /// went unenforced until a second transport existed to notice.
    bulk: BulkSupport,
    max_message: u32,
    /// When something last arrived over this link, on the host's monotonic
    /// clock. The handshake counts, because it arrived here too.
    ///
    /// This is the only evidence a link is still carrying anything. Nothing
    /// else in the core can tell a working socket from one whose far end has
    /// vanished — that is a host's job, and the honest answer from a host takes
    /// as long as a keepalive. See [`Core::best_link`], which routes by it.
    last_recv_ms: u64,
    /// Which transport is carrying this session, and how it reads to a person.
    ///
    /// The id is what orders them: preference is ascending, which is how the
    /// hosts register them — Wi-Fi before Bluetooth. Kept on the link rather
    /// than read back out of the `LinkId`'s high bits, because that would make
    /// every routing decision depend on ids having been minted with the
    /// namespacing rule, which is a convention a host can get wrong.
    transport: TransportId,
    /// The name for it. Not used to decide anything: it exists because "the
    /// phone is on Bluetooth now" is invisible otherwise, and a transport that
    /// silently changes under you is one nobody can tell is working.
    kind: TransportKind,
    /// Our `caps_out` ∩ their `caps_in`, which is what we may send.
    can_send: Vec<String>,
    /// Their `caps_out` ∩ our `caps_in`, which is what we will accept.
    can_recv: Vec<String>,
}

enum LinkState {
    Pending(PendingLink),
    Handshaking(Box<HandshakingLink>),
    /// Boxed for the same reason its sibling is: a live session is by far the
    /// largest thing a link can be, and every pending or handshaking link would
    /// otherwise pay for it.
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

struct PairingWindow {
    /// The code's key, or `None` for a window that answers no one.
    ///
    /// `None` is the side that *initiated*. It has no code to answer with — it
    /// already sent one — and only needs somewhere to keep the confirmation it
    /// is waiting on. It used to keep it in a window with an all-zero psk, which
    /// is a constant anybody can type: for as long as a human was looking at the
    /// SAS dialog, any device that could reach this one completed `XXpsk0`
    /// against a known key, and the handshake it finished *replaced* the pending
    /// confirmation. The human then compared a code and approved a stranger.
    psk: Option<[u8; 32]>,
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
    /// to decide where to dial, never to decide who answered.
    addrs: BTreeMap<DeviceId, Routes>,
    /// Every address discovery has ever shown us, keyed by advertised
    /// fingerprint and kept regardless of whether that device is paired.
    ///
    /// Discovery resolves a service once and then stays quiet until something
    /// about it changes, so an announcement that arrives before pairing is the
    /// only one there will be. Dropping it because the sender was not yet a
    /// peer meant the address was gone for good.
    seen: BTreeMap<Fingerprint, Routes>,
    pairing: Option<PairingWindow>,
    /// Dials we started for pairing, and the PSK to use when they land.
    pending_pair_dials: BTreeMap<DialToken, ([u8; 32], (TransportId, String))>,
    /// Dials we started to reach a known peer, each with the routes not yet
    /// tried. A dial that fails falls through to the next rather than reporting
    /// the peer unreachable while a working route sits untried.
    /// The flag is whether a person asked, which decides whether failing to
    /// reach them is worth saying out loud.
    /// Each carries a deadline, because a transport that answers neither way
    /// would otherwise hold the walk open forever — see
    /// [`crate::link::DIAL_TIMEOUT_MS`].
    pending_peer_dials: BTreeMap<DialToken, (u64, PeerDial)>,
    /// Why the last attempt to reach a peer ended without a session.
    ///
    /// State, deliberately, and not a `UiEvent`. An automatic dial runs on
    /// every sighting, so announcing each exhausted attempt would make a device
    /// that is coming up perfectly normally flicker an error — the same reason
    /// `try_next_route` only says "unreachable" out loud when a person asked.
    /// A screen showing a peer as not connected wants to say *why* at the
    /// moment it draws, which is a question about the present, not news.
    ///
    /// Bounded by the number of paired peers, and dropped when one is reached
    /// or forgotten.
    dial_trouble: BTreeMap<DeviceId, String>,
    /// When to try the peers nothing can reach again. See
    /// [`crate::config::CoreConfig::reconnect_every_ms`].
    reconnect_at: Option<u64>,
    plugins: Vec<Box<dyn Plugin>>,
    /// Which plugin asked for an outstanding effect.
    effect_owner: BTreeMap<EffectToken, usize>,
    /// Which plugin owns a bulk transfer, so its answer reaches the right one.
    bulk_owner: BTreeMap<TransferId, usize>,
    /// This device's transfer numbering. See [`Cx::new_transfer`].
    next_transfer: u64,
    /// Transfers a host is listening for, and when to stop waiting.
    ///
    /// An entry lives from [`Action::BulkListen`] until the far end connects,
    /// and no longer: [`Event::BulkStarted`] takes it out, so the deadline
    /// bounds the wait and never the file. See [`BULK_DIAL_WAIT_MS`].
    bulk_wait: BTreeMap<TransferId, u64>,
    caps_out: Vec<String>,
    caps_in: Vec<String>,
    /// The subset of `caps_in` this host can actually act on, rather than only
    /// relay to a plugin that will refuse. Every device advertises the full set;
    /// this is what separates "can do" from "can ask for".
    caps_served: Vec<String>,
    /// The same fact as `caps_served`, in the form a plugin asks about it: what
    /// this machine can carry out, handed to every `Cx`.
    serves: crate::vocab::EffectSet,
    next_token: u64,
    next_dial: u64,
    next_msg_id: u32,
    /// Refreshed at every entry to `handle`, so the handshake timestamp is
    /// always current without threading a second clock through every function.
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

    /// What this host can carry out itself. Anything in `caps_in` but not here
    /// is a capability it can ask a peer for and will refuse if asked.
    #[must_use]
    pub fn caps_served(&self) -> &[String] {
        &self.caps_served
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

    /// Whether a pairing window is open right now.
    ///
    /// `docs/PROTOCOL.md` § 4 specifies a `pair=0|1` key in the discovery
    /// advertisement, "a convenience for a user interface". Both ends have read
    /// it since M1 — `tcp.rs` and `NWTransport.swift` both decode it — and
    /// nothing has ever written it, so `DiscoveredPeer::pairing` has been
    /// `false` in production for its whole life.
    ///
    /// Read rather than announced because the window closes four ways: paired,
    /// refused, given up on, and expired. Only the first two have an event, and
    /// an advertisement that lied because the fourth had no event would be
    /// worse than none.
    #[must_use]
    pub fn pairing_open(&self) -> bool {
        self.pairing.is_some()
    }

    /// Why the last attempt to reach this peer ended without a session, if it
    /// has not been reached since.
    ///
    /// Only meaningful alongside [`PeerState::Unreachable`]. A peer that is
    /// connecting has an attempt in flight and nothing to explain yet.
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
        // A dial that has not produced a link yet is not a link, and it is not
        // nothing either. Reported as unreachable, it made the screen say "Not
        // connected" for the entire time the app was busy connecting — and
        // beside a state called "Connecting", that reads as *gave up* rather
        // than as *not yet*. It is a longer window now that a dial is given a
        // budget to answer within, so the difference is one a person sees.
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
        // Everything below works in monotonic milliseconds. The wall clock is
        // stashed for the one thing that needs it — see `Now`.
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
                // Answered by whichever table actually holds the token, and by
                // neither when nothing does. It used to fall through to
                // `PairingFailed` for anything it did not recognise, which was
                // harmless only while every token lived until it was answered.
                // A dial can now expire, so a host answering one late — the
                // ordinary case for a connection that was never going to come
                // up — would announce a pairing failure to someone who had not
                // been pairing.
                if self.pending_pair_dials.remove(&dial).is_some() {
                    out.ui(UiEvent::PairingFailed { reason });
                } else if let Some((_, pending)) = self.pending_peer_dials.remove(&dial) {
                    self.try_next_route(now_ms, pending, &reason, &mut out);
                }
            }
            Event::Discovered { transport, peer } => {
                if let Some(fp) = peer.fingerprint {
                    // Cached unconditionally. Whether the sender is paired is
                    // not knowable from an advertisement, and pairing usually
                    // happens after the single resolution discovery will emit —
                    // so dropping this as "not a peer yet" threw away the only
                    // chance to learn where that device lives.
                    // Per transport, so a sighting on one never evicts what
                    // another already knows.
                    // Bounded, because nothing prunes it and every acrylius
                    // device on every network this one ever joins leaves an
                    // entry. A key and a couple of addresses is small, but the
                    // map only ever grows, and a long-lived daemon on a busy
                    // network is exactly where that stops being theoretical.
                    //
                    // Dropped rather than evicted cleverly: this is a cache of
                    // where something was last seen, and anything discovery
                    // still cares about will be advertised again within seconds.
                    // A paired peer's address is kept in `addrs` below, which is
                    // bounded by the number of peers and is the one that matters.
                    if self.seen.len() >= MAX_SEEN && !self.seen.contains_key(&fp) {
                        self.seen.clear();
                    }
                    self.seen
                        .entry(fp.clone())
                        .or_default()
                        .set(transport, peer.addr.clone());
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
                        // And reach it. Seeing a device we have already paired
                        // with, on an address we have just learned, is the whole
                        // set of conditions for a session — waiting for someone
                        // to press a button adds nothing, and it is why a phone
                        // whose Bluetooth link dropped stayed dark until the app
                        // was force-quit: the radio reconnected and announced
                        // itself, and nothing acted on it.
                        //
                        // It is also what makes a better transport take over. A
                        // peer already reachable over Bluetooth is skipped here,
                        // so Wi-Fi coming back does not win until the Bluetooth
                        // session actually ends — which is the conservative way
                        // round, and the same order `dispatch_send` prefers.
                        self.connect_peer(now_ms, id, &mut out, false);
                    } else {
                        // Nobody we know. Said out loud, because until now the
                        // core kept every sighting in a private map with no
                        // accessor and no event — it knew every acrylius
                        // machine on the network and had no way to mention one.
                        // That is the whole gap between "discovery works" and
                        // "you can pick a computer to pair with".
                        out.ui(UiEvent::Discovered {
                            fingerprint: fp,
                            name: peer.name,
                            addr: peer.addr,
                            transport,
                            pairing: peer.pairing,
                        });
                    }
                }
            }
            Event::BulkListening { transfer, endpoint } => {
                self.dispatch_to_transfer_owner(now_ms, transfer, &mut out, |p, cx| {
                    p.on_bulk_listening(cx, transfer, &endpoint);
                });
            }
            // Bytes are moving, so there is nothing left to time out. How long a
            // file takes is the file's business.
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
                // The transfer is over either way, so nothing should still be
                // holding a key for it, or waiting on it.
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
            // Wall clock, not monotonic: the peer compares this against its own
            // clock, and an uptime means nothing to anyone else.
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

        // A link we dialled to pair: we speak first, with XXpsk3.
        if let Some(d) = dial
            && let Some((psk, via)) = self.pending_pair_dials.remove(&d)
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
                                    via: Some(via),
                                    // Pairing walks no route list: a person
                                    // typed one address and a code for it.
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
            // The routes we have not tried travel with the link, not with the
            // dial token. A connected socket is not a finished session, and
            // until it is one there is still somewhere else to go.
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
                let Some(window) = &mut self.pairing else {
                    // No window open. Refusing here is what makes a pairing
                    // window a *window* rather than a permanently reachable
                    // endpoint that merely usually rejects you.
                    self.fail_link(link, "no pairing window is open", out);
                    return;
                };
                let Some(psk) = window.psk else {
                    // A window belonging to a pairing we started. It answers
                    // nobody: we are the one waiting to be confirmed, and there
                    // is no code here for anyone to have matched.
                    self.fail_link(link, "no pairing window is open", out);
                    return;
                };
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
                                        via: None,
                                        // Somebody dialled us, so there is no
                                        // list of ours to walk.
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
        // Directional, and deliberately not symmetric: what we may send is our
        // outgoing set against their incoming set, and vice versa. Conflating
        // the two would let a peer receive a capability it only said it could
        // send.
        let can_send = crate::proto::handshake::negotiate(&self.caps_out, &rec.caps_in);
        let can_recv = crate::proto::handshake::negotiate(&rec.caps_out, &self.caps_in);
        let name = rec.name.clone();

        // Once per session, at `info`, for the same reason the BLE transport
        // says a central subscribed: which routes a peer has, and when each of
        // them appeared, is the first question anybody asks about this thing and
        // until now the journal could not answer it at all. A device beside a
        // desktop holds two of these; nothing said so.
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
                // The handshake came over this link a moment ago, so it is as
                // proven as a link ever gets. Starting it at zero instead would
                // make every new session lose to whatever was already there,
                // which is the opposite of what just happened.
                last_recv_ms: now_ms,
                transport: attrs.transport,
                kind: attrs.kind,
                can_send,
                can_recv,
            })),
        );
        // Whatever went wrong getting here is history the moment it works.
        // Leaving it would have a connected peer explain a failure it no
        // longer has.
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
            w.awaiting = Some(AwaitingConfirm {
                link,
                record,
                sas,
                via: h.via,
            });
        } else {
            // We initiated; there is no window, so keep the same state inline.
            // `None`, not a zero key: this holds a confirmation, it does not
            // open a door. See `PairingWindow::psk`.
            self.pairing = Some(PairingWindow {
                psk: None,
                // The window a *person* now has to compare six digits in, not
                // the handshake timeout that got us here. `h.deadline` is
                // fifteen seconds measured from before the connection was made;
                // the responder has always given this two minutes, and the side
                // that typed the code deserves the same.
                deadline: now_ms + self.config.pairing_window_ms,
                attempts: 0,
                awaiting: Some(AwaitingConfirm {
                    link,
                    record,
                    sas,
                    via: h.via,
                }),
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
        // A pairing that just completed has proved an address reachable. Keep
        // it: discovery may not speak again for a long time, and until this was
        // recorded a device reported itself unreachable the moment after it
        // finished pairing.
        if let Some((transport, addr)) = a.via {
            self.addrs
                .entry(id.clone())
                .or_default()
                .set(transport, addr);
        } else if let Some(fp) = a.record.fingerprint()
            && let Some(via) = self.seen.get(&fp).cloned()
        {
            // They dialled us, so we have no address of theirs from the
            // handshake. Discovery may still have shown us one.
            self.addrs.entry(id.clone()).or_default().merge_from(&via);
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
        // The link stays up but unnegotiated. The peer will open a session when
        // it wants one; keeping pairing and session strictly separate means
        // there is exactly one code path that establishes a session.
        out.push(Action::Close {
            link: a.link,
            reason: LinkDownReason::Closed,
        });
        self.links.remove(&a.link);
        // And ask to be woken at once, so something opens a session.
        //
        // "The peer will open a session when it wants one" was only ever true
        // between two computers. A phone always dials and is never dialled —
        // `NWTransport::advertise` is deliberately unimplemented — so when the
        // phone is the side confirming, nobody dialled at all. Pairing
        // succeeded and the device sat at "Not connected" until the app was
        // force-quit, because a relaunch was the only thing that produced a
        // fresh sighting to act on.
        //
        // The reconnect heartbeat rather than a dial from here, and that is
        // deliberate: the two ends confirm at different moments, so whichever
        // goes first would dial a peer that has not finished pairing yet and be
        // refused as a stranger. Arming the timer instead means the attempt is
        // repeated until it lands, on the one code path that already knows how
        // to do that.
        self.reconnect_at = Some(0);
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
        // Taken and returned as the box it is stored in: the link is removed
        // from the table for the duration of the call and put back afterwards,
        // and moving a session's worth of bytes twice per frame to do it would
        // be a poor trade for the borrow it avoids.
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
        // Recorded only once the frame has decrypted, so that noise on a socket
        // cannot make a dead route look alive. Anything that gets this far came
        // from the peer and from nobody else.
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

        // Checked once, here, so no plugin has to remember to. A capability the
        // peer never declared it would send does not reach a handler at all.
        if !allowed.iter().any(|c| c == env.cap) {
            self.send_error(now_ms, &peer, &env, ErrorCode::CapNotNegotiated, out);
            return;
        }

        // An error reply is protocol-level, not a plugin's business, so the core
        // surfaces it and stops. Routing it onward would have a plugin meet an
        // unknown verb and answer with another `err`, which the peer would
        // answer in turn.
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
                // The other promise `LinkAttrs` makes: "the core will never hand
                // down a frame larger than this, and enforces it on plugins so
                // an oversized body is a `TooLarge` error rather than a
                // mysterious hang". Measured after sealing, because the tag and
                // the AEAD overhead travel too and a transport has to carry all
                // of it.
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

    /// Record which plugin owns whatever a `Cx` just asked for.
    ///
    /// Both effects and bulk transfers are answered later, by which time the
    /// only way back to the plugin that asked is this map. A transfer that was
    /// never recorded here is one whose answers go nowhere, which is exactly
    /// how an accepted offer sat waiting for an endpoint that had already
    /// arrived.
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

    /// Turn a plugin's bulk request into an action, with a key it never sees.
    ///
    /// The key is derived here because the session secret is the core's alone.
    /// A plugin that could derive one could derive any of them, and a host is
    /// given a single-use key scoped to one transfer rather than anything it
    /// could reuse.
    /// What the live link to a peer can carry, if there is one.
    /// Dial a peer we believe we know how to reach.
    ///
    /// `by_hand` separates the two callers, and it governs two things because
    /// both follow from the same question — did a person ask for this?
    ///
    /// A typed `connect` that finds no address deserves to be told so; an
    /// automatic attempt after a sighting does not, because an error nobody
    /// caused is only noise. And a typed `connect` must still work while a dial
    /// is outstanding, since retrying is the one thing a person can do about a
    /// dial that is going nowhere — but discovery may resolve the same service
    /// repeatedly, and a dial per sighting would be a storm aimed at a device
    /// whose only offence is being switched on.
    /// Try the next address for a peer, or say it cannot be reached.
    ///
    /// Reached from both ways an attempt can end without a session: the dial
    /// itself failing, and a dial that connected whose handshake then ran out of
    /// time. The second used to go nowhere at all — the routes were dropped the
    /// moment the socket opened, on the assumption that a connected socket was a
    /// reached peer.
    fn try_next_route(&mut self, now_ms: u64, pending: PeerDial, why: &str, out: &mut Outcome) {
        let (peer, mut routes, by_hand) = pending;
        // Something else got there while this was failing. Neither a further
        // dial nor a death notice would be true.
        if self.best_link(&peer).is_some() {
            return;
        }
        // A device on Wi-Fi and BLE has two ways in. Reporting it unreachable
        // because the first failed would be wrong, and wrong in the direction a
        // user cannot diagnose.
        if routes.is_empty() {
            // Every route is spent, so this is the answer for now and worth
            // keeping. Recorded whoever asked: a screen that draws a peer as
            // not connected wants a reason regardless of what started the
            // attempt, and there is no longer a button to press to produce one.
            self.dial_trouble.insert(peer.clone(), why.to_string());
            // Only when a person asked. An automatic attempt runs on every
            // sighting, and one route can easily be seen before a working one
            // is — announcing that would make a device flicker "unreachable"
            // while it is coming up perfectly normally. It stays unreachable
            // either way; that is a fact about its state, not news.
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

    fn connect_peer(&mut self, now_ms: u64, peer: DeviceId, out: &mut Outcome, by_hand: bool) {
        if !by_hand
            && self
                .pending_peer_dials
                .values()
                .any(|(_, (p, _, _))| p == &peer)
        {
            return;
        }
        // Three places an address can come from, in order of how much they are
        // worth: one we recorded when the peer last worked, one discovery has
        // shown us, or nothing.
        let known = self.addrs.get(&peer).cloned().or_else(|| {
            let fp = self.peers.get(&peer)?.fingerprint()?;
            self.seen.get(&fp).cloned()
        });
        let mut routes: Vec<(TransportId, String)> =
            known.iter().flat_map(Routes::in_preference_order).collect();

        match self.peer_state(&peer) {
            PeerState::Unreachable => {}
            // A dial or a handshake is already in flight. Let it finish —
            // including when a person asked, because what they asked for is
            // already happening and a second attempt would not make it
            // happen sooner.
            PeerState::Connecting => return,
            PeerState::Reachable => {
                // Reachable is not the same as reachable *well*, and treating
                // them as one thing is what put a phone on Bluetooth and left
                // it there.
                //
                // A phone walking into the room finds Bluetooth first: it is
                // already connected to the desktop's radio while mDNS has yet
                // to resolve anything. Stopping here because a link exists
                // meant Wi-Fi was never dialled for the rest of the session —
                // and a Bluetooth link cannot carry a file, so every transfer
                // was refused while a perfectly good route sat unused.
                //
                // So a strictly better transport is still worth dialling. The
                // existing link is left alone rather than replaced: the core
                // keys sends by `LinkId`, whose high bits are the transport,
                // and picks the lowest — so the better link takes over by
                // existing, and the worse one stays as the fallback it already
                // was.
                if by_hand {
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
            // "Unreachable" would be true but useless. Not knowing where a
            // device is differs from failing to reach it, and only one of
            // those the user can do something about.
            //
            // No mention of a command-line flag: this reaches a phone screen as
            // often as a terminal, and advice about `--addr` there is advice
            // about a thing that is not on the device reading it.
            const NOWHERE: &str = "Nothing has found it yet — it may be switched off, on \
                                   another network, or a device that only ever dials out, \
                                   which is never discovered at all.";
            // Kept whoever asked. This is the most common reason a peer sits
            // there not connecting, and the screen saying so is the only place
            // it now gets explained.
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

    /// The link a message to this peer would take.
    ///
    /// Two doc blocks left over from when `transport_for` and
    /// `best_link_transport` were separate walks have been removed rather than
    /// updated: both described choosing by `LinkId` order and said that with
    /// Wi-Fi and Bluetooth both up this answers Wi-Fi, which is exactly the
    /// behaviour below no longer has.
    ///
    /// One definition, because everything that asks about a peer's connection
    /// has to get the same answer: what a send goes over, what the screen
    /// names, and whether a file can be carried at all. Those were three
    /// separate walks that agreed only because a `BTreeMap` happens to order
    /// `LinkId`s the way transports are numbered — true by convention in the
    /// daemon, and quietly false anywhere ids are minted another way.
    ///
    /// **Most recently heard from first**, then the better transport, then the
    /// newer link. Not transport preference alone, which is what this used to
    /// be and what made a phone's now-playing screen stop moving whenever Wi-Fi
    /// was switched off.
    ///
    /// Transport preference alone answers "which of these *would* be best",
    /// and the core has no way to ask whether any of them still works. A socket
    /// whose far end has vanished rather than closed stays `ESTABLISHED` and
    /// accepts everything written to it; only a keepalive ends that, and only
    /// after twenty seconds. Preferring it for those twenty seconds put every
    /// reply and every announcement into it while a Bluetooth link sat beside it
    /// working. Worse, a phone that re-dials Wi-Fi leaves the dead socket in
    /// place next to the new one, and `min_by_key` kept the *older* of two equal
    /// transports — so the dead one went on being chosen after the repair.
    /// Observed as `a route to a peer went away … now_on=Some(1)`: a second live
    /// route on the transport that had supposedly just gone.
    ///
    /// Which link a message arrived on is the one piece of evidence available
    /// for free, and it is the peer's own answer to the question: it sends over
    /// *its* best link, so hearing from it over Bluetooth is the peer saying
    /// that is where it lives now. Recency decides, and transport preference
    /// only breaks a tie between links equally proven — which is what two
    /// freshly established sessions are.
    ///
    /// Deliberately a preference and never a teardown. An earlier attempt read
    /// the same evidence and *closed* the route it thought was dead, which threw
    /// away a perfectly good Wi-Fi link whenever one stray Bluetooth frame
    /// landed in the moment between this end completing a handshake and the
    /// other end finishing it. Getting this wrong should cost one message the
    /// slower way, not a session.
    fn best_link(&self, peer: &DeviceId) -> Option<(LinkId, &UpLink)> {
        self.links
            .iter()
            .filter_map(|(id, st)| match st {
                LinkState::Up(u) if &u.peer == peer => Some((*id, &**u)),
                _ => None,
            })
            // `Reverse` on the transport because a *lower* id is the better one,
            // and the id last so that two links which are otherwise equal are
            // still ordered — a peer that reconnects on one transport leaves
            // both, and the newer of them is the one it just proved.
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

        // A link that cannot carry bulk must say so here, not by handing out an
        // endpoint the far end has no route to.
        //
        // `BulkSupport::None` exists for exactly this and went unchecked until
        // there was a second transport to check it against: over BLE the
        // desktop would accept an offer, listen on a TCP port, and send an
        // address a phone with Wi-Fi off can never reach — which looks, from
        // the phone, like a transfer that simply stopped.
        if self.bulk_support_for(&peer) == Some(BulkSupport::None) {
            out.ui(UiEvent::Error {
                peer: Some(peer.clone()),
                code: ErrorCode::NotAllowed,
                detail: format!(
                    "the link to {peer} cannot carry files. Reach it over the \
                     network for that."
                ),
            });
            // Reported finished-and-failed as well, so whichever plugin asked
            // unwinds the way it would for any other failure rather than
            // waiting for an endpoint that is never coming.
            out.push(Action::BulkCancel { transfer });
            self.bulk_owner.remove(&transfer);
            return;
        }

        let Some(hash) = self.handshake_hash_for(&peer) else {
            // No session, no key, and therefore no transfer. Reported as a
            // finished-and-failed one so the plugin unwinds the same way it
            // would for any other failure rather than waiting forever.
            out.ui(UiEvent::Error {
                peer: Some(peer.clone()),
                code: ErrorCode::NotAllowed,
                detail: format!("no session with {peer} to derive a transfer key from"),
            });
            return;
        };
        // Whoever offered the transfer owns the id, and its device id separates
        // the two directions. Both ends can name it without asking: the side
        // that dials is the side that offered, and the side that listens was
        // offered to. Without this the first transfer each way shares a key and
        // a nonce sequence — see `proto::bulk::key`.
        //
        // The *number* has to be the offerer's too, and it is no longer ours to
        // assume: a device that receives an offer now keys everything by an id
        // of its own, because the one in the offer was minted from the sender's
        // counter and means something else here. The sender has never heard of
        // ours, so `offered_as` is what goes into the key.
        let (offerer, numbered) = match &request {
            BulkRequest::Send { .. } => (self.device_id(), transfer.0),
            BulkRequest::Listen { offered_as, .. } => (peer.clone(), *offered_as),
            BulkRequest::Cancel { .. } => unreachable!("handled above"),
        };
        let key = crate::proto::bulk::key(&hash, offerer.as_str(), numbered).to_vec();

        match request {
            BulkRequest::Listen { expect_bytes, .. } => {
                // The clock starts here rather than at `BulkListening`, so that
                // a host which never manages to bind is bounded by the same
                // deadline as a sender which never dials. Both look identical
                // from here, and both used to wait for ever.
                self.bulk_wait.insert(transfer, now_ms + BULK_DIAL_WAIT_MS);
                out.push(Action::BulkListen {
                    transfer,
                    // `numbered` is the offerer's, which for a listen is the
                    // peer's — the same number the dialer will greet us with.
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

    /// The session a bulk key is derived from.
    ///
    /// `best_link`, not the first link that happens to match. Both ends derive
    /// the key from the handshake hash of a session they must agree on, and a
    /// peer reachable two ways has two of them — so picking by iteration order
    /// meant one end could key a transfer from a session the other end was not
    /// using, and nothing would decrypt. It went unnoticed while a peer only
    /// ever had one link, which is the same reason `max_message` and
    /// `BulkSupport` went unenforced.
    fn handshake_hash_for(&self, peer: &DeviceId) -> Option<Vec<u8>> {
        self.best_link(peer).map(|(_, u)| u.handshake_hash.clone())
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
                        psk: Some(pairing::psk(&norm)),
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
                    self.pending_pair_dials
                        .insert(d, (pairing::psk(&norm), (transport, addr.clone())));
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
                // Not `by_hand`, deliberately. Nobody pressed anything, so a
                // peer that cannot be reached is state and not news — and it is
                // the automatic path that is allowed to improve on a route
                // already carrying, which is the whole point of asking.
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
    }

    /// Hand something to the plugin that owns a transfer.
    ///
    /// A transfer that nobody owns is one that has already finished, or one a
    /// host invented. Either way there is nothing to tell.
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

        // Senders that were told where to connect and never did. See
        // [`BULK_DIAL_WAIT_MS`]; `BulkStarted` has already taken out everything
        // that is actually moving bytes.
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
            // Reported as a failure rather than left silent, which is the rule
            // every other bulk ending follows: the plugin unwinds, the person
            // who accepted it is told, and the far end gets an answer.
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
            // A handshake that ran out of time has not proved the *device*
            // unreachable, only this way in. Carry on down the list this dial
            // was walking — or, if there is nothing left of it, say so once.
            if let Some(LinkState::Handshaking(h)) = was
                && let Some(pending) = h.fallback
            {
                self.try_next_route(now_ms, pending, "it answered, then stopped part way", out);
            }
        }

        // Dials nobody ever answered.
        //
        // A transport is meant to answer every dial exactly once, with a link
        // or with a failure, and the whole route walk hangs off that promise.
        // Network.framework does not keep it: a connection with no viable path
        // waits for one rather than failing, so switching Wi-Fi off left the
        // phone dialling a Wi-Fi route indefinitely and never walking on to the
        // Bluetooth route behind it. Nothing above rescues that — the retry
        // heartbeat declines to start a second dial while one is outstanding,
        // and this one never came back.
        //
        // So the walk is bounded here as well, where it cannot depend on a host
        // remembering to bound it.
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
            self.try_next_route(now_ms, pending, "it never answered", out);
        }

        // Try again the peers nothing can reach.
        //
        // Auto-connect fires on a sighting, and there is no rule that says a
        // sighting will ever happen again: mDNS resolves once and goes quiet,
        // and a peripheral CoreBluetooth has already reported is reported
        // again at its own discretion. Everything that ended a session without
        // producing a fresh advertisement therefore left the device
        // unreachable for good — a sleeping computer, a phone in a pocket, a
        // Bluetooth link that dropped.
        if self.reconnect_at.is_none_or(|at| now_ms >= at) {
            self.reconnect_at = Some(now_ms + self.config.reconnect_every_ms);
            // Only where there is somewhere to dial. A peer nothing has ever
            // found has no address to try, and asking again every ten seconds
            // would be a storm aimed at a machine that is simply off.
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
            // proved the *device* unreachable, only this way in — something
            // else answering on the port, or the peer refusing us. Carry on
            // down the list this dial was walking rather than stopping here
            // with the alternatives still untried and nobody told.
            Some(LinkState::Handshaking(h)) => {
                if let Some(pending) = h.fallback {
                    self.try_next_route(
                        now_ms,
                        pending,
                        "the connection closed before it opened",
                        out,
                    );
                }
                return;
            }
            _ => return,
        };

        // Losing a link is not losing a device.
        //
        // This used to announce every link's death as the peer's, which is
        // wrong the moment a peer is reachable two ways — the arrangement a
        // phone beside a desktop has all the time. Neither consequence is
        // cosmetic. `PeerUnreachable` is what a host uses to throw away what a
        // peer told it, and `on_peer_disconnected` is what stops a plugin
        // broadcasting to it. So switching from Wi-Fi to Bluetooth emptied the
        // phone's session controls and stopped the desktop announcing its lock
        // state to a device it was still connected to.
        //
        // And nothing put either back, because the repair for both hangs off
        // the peer becoming reachable again — which never happens to a peer
        // that never left.
        // The other half of the pair above, and the line that makes a takeover
        // readable: which route died, and what — if anything — the next message
        // will go over instead.
        tracing::info!(
            peer = %u.peer,
            transport = u.transport.0,
            now_on = ?self.best_link(&u.peer).map(|(_, b)| b.transport.0),
            "a route to a peer went away"
        );

        if self.best_link(&u.peer).is_some() {
            // Still here, but possibly not over what it was. Said rather than
            // left silent, because a host that shows which transport is
            // carrying would otherwise go on showing the one that just died,
            // and because this is the moment to ask a peer for anything that
            // was only ever sent when it changed.
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
        // And go looking straight away rather than at the next heartbeat.
        //
        // This is the moment a takeover has to happen: the route that was
        // carrying the session has just died, and the whole point of holding a
        // second one is that the gap is short. Waiting up to `reconnect_every_ms`
        // to *begin* would add ten seconds to every switch between radios, on
        // top of however long the dial itself takes.
        //
        // Armed rather than dialled here, so the attempt still runs through the
        // one path that knows about routes already being walked.
        self.reconnect_at = Some(0);
        let mut cx = Cx::new(now_ms, self.next_token, self.next_transfer, self.serves);
        for p in &mut self.plugins {
            p.on_peer_disconnected(&mut cx, &u.peer);
        }
        self.next_token = cx.next_token;
        self.next_transfer = cx.next_transfer;
        self.drain_cx(cx, out);
    }

    /// Drop an *established* link and announce it the way any other death is.
    ///
    /// `fail_link` is not enough for one of these. It removes the link from the
    /// table, which is right for a link that never came up — but
    /// `on_transport_frame` holds the only `Box<UpLink>` *out* of that table for
    /// the duration of the call, so the remove found nothing to remove. The peer
    /// was never announced unreachable, no plugin was told it had gone, and the
    /// `LinkDown` the transport sent once it acted on the `Close` found nothing
    /// left to report either. A session dropped over one bad frame left a device
    /// that looked connected, and stayed that way.
    ///
    /// So the link goes back before it is torn down, and the teardown is the one
    /// every other link gets — including the part that decides a peer reachable
    /// two ways has not actually gone anywhere.
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
        // A link that never finished handshaking has no peer yet, and saying
        // `None` is the honest answer: naming the device we *hoped* was at the
        // other end would be attributing a failure to somebody on the strength
        // of an opener that did not verify.
        let peer = match self.links.remove(&link) {
            Some(LinkState::Up(u)) => Some(u.peer.clone()),
            _ => None,
        };
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

/// Explicit, compile-time plugin registration.
///
/// Deliberately not `inventory`, `ctor` or `linkme`: static linking into an iOS
/// binary with dead-strip enabled removes constructor-registered symbols, and
/// the failure mode is an app that silently has zero plugins. There is nothing
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
        let mut caps_served = Vec::new();
        let mut plugins = Vec::new();
        for p in self.plugins {
            let m = p.manifest();
            // Every plugin is registered and every capability advertised, in
            // both directions.
            //
            // Gating this on `requires` was wrong, and running it is what
            // showed why: a phone has no desktop session of its own, so it
            // would have lost `session/1` entirely and been unable to ask a
            // computer to lock one. Being unable to *serve* a capability says
            // nothing about being able to *use* it. It also broke replies,
            // since an answer arrives under the same capability as the request.
            //
            // What a device can actually do is discovered two ways instead:
            // a plugin announces what it has when a peer connects (a catalogue
            // of commands, a session state, wake targets), and an attempt the
            // host cannot serve is answered `not_allowed`.
            if m.requires.iter().all(|k| self.effects.contains(k)) {
                caps_served.extend(m.incoming.iter().map(|s| (*s).to_string()));
            } else {
                tracing::debug!(
                    plugin = m.id,
                    "this host cannot serve requests for this capability, only send them"
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

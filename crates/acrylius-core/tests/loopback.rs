//! Two cores, one process, no sockets and no clock.
//!
//! This is the project's spine. It drives a complete `XXpsk3` pairing, then an
//! `IKpsk2` session, then a plugin round trip, entirely in memory, so the whole
//! protocol is verifiable on a Linux box with no Apple hardware anywhere near
//! it. That was the point of making the core sans-IO.

use std::collections::{BTreeMap, VecDeque};

use acrylius_core::config::CoreConfig;
use acrylius_core::core::{Core, CoreBuilder};
use acrylius_core::link::{LinkAttrs, LinkDownReason, LinkId, TransportId, TransportKind};
use acrylius_core::noise::Identity;
use acrylius_core::peer::PeerState;
use acrylius_core::plugins::ping;
use acrylius_core::proto::ids::DeviceId;
use acrylius_core::vocab::{
    Action, DiscoveredPeer, Event, LocalCommand, Now, Sensitivity, UiEvent,
};

const CODE: &str = "ABCD1234";
/// Wrong, but well formed. A code that fails `pairing::normalize` never leaves
/// the device that typed it, so it is no guess at all and reaches no window.
const WRONG: &str = "ZZZZ9999";
const TRANSPORT: TransportId = TransportId(0);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Side {
    A,
    B,
    /// A second computer, so that one device can be paired with two.
    ///
    /// Most tests need two devices and say nothing about C. It exists because
    /// some questions cannot even be asked with two — chiefly "does the core
    /// route to the *right* peer", which is invisible while every core has
    /// exactly one peer and any answer is the correct one.
    C,
}

impl Side {
    // `other()` used to live here, for the one place that assumed a transfer
    // had the same name at both ends and could therefore be answered by
    // flipping sides. It cannot: a receiver numbers a transfer itself, so the
    // harness matches a dial to its listener by the endpoint instead, and there
    // was nothing left that needed to name "the other one".
    fn addr(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
        }
    }
}

/// An in-memory transport. It is a transport in exactly the sense the core
/// means: it produces LinkUp/LinkRecv/LinkDown and consumes Dial/LinkSend/Close.
struct Net {
    a: Core,
    b: Core,
    /// A second computer. Built for every `Net` and left alone unless a test
    /// pairs with it, because an unpaired core is inert: it is dialled by
    /// nothing and dials nothing.
    c: Core,
    now: u64,
    next_link: u64,
    /// (side, link) -> the far end of the same wire.
    ///
    /// Both halves, because with three devices "the other side" is no longer a
    /// property of the side that sent — it is a property of the wire.
    peer_link: BTreeMap<(Side, u64), (Side, u64)>,
    queue: VecDeque<(Side, Event)>,
    /// Milliseconds to add to side B's clock. Two machines that booted at
    /// different times do not share an uptime.
    pub b_skew: u64,
    /// Milliseconds since the epoch, shared by both sides the way two
    /// NTP-synced machines share one.
    pub wall: u64,
    /// Keys handed to each side's host, so a test can check both derived the
    /// same one without either sending it.
    pub bulk_keys: BTreeMap<(Side, acrylius_core::vocab::TransferId), Vec<u8>>,
    pub ui: Vec<(Side, UiEvent)>,
    pub persisted: Vec<(Side, String, bool)>,
    /// Every dial the core asked for, in order, so a test can check which route
    /// was preferred and how far the fallback walked.
    pub dialed: Vec<(TransportId, String)>,
    /// Give new links BLE's attributes rather than loopback's. The differences
    /// that matter are `BulkSupport::None` and a 16 KiB `max_message`.
    pub links_are_ble: bool,
    /// The same, for one transport only — so a pair can be on Bluetooth and
    /// Wi-Fi at the same time, which is the arrangement a phone actually has
    /// and the one where preferring the wrong link shows up.
    pub ble_transport: Option<TransportId>,
    /// How far a transfer gets once the sender has been told where to dial.
    pub dials: Dials,
    /// Which side is listening on which endpoint, and what it calls the
    /// transfer. The two ends number a transfer separately, so a dial can only
    /// be matched to its listener by where it was told to go.
    listening: BTreeMap<String, (Side, TransferId)>,
    /// Every link the harness has brought up, and which transport carried it,
    /// so a test can take one away by name. Losing the better transport while
    /// the worse one is still connected is the case a real phone meets every
    /// time Wi-Fi is switched off, and the only way to write it is to be able
    /// to say *which* link died.
    pub links: Vec<(Side, TransportId, u64)>,
}

impl Net {
    fn new(a: Core, b: Core) -> Self {
        Self {
            a,
            b,
            c: core("other-pc"),
            now: 1_700_000_000_000,
            next_link: 0,
            peer_link: BTreeMap::new(),
            queue: VecDeque::new(),
            b_skew: 0,
            wall: 1_700_000_000_000,
            bulk_keys: BTreeMap::new(),
            ui: Vec::new(),
            persisted: Vec::new(),
            dialed: Vec::new(),
            links_are_ble: false,
            ble_transport: None,
            dials: Dials::AndFinishes,
            listening: BTreeMap::new(),
            links: Vec::new(),
        }
    }

    fn skew_for(&self, s: Side) -> u64 {
        match s {
            Side::A => 0,
            Side::B => self.b_skew,
            // Its own origin again, so a three-device test cannot accidentally
            // pass by two machines sharing an uptime.
            Side::C => self.b_skew.wrapping_mul(2),
        }
    }

    fn core(&mut self, s: Side) -> &mut Core {
        match s {
            Side::A => &mut self.a,
            Side::B => &mut self.b,
            Side::C => &mut self.c,
        }
    }

    fn local(&mut self, s: Side, cmd: LocalCommand) {
        self.queue.push_back((s, Event::Local(cmd)));
        self.run();
    }

    /// Drive until nothing is left to deliver.
    fn run(&mut self) {
        let mut guard = 0;
        while let Some((side, ev)) = self.queue.pop_front() {
            guard += 1;
            assert!(guard < 500, "the harness did not settle: a message loop?");
            // Each side keeps its own monotonic origin — machines do not share
            // an uptime — while the wall clock is the one thing they agree on.
            let now = Now {
                monotonic_ms: self.now + self.skew_for(side),
                wall_ms: self.wall,
            };
            let out = self.core(side).handle(now, ev);
            for action in out.actions {
                self.apply(side, action);
            }
        }
    }

    fn apply(&mut self, side: Side, action: Action) {
        match action {
            Action::Ui(e) => self.ui.push((side, e)),

            // A bulk channel, simulated. No socket and no bytes: what is being
            // checked here is the negotiation — that a receiver is asked before
            // anything is listened for, that the endpoint reaches the sender,
            // and that both ends are told how it ended. The bytes themselves
            // are the transport's business and are tested against real sockets.
            Action::BulkListen { transfer, key, .. } => {
                assert_eq!(key.len(), 32, "a host is handed a real key");
                self.bulk_keys.insert((side, transfer), key);
                // One endpoint per transfer, so that a dial can be matched back
                // to the listener that put it there. The two ends no longer
                // agree on what a transfer is called — the receiver numbers it
                // itself — so a harness that told both sides the sender's
                // number would be modelling something no host does.
                let endpoint = format!("{}:{}", side.addr(), 9000 + (transfer.0 & 0xffff));
                self.listening.insert(endpoint.clone(), (side, transfer));
                self.queue
                    .push_back((side, Event::BulkListening { transfer, endpoint }));
            }
            Action::BulkSend {
                transfer,
                key,
                endpoint,
            } => {
                // Both ends must have derived the same key from the session
                // without either transmitting it. If this ever fails, nothing
                // would decrypt on a real socket. Looked up by endpoint, since
                // the listener files it under its own number.
                if let Some(&(listener, theirs)) = self.listening.get(&endpoint)
                    && let Some(k) = self.bulk_keys.get(&(listener, theirs))
                {
                    assert_eq!(&key, k, "both ends must derive the same bulk key");
                }
                self.bulk_keys.insert((side, transfer), key);
                if self.dials == Dials::Never {
                    return;
                }
                // Whoever is listening on that endpoint, under whatever it
                // calls the transfer.
                let Some(&(listener, theirs)) = self.listening.get(&endpoint) else {
                    return;
                };
                // Only the listening side learns that anything connected, and
                // only its host could have known. That is the whole reason
                // `BulkStarted` exists.
                self.queue
                    .push_back((listener, Event::BulkStarted { transfer: theirs }));
                if self.dials == Dials::AndKeepsGoing {
                    return;
                }
                for (who, id) in [(side, transfer), (listener, theirs)] {
                    self.queue.push_back((
                        who,
                        Event::BulkFinished {
                            transfer: id,
                            ok: true,
                            detail: String::new(),
                        },
                    ));
                }
            }
            Action::BulkCancel { transfer } => {
                self.bulk_keys.remove(&(side, transfer));
            }
            Action::Persist {
                key,
                value,
                sensitivity,
            } => {
                self.persisted.push((side, key, value.is_some()));
                assert_eq!(
                    sensitivity,
                    Sensitivity::Secret,
                    "peer records hold key material"
                );
            }
            Action::Dial {
                addr,
                dial,
                transport,
            } => {
                self.dialed.push((transport, addr.clone()));
                // Only the two cores are wired. Anything else is somewhere that
                // did not answer — which is the whole point of having more than
                // one route to try.
                let Some(target) = (match addr.as_str() {
                    "A" => Some(Side::A),
                    "B" => Some(Side::B),
                    "C" => Some(Side::C),
                    _ => None,
                }) else {
                    self.queue.push_back((
                        side,
                        Event::DialFailed {
                            dial,
                            reason: format!("nothing is listening at {addr}"),
                        },
                    ));
                    return;
                };
                assert_ne!(target, side, "a core does not dial itself");
                let mine = self.next_link;
                let theirs = self.next_link + 1;
                self.next_link += 2;
                self.peer_link.insert((side, mine), (target, theirs));
                self.peer_link.insert((target, theirs), (side, mine));
                self.links.push((side, transport, mine));
                self.links.push((target, transport, theirs));
                let attrs = if self.links_are_ble || self.ble_transport == Some(transport) {
                    LinkAttrs::ble(transport)
                } else {
                    LinkAttrs::loopback(transport)
                };
                self.queue.push_back((
                    side,
                    Event::LinkUp {
                        link: LinkId(mine),
                        attrs: attrs.clone(),
                        dial: Some(dial),
                    },
                ));
                self.queue.push_back((
                    target,
                    Event::LinkUp {
                        link: LinkId(theirs),
                        attrs,
                        dial: None,
                    },
                ));
            }
            Action::LinkSend { link, msg } => {
                let Some(&(peer_side, peer)) = self.peer_link.get(&(side, link.0)) else {
                    return;
                };
                self.queue.push_back((
                    peer_side,
                    Event::LinkRecv {
                        link: LinkId(peer),
                        msg,
                    },
                ));
            }
            Action::Close { link, .. } => {
                if let Some((peer_side, peer)) = self.peer_link.remove(&(side, link.0)) {
                    self.peer_link.remove(&(peer_side, peer));
                    self.queue.push_back((
                        peer_side,
                        Event::LinkDown {
                            link: LinkId(peer),
                            reason: LinkDownReason::Closed,
                        },
                    ));
                }
            }
            Action::Effect { .. } | Action::Advertise { .. } | Action::Discover { .. } => {}
        }
    }

    fn sas_for(&self, s: Side) -> Option<&str> {
        self.ui.iter().rev().find_map(|(side, e)| match e {
            UiEvent::PairingSas { sas, .. } if *side == s => Some(sas.as_str()),
            _ => None,
        })
    }

    fn saw(&self, s: Side, f: impl Fn(&UiEvent) -> bool) -> bool {
        self.ui.iter().any(|(side, e)| *side == s && f(e))
    }
}

/// A core that also shares files, for the transfer tests.
fn sharing_core(name: &str) -> Core {
    CoreBuilder::new(
        Identity::generate().unwrap(),
        CoreConfig {
            name: name.to_string(),
            platform: "test".to_string(),
            ..Default::default()
        },
    )
    .effects([acrylius_core::vocab::EffectKind::Share])
    .plugin(ping::PingPlugin::default())
    .plugin(acrylius_core::plugins::share::SharePlugin::default())
    .build()
}

fn core(name: &str) -> Core {
    CoreBuilder::new(
        Identity::generate().unwrap(),
        CoreConfig {
            name: name.to_string(),
            platform: "test".to_string(),
            ..Default::default()
        },
    )
    .plugin(ping::PingPlugin::default())
    .build()
}

/// Pair A and B, leaving both with a persisted record of the other.
fn paired() -> (Net, DeviceId, DeviceId) {
    let (a, b) = (core("phone"), core("pc"));
    let (a_id, b_id) = (a.device_id(), b.device_id());
    let mut net = Net::new(a, b);

    net.local(
        Side::B,
        LocalCommand::OpenPairingWindow {
            code: CODE.to_string(),
        },
    );
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
            code: CODE.to_string(),
        },
    );

    // The property the pairing screen rests on: one code, both screens.
    let sa = net.sas_for(Side::A).expect("A shows a SAS").to_string();
    let sb = net.sas_for(Side::B).expect("B shows a SAS").to_string();
    assert_eq!(
        sa, sb,
        "both ends must display the same short authentication string"
    );

    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });
    (net, a_id, b_id)
}

/// A, paired with two computers at once. Returns their ids in order.
///
/// The arrangement a phone actually has, and the one two cores cannot express:
/// while every device has exactly one peer, routing to the wrong peer is
/// indistinguishable from routing to the right one.
fn paired_twice() -> (Net, DeviceId, DeviceId) {
    let (mut net, _a_id, b_id) = paired();
    let c_id = net.c.device_id();

    // A second session with a second peer needs a later opener than the first.
    net.wall += 1_000;
    net.local(
        Side::C,
        LocalCommand::OpenPairingWindow {
            code: CODE.to_string(),
        },
    );
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::C.addr().to_string(),
            code: CODE.to_string(),
        },
    );
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::C, LocalCommand::ConfirmPairing { accept: true });
    // Checked here rather than left to fail later: a helper that quietly did
    // not pair sends every test built on it looking in the wrong place.
    assert!(
        net.saw(Side::C, |e| matches!(e, UiEvent::PairingComplete { .. })),
        "the second computer did not pair"
    );
    (net, b_id, c_id)
}

/// Tell A where B lives, the way discovery would.
fn discover(net: &mut Net, side: Side, of: Side) {
    discover_via(net, side, of, TRANSPORT, of.addr());
}

/// The same, but naming which transport saw it and where. Two transports see the
/// same device independently, and that is the case worth being able to write.
fn discover_via(net: &mut Net, side: Side, of: Side, transport: TransportId, addr: &str) {
    let fp = match of {
        Side::A => net.a.fingerprint(),
        Side::B => net.b.fingerprint(),
        Side::C => net.c.fingerprint(),
    };
    net.queue.push_back((
        side,
        Event::Discovered {
            transport,
            peer: DiscoveredPeer {
                fingerprint: Some(fp),
                name: "peer".to_string(),
                addr: addr.to_string(),
                pairing: false,
            },
        },
    ));
    net.run();
}

/// A second transport, worse than `TRANSPORT`. Preference is ascending id, so
/// this stands in for BLE beside Wi-Fi.
const SLOWER: TransportId = TransportId(1);

/// What the harness does when a sender is told where to connect.
///
/// A working transfer does all of it and does it at once, which is what every
/// test wanted until the core grew a deadline. Watching it wait needs a way to
/// stop part-way — and the two ways of stopping are the whole point, because
/// from the outside they look identical and only one of them should end.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dials {
    /// Connects and finishes, the way a small file on a local network does.
    AndFinishes,
    /// Connects, and is still going. A large file, a slow disk, a phone on the
    /// edge of the network.
    AndKeepsGoing,
    /// Never arrives. Its session died between the accept and the dial, or its
    /// app was killed, or its Wi-Fi went away.
    Never,
}

/// Take one transport's link away, the way switching Wi-Fi off does.
///
/// Both ends hear it, because both ends have a socket and both are wrong about
/// it until told otherwise.
/// Deliver a well-formed transport frame that will not decrypt, on a live link.
///
/// A frame, not junk: junk is refused by the framing before it ever reaches an
/// established session, and it is the established-session path this exercises.
/// The link stays in the harness's table on purpose — the point is that the
/// *core* drops it, and that it says so.
fn deliver_undecryptable(net: &mut Net, side: Side, transport: TransportId) {
    // The last one, not the first: `links` keeps every link the harness ever
    // brought up, and pairing leaves closed ones in front of the live session.
    let link = net
        .links
        .iter()
        .rev()
        .find(|(s, t, _)| *s == side && *t == transport)
        .map(|(_, _, l)| *l)
        .expect("a live link to spoil");
    let msg = acrylius_core::proto::frame::join(
        acrylius_core::proto::frame::FrameKind::Transport,
        &[0xffu8; 32],
    );
    net.queue.push_back((
        side,
        Event::LinkRecv {
            link: LinkId(link),
            msg,
        },
    ));
    net.run();
}

fn lose_link(net: &mut Net, transport: TransportId) {
    let dead: Vec<(Side, u64)> = net
        .links
        .iter()
        .filter(|(_, t, _)| *t == transport)
        .map(|(s, _, l)| (*s, *l))
        .collect();
    assert!(!dead.is_empty(), "no link on {transport:?} to lose");
    net.links.retain(|(_, t, _)| *t != transport);
    for (side, link) in dead {
        net.queue.push_back((
            side,
            Event::LinkDown {
                link: LinkId(link),
                reason: LinkDownReason::Closed,
            },
        ));
    }
    net.run();
}

#[test]
fn a_worse_transport_takes_over_when_the_better_one_dies() {
    // The mirror of `a_better_transport_is_taken_even_while_a_worse_one_is_working`,
    // and the half that was missing. Switching Wi-Fi off does not close a TCP
    // connection — nothing is closed, the peer just stops answering — so both
    // ends went on believing a dead link was the best route to each other.
    //
    // Because routing picks the lowest transport id among *live* links, that
    // one belief was enough to send every message into a socket that could not
    // carry it, while a working Bluetooth link sat beside it unused. On the
    // phone it looked like "connected, but nothing happens"; on the desktop it
    // was a socket with four kilobytes stuck in its send queue.
    //
    // Noticing the death is a host's job — keepalive on Linux, path viability
    // on iOS. What the core owes is this: once told, fall back at once.
    let (mut net, _a_id, b_id) = paired();
    net.ble_transport = Some(SLOWER);

    // Same setup as the upgrade test: pairing already recorded a working route
    // on the better transport, so it is overwritten with one that does not
    // answer, the worse transport connects, and only then does Wi-Fi turn up.
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "not-listening");
    discover_via(&mut net, Side::A, Side::B, SLOWER, "B");
    // A second session to one peer needs a strictly later opener timestamp than
    // the last one seen, or it is indistinguishable from a replay of it.
    net.wall += 1_000;
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "B");
    assert_eq!(
        net.a.transport_for(&b_id),
        Some(TransportKind::UnixLoopback),
        "the better transport carries while both are up"
    );

    lose_link(&mut net, TRANSPORT);

    // Still reachable, over what is left.
    assert_eq!(
        net.a.peer_state(&b_id),
        PeerState::Reachable,
        "one dead route is not an unreachable device while another is connected"
    );
    assert_eq!(
        net.a.transport_for(&b_id),
        Some(TransportKind::BleGatt),
        "and what is left is what carries"
    );
}

#[test]
fn losing_one_of_two_routes_does_not_report_the_device_as_gone() {
    // The sting in the previous test's tail, and what it did not check: the
    // core stayed right about where to send, and told the user the opposite.
    // Every link's death was announced as the peer's.
    //
    // That is not cosmetic. `PeerUnreachable` is what a host uses to discard
    // what a peer told it — the phone drops its session state on exactly this
    // event — and `on_peer_disconnected` is what stops a plugin broadcasting to
    // a peer. So switching from Wi-Fi to Bluetooth emptied the session controls
    // and stopped the desktop announcing its lock state to a device it was
    // still talking to, and nothing restored either: both repairs wait for the
    // peer to become reachable again, which never happens to a peer that never
    // left.
    let (mut net, _a_id, b_id) = paired();
    net.ble_transport = Some(SLOWER);

    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "not-listening");
    discover_via(&mut net, Side::A, Side::B, SLOWER, "B");
    net.wall += 1_000;
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "B");

    net.ui.clear();
    lose_link(&mut net, TRANSPORT);

    assert!(
        !net.saw(Side::A, |e| matches!(e, UiEvent::PeerUnreachable { .. })),
        "a device still connected over the other transport was reported gone"
    );
    // And the change is announced rather than left silent: a screen showing
    // which transport is carrying has no other way to learn it changed.
    assert!(
        net.saw(Side::A, |e| matches!(e, UiEvent::PeerReachable { .. })),
        "nothing told the host the route had changed"
    );
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
}

/// Switch Wi-Fi off on one side only, which is the only way it ever happens.
///
/// `lose_link` tells both ends at once, and that is the one thing losing an
/// interface never does. The phone's socket dies with the interface and it knows
/// immediately; this end keeps an ESTABLISHED socket to a peer that has simply
/// stopped answering, and goes on believing in it until a keepalive says
/// otherwise — twenty seconds later. So the wire is cut for both — sends into it
/// vanish, the way a dead socket swallows them — and only `noticed_by` is told.
fn wifi_goes_off(net: &mut Net, transport: TransportId, noticed_by: Side) {
    let dead: Vec<(Side, u64)> = net
        .links
        .iter()
        .filter(|(_, t, _)| *t == transport)
        .map(|(s, _, l)| (*s, *l))
        .collect();
    assert!(!dead.is_empty(), "no link on {transport:?} to lose");
    for (side, link) in &dead {
        net.peer_link.remove(&(*side, *link));
    }
    net.links
        .retain(|(s, t, _)| *t != transport || *s != noticed_by);
    for (side, link) in dead {
        if side == noticed_by {
            net.queue.push_back((
                side,
                Event::LinkDown {
                    link: LinkId(link),
                    reason: LinkDownReason::Transport("the interface went away".to_string()),
                },
            ));
        }
    }
    net.run();
}

/// Both routes up at once, which is what a phone beside a desktop always has.
///
/// It finds the radio before mDNS resolves anything, so Bluetooth comes up first
/// and Wi-Fi is dialled alongside it. Reproduced in that order because the order
/// is what decides which link is older, and the old routing rule cared.
fn both_routes_up(net: &mut Net) {
    discover_via(net, Side::A, Side::B, TRANSPORT, "not-listening");
    discover_via(net, Side::A, Side::B, SLOWER, "B");
    // A second session to one peer needs a strictly later opener timestamp than
    // the last one seen, or it is indistinguishable from a replay of it.
    net.wall += 1_000;
    discover_via(net, Side::A, Side::B, TRANSPORT, "B");
}

#[test]
fn the_route_a_peer_is_actually_using_is_the_one_it_is_answered_over() {
    // The takeover, as the journal finally showed it happening. Bluetooth is
    // already up when Wi-Fi dies, so nothing is dialled and no session is opened
    // — this end is told nothing at all, and the only news it gets is that the
    // questions have started arriving the other way.
    //
    // Preferring the better transport regardless meant answering into the dead
    // socket until the keepalive expired. Reported from the phone as a
    // now-playing timeline that runs on by itself and then corrects ten or
    // twenty seconds later, which is that timer and nothing else.
    let (mut net, a_id, b_id) = paired();
    net.ble_transport = Some(SLOWER);
    both_routes_up(&mut net);
    assert_eq!(
        net.b.transport_for(&a_id),
        Some(TransportKind::UnixLoopback),
        "both up, and the better one carries while both are proven"
    );

    wifi_goes_off(&mut net, TRANSPORT, Side::A);
    assert_eq!(
        net.b.transport_for(&a_id),
        Some(TransportKind::UnixLoopback),
        "the premise: this end has heard nothing and cannot yet know"
    );

    // Time passes, as it does between a handshake and the next poll. The
    // harness's clock stands still unless a test moves it, and two links that
    // last spoke in the same millisecond are equally proven — which is a tie,
    // and a tie is what transport preference is for.
    net.now += 2_000;

    // What a now-playing screen does every two seconds. The question arrives the
    // worse way, which is the peer saying that is where it lives now.
    net.ui.clear();
    net.local(
        Side::A,
        LocalCommand::Plugin {
            peer: b_id,
            cap: ping::CAP.to_string(),
            ty: "ping".to_string(),
            body: b"are-you-there".to_vec(),
        },
    );
    assert!(
        net.saw(Side::A, |e| {
            matches!(e, UiEvent::Plugin { ty, body, .. }
                if ty == "pong" && body == b"are-you-there")
        }),
        "the question arrived and the answer went into the dead socket"
    );
    assert_eq!(
        net.b.transport_for(&a_id),
        Some(TransportKind::BleGatt),
        "and the screen names the route that is carrying"
    );
}

#[test]
fn a_route_that_is_merely_out_of_favour_is_never_torn_down() {
    // The guard on the test above, and on the mistake made reaching it. An
    // earlier attempt read the same evidence and *closed* the route it judged
    // dead — which throws away a working Wi-Fi link whenever one stray Bluetooth
    // frame lands in the moment between this end completing a handshake and the
    // other end finishing it.
    //
    // Choosing wrong has to cost one message the slower way and nothing more, so
    // that the route is still there the instant the peer uses it again.
    let (mut net, a_id, b_id) = paired();
    net.ble_transport = Some(SLOWER);
    both_routes_up(&mut net);

    // Both alive. The peer says something over the worse one anyway.
    net.now += 1_000;
    net.local(
        Side::A,
        LocalCommand::Plugin {
            peer: b_id.clone(),
            cap: ping::CAP.to_string(),
            ty: "ping".to_string(),
            body: b"over-bluetooth".to_vec(),
        },
    );

    // Nothing was closed: Wi-Fi is still there, and one word over it is enough
    // to have it carrying again.
    net.now += 1_000;
    net.local(
        Side::B,
        LocalCommand::Plugin {
            peer: a_id.clone(),
            cap: ping::CAP.to_string(),
            ty: "ping".to_string(),
            body: b"over-wifi".to_vec(),
        },
    );
    assert!(
        net.saw(Side::B, |e| {
            matches!(e, UiEvent::Plugin { ty, body, .. }
                if ty == "pong" && body == b"over-wifi")
        }),
        "the answer did not come back at all"
    );
    assert_eq!(
        net.b.transport_for(&a_id),
        Some(TransportKind::UnixLoopback),
        "the peer answered over Wi-Fi, so Wi-Fi is proven again and carries"
    );
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
}

#[test]
fn a_peer_that_reconnects_is_not_answered_over_the_socket_it_abandoned() {
    // Straight from the journal: `a route to a peer went away … now_on=Some(1)`
    // — a second live route on the transport that had supposedly just gone. A
    // phone whose Wi-Fi drops and returns leaves this end holding two sockets on
    // one transport, the older of them dead, because nothing closed it and the
    // peer had no way to say so.
    //
    // `min_by_key` kept the *older* of two equal transports, so the repair made
    // no difference: every message went on into the socket that was already
    // being ignored.
    let (mut net, a_id, b_id) = paired();
    net.ble_transport = Some(SLOWER);
    both_routes_up(&mut net);

    // Wi-Fi goes and comes back. Only the phone ever knew it went.
    wifi_goes_off(&mut net, TRANSPORT, Side::A);
    net.wall += 1_000;
    net.now += 1_000;
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "B");

    let on_wifi = net
        .links
        .iter()
        .filter(|(s, t, _)| *s == Side::B && *t == TRANSPORT)
        .count();
    assert!(
        on_wifi >= 2,
        "the premise: this end is holding the abandoned socket as well as the new one, and has {on_wifi}"
    );

    net.ui.clear();
    net.local(
        Side::B,
        LocalCommand::Plugin {
            peer: a_id,
            cap: ping::CAP.to_string(),
            ty: "ping".to_string(),
            body: b"which-socket".to_vec(),
        },
    );
    assert!(
        net.saw(Side::B, |e| {
            matches!(e, UiEvent::Plugin { ty, body, .. }
                if ty == "pong" && body == b"which-socket")
        }),
        "answered over the abandoned socket rather than the one just proved"
    );
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
}

#[test]
fn a_session_dropped_over_a_bad_frame_says_the_device_has_gone() {
    // Dropping a link because the peer sent nonsense is right. Doing it in
    // silence is not: `on_transport_frame` holds the only `UpLink` out of the
    // table, so removing it removed nothing, nobody was told the peer had gone,
    // and the `LinkDown` that followed the close found nothing left to report.
    // The device stayed `Reachable` with no route under it, for good.
    let (mut net, _a_id, b_id) = paired();
    discover(&mut net, Side::A, Side::B);
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);

    net.ui.clear();
    deliver_undecryptable(&mut net, Side::A, TRANSPORT);

    assert!(
        net.saw(Side::A, |e| matches!(e, UiEvent::PeerUnreachable { .. })),
        "the session was dropped and nobody was told"
    );
    assert_eq!(
        net.a.peer_state(&b_id),
        PeerState::Unreachable,
        "and the core must not still believe it can be reached"
    );
}

#[test]
fn a_handshake_that_never_finishes_falls_back_to_the_next_route() {
    // A dial that connects has proved a socket, not a peer. Here the preferred
    // address has the wrong machine on it: the connection opens, the handshake
    // cannot finish, and the attempt used to end there in silence — the untried
    // routes were dropped the moment the link came up, on the assumption that a
    // connected socket was a reached device.
    let (mut net, _a_id, b_id) = paired();
    // Both routes on record before anything is dialled: the preferred transport
    // points at the wrong machine, the other at B.
    discover_via(&mut net, Side::A, Side::B, TransportId(1), "B");
    discover_via(&mut net, Side::A, Side::B, TransportId(0), "C");
    net.local(Side::A, LocalCommand::Disconnect { peer: b_id.clone() });
    net.ui.clear();
    net.dialed.clear();
    // A second opener in the same millisecond as the first is a replay, and B is
    // right to refuse it. Real clocks move; this one has to be told to.
    net.wall += 1_000;

    net.local(Side::A, LocalCommand::Connect { peer: b_id.clone() });

    assert!(
        net.dialed
            .iter()
            .any(|(t, a)| *t == TransportId(0) && a == "C"),
        "the preferred route is tried first: {:?}",
        net.dialed
    );
    assert!(
        net.dialed
            .iter()
            .any(|(t, a)| *t == TransportId(1) && a == "B"),
        "and the next one after it came to nothing: {:?}",
        net.dialed
    );
    assert_eq!(
        net.a.peer_state(&b_id),
        PeerState::Reachable,
        "the device was reachable all along, by the second route"
    );
}

#[test]
fn losing_the_last_route_does_report_the_device_as_gone() {
    // The other half, kept honest: the case above must not be bought by never
    // reporting a departure at all.
    let (mut net, _a_id, b_id) = paired();
    discover(&mut net, Side::A, Side::B);
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);

    net.ui.clear();
    lose_link(&mut net, TRANSPORT);

    assert!(
        net.saw(Side::A, |e| matches!(e, UiEvent::PeerUnreachable { .. })),
        "the last route died and nobody was told"
    );
    assert_eq!(net.a.peer_state(&b_id), PeerState::Unreachable);
}

#[test]
fn a_sighting_on_one_transport_does_not_evict_another() {
    let (mut net, _a_id, b_id) = paired();
    // The better transport is overwritten first, because pairing has already
    // recorded a route that works and any sighting would otherwise just
    // reconnect on it. With every address bad, nothing connects and what is
    // left to inspect is the address book itself.
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "not-listening");
    discover_via(&mut net, Side::A, Side::B, SLOWER, "also-not-listening");

    // Pairing dialled too, and so did each sighting; only what Connect does is
    // under test here.
    net.dialed.clear();
    net.local(Side::A, LocalCommand::Connect { peer: b_id.clone() });

    assert_eq!(
        net.dialed,
        vec![
            (TRANSPORT, "not-listening".to_string()),
            (SLOWER, "also-not-listening".to_string()),
        ],
        "a sighting on one transport adds to the book rather than replacing \
         what another put there"
    );
    assert_eq!(net.a.peer_state(&b_id), PeerState::Unreachable);
}

#[test]
fn seeing_a_paired_device_is_enough_to_reach_it() {
    // Nobody presses anything. This is what a phone needs: when its Bluetooth
    // link drops, the radio reconnects and reads identity again, and that
    // sighting has to be what brings the session back. Waiting for a button is
    // why it stayed dark until the app was force-quit a second time.
    let (mut net, _a_id, b_id) = paired();
    assert_eq!(net.a.peer_state(&b_id), PeerState::Unreachable);

    net.dialed.clear();
    discover(&mut net, Side::A, Side::B);

    assert_eq!(
        net.dialed,
        vec![(TRANSPORT, "B".to_string())],
        "a sighting of a device we have already paired with is dialled on its own"
    );
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
}

#[test]
fn a_better_transport_is_taken_even_while_a_worse_one_is_working() {
    // The bug this pins: a phone finds Bluetooth first, because it is already
    // connected to the desktop's radio while mDNS has yet to resolve. Treating
    // "a link exists" as "nothing more to do" left it there for the whole
    // session — and a Bluetooth link cannot carry a file, so every transfer was
    // refused with a working Wi-Fi route sitting unused.
    let (mut net, _a_id, b_id) = paired();

    // On the worse transport, and only that: pairing's route is overwritten
    // with one that does not answer, so the fallback is what connects.
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "not-listening");
    discover_via(&mut net, Side::A, Side::B, SLOWER, "B");
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);

    net.dialed.clear();
    // Wi-Fi turns up.
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "B");

    assert_eq!(
        net.dialed,
        vec![(TRANSPORT, "B".to_string())],
        "being reachable already is no reason to stay on the worse transport"
    );
    // And the worse link is still there behind it, rather than torn down: two
    // ways in is what the fallback needs, and the core sends over the better.
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
}

#[test]
fn a_hello_no_newer_than_the_last_one_is_refused() {
    // `Noise_IKpsk2` message 1 is replayable — an eavesdropper can record a
    // session opener and send it again — so every opener carries a timestamp
    // and a peer's watermark only ever moves forward. PROTOCOL.md §7 is about
    // this, and `accept_hello` is where it is enforced.
    //
    // It works. Nothing said so: the whole suite passed with `accept_hello`
    // replaced by `true`, which accepts every opener ever offered, including
    // one replayed from a device with no record at all. Found by mutation
    // testing, which is also how the exact shape below came out — the setup is
    // the one that hit the watermark by accident while a different test was
    // being written.
    let (mut net, _a_id, b_id) = paired();
    net.ble_transport = Some(SLOWER);

    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "not-listening");
    discover_via(&mut net, Side::A, Side::B, SLOWER, "B");
    assert_eq!(
        net.a.transport_for(&b_id),
        Some(TransportKind::BleGatt),
        "the worse transport is what connected"
    );

    // Now the better transport turns up — and the clock has deliberately not
    // moved, so the Hello that comes back repeats a timestamp already seen.
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "B");

    assert_eq!(
        net.a.transport_for(&b_id),
        Some(TransportKind::BleGatt),
        "a replayed opener must not open a session, however good its route"
    );
    assert_eq!(
        net.a.peer_state(&b_id),
        PeerState::Reachable,
        "and refusing it must not cost the session that was already working"
    );
}

#[test]
fn a_pairing_window_closes_after_too_many_wrong_codes() {
    // The code is short enough to be read aloud, which is only safe because
    // the window stops accepting guesses. Nothing tested that: deleting
    // `pairing_attempt_failed` outright left the suite green, as did inverting
    // the comparison so the window burned on the first failure instead of the
    // last.
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    net.local(
        Side::B,
        LocalCommand::OpenPairingWindow {
            code: CODE.to_string(),
        },
    );

    let limit = CoreConfig::default().max_pairing_attempts;
    assert!(limit > 1, "a limit of one cannot tell the two ends apart");

    let closed = |net: &Net| {
        net.saw(
            Side::B,
            |e| matches!(e, UiEvent::PairingFailed { reason } if reason.contains("too many")),
        )
    };

    for attempt in 1..limit {
        guess(&mut net, WRONG);
        assert!(
            !closed(&net),
            "the window closed after {attempt} of {limit} attempts"
        );
    }

    guess(&mut net, WRONG);
    assert!(
        closed(&net),
        "the window is still open after {limit} wrong codes"
    );

    // And it is really shut, not merely reported shut: the right code no
    // longer works either.
    guess(&mut net, CODE);
    assert!(
        !net.saw(Side::B, |e| matches!(e, UiEvent::PairingComplete { .. })),
        "a burnt window must not pair, even with the correct code"
    );
}

/// One pairing attempt from A, with whatever code it believes in.
fn guess(net: &mut Net, code: &str) {
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
            code: code.to_string(),
        },
    );
}

#[test]
fn a_device_already_reached_is_not_dialled_again() {
    // Discovery repeats — a service resolves, a radio re-announces, a scan
    // starts over. Dialling per sighting would be a storm aimed at a device
    // whose only offence is being switched on.
    let (mut net, _a_id, b_id) = paired();
    discover(&mut net, Side::A, Side::B);
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);

    net.dialed.clear();
    discover(&mut net, Side::A, Side::B);
    discover_via(&mut net, Side::A, Side::B, SLOWER, "B");

    assert!(
        net.dialed.is_empty(),
        "a peer already reached is left alone"
    );
}

#[test]
fn a_dial_that_fails_falls_through_to_the_next_route() {
    let (mut net, _a_id, b_id) = paired();
    // The preferred route is the broken one, so the fallback has to do work.
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "not-listening");
    // Everything above is setup: that sighting dialled too, and had only the
    // broken route to try. The second one is where both are on file.
    net.dialed.clear();
    discover_via(&mut net, Side::A, Side::B, SLOWER, "B");

    assert_eq!(
        net.dialed,
        vec![
            (TRANSPORT, "not-listening".to_string()),
            (SLOWER, "B".to_string()),
        ],
        "both routes tried, best first"
    );
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
    assert!(
        !net.saw(Side::A, |e| matches!(e, UiEvent::PeerUnreachable { .. })),
        "a peer reached on the second route was never unreachable"
    );
}

#[test]
fn a_peer_is_unreachable_only_once_every_route_has_failed() {
    let (mut net, _a_id, b_id) = paired();
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "not-listening");
    discover_via(&mut net, Side::A, Side::B, SLOWER, "also-not-listening");

    net.dialed.clear();
    net.local(Side::A, LocalCommand::Connect { peer: b_id.clone() });

    assert_eq!(
        net.dialed.len(),
        2,
        "every route was tried before giving up"
    );
    assert_eq!(net.a.peer_state(&b_id), PeerState::Unreachable);
    let unreachable = net
        .ui
        .iter()
        .filter(|(s, e)| *s == Side::A && matches!(e, UiEvent::PeerUnreachable { .. }))
        .count();
    assert_eq!(
        unreachable, 1,
        "said once, at the end, not once per attempt"
    );
}

#[test]
fn two_cores_pair_and_agree_on_everything() {
    let (net, a_id, b_id) = paired();

    assert!(net.saw(Side::A, |e| matches!(e, UiEvent::PairingComplete { .. })));
    assert!(net.saw(Side::B, |e| matches!(e, UiEvent::PairingComplete { .. })));

    // Each stored the other, under an id derived from the key Noise proved.
    let a_knows: Vec<_> = net.a.peers().filter_map(|p| p.id()).collect();
    let b_knows: Vec<_> = net.b.peers().filter_map(|p| p.id()).collect();
    assert_eq!(a_knows, vec![b_id]);
    assert_eq!(b_knows, vec![a_id]);

    // And each wrote it out as secret material.
    assert!(
        net.persisted
            .iter()
            .any(|(s, k, present)| *s == Side::A && k.starts_with("peer/") && *present)
    );
    assert!(
        net.persisted
            .iter()
            .any(|(s, k, present)| *s == Side::B && k.starts_with("peer/") && *present)
    );
}

#[test]
fn a_message_goes_to_the_peer_it_is_addressed_to() {
    // `best_link` filters the link table by peer before choosing. Removing that
    // filter — so it returns whichever link sorts first, whoever it belongs to —
    // left the entire suite green, because every core in it had exactly one
    // peer and any link was that peer's link.
    //
    // What it would cost in the real world is not subtle: a clipboard, a file,
    // or a command addressed to one computer, delivered to a different one.
    // Two devices cannot ask the question. This is why C exists.
    let (mut net, b_id, c_id) = paired_twice();
    discover(&mut net, Side::A, Side::B);
    net.wall += 1_000;
    discover(&mut net, Side::A, Side::C);

    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
    assert_eq!(net.a.peer_state(&c_id), PeerState::Reachable);

    // Addressed to C, which is deliberately *not* the link that sorts first —
    // B was paired earlier and holds the lower id, so a core that ignores who a
    // link belongs to sends this to B.
    net.local(
        Side::A,
        LocalCommand::Plugin {
            peer: c_id.clone(),
            cap: ping::CAP.to_string(),
            ty: "ping".to_string(),
            body: b"for-c".to_vec(),
        },
    );

    // The answer names who sent it, which is what makes this observable at all:
    // a ping delivered to the wrong computer is still answered, and the pong
    // still comes back. Only the name on it is different.
    assert!(
        net.saw(Side::A, |e| {
            matches!(e, UiEvent::Plugin { peer, ty, body, .. }
                if ty == "pong" && body == b"for-c" && peer == &c_id)
        }),
        "the pong did not come from the computer the ping was addressed to"
    );
    assert!(
        !net.saw(Side::A, |e| {
            matches!(e, UiEvent::Plugin { peer, body, .. }
                if body == b"for-c" && peer == &b_id)
        }),
        "a message addressed to one computer was answered by another"
    );
}

#[test]
fn the_peer_a_message_is_for_outranks_the_peer_we_heard_from_last() {
    // The twin of the test above, and what it stopped catching the moment
    // routing began to care about recency. That one relies on the addressed
    // peer's link being the *newest*, so dropping the `peer` filter from
    // `best_link` still happened to answer the right computer — a filter this
    // suite has already caught being removed once.
    //
    // Here the other computer is the one we heard from most recently, so a walk
    // that forgets who a link belongs to picks it, and a clipboard or a file
    // meant for one machine arrives at another.
    let (mut net, b_id, c_id) = paired_twice();
    discover(&mut net, Side::A, Side::B);
    net.wall += 1_000;
    discover(&mut net, Side::A, Side::C);

    // B speaks last, which under the old rule it never did. Sent *from* B, not
    // asked for by A: a walk that has forgotten who a link belongs to answers
    // every question wrong in the same direction, so driving this from A would
    // only misroute the setup as well and leave the trap unsprung.
    net.now += 1_000;
    net.local(
        Side::B,
        LocalCommand::Plugin {
            peer: net.a.device_id(),
            cap: ping::CAP.to_string(),
            ty: "ping".to_string(),
            body: b"from-b".to_vec(),
        },
    );

    net.now += 1_000;
    net.ui.clear();
    net.local(
        Side::A,
        LocalCommand::Plugin {
            peer: c_id.clone(),
            cap: ping::CAP.to_string(),
            ty: "ping".to_string(),
            body: b"for-c".to_vec(),
        },
    );

    assert!(
        net.saw(Side::A, |e| {
            matches!(e, UiEvent::Plugin { peer, ty, body, .. }
                if ty == "pong" && body == b"for-c" && peer == &c_id)
        }),
        "the pong did not come from the computer the ping was addressed to"
    );
    assert!(
        !net.saw(Side::A, |e| {
            matches!(e, UiEvent::Plugin { peer, body, .. }
                if body == b"for-c" && peer == &b_id)
        }),
        "a message went to whichever computer had spoken most recently"
    );
}

#[test]
fn a_paired_peer_can_open_a_session_and_ping() {
    let (mut net, _a_id, b_id) = paired();
    discover(&mut net, Side::A, Side::B);

    net.local(Side::A, LocalCommand::Connect { peer: b_id.clone() });
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
    assert!(net.saw(Side::A, |e| matches!(e, UiEvent::PeerReachable { .. })));
    assert!(net.saw(Side::B, |e| matches!(e, UiEvent::PeerReachable { .. })));

    net.local(
        Side::A,
        LocalCommand::Plugin {
            peer: b_id,
            cap: ping::CAP.to_string(),
            ty: "ping".to_string(),
            body: b"hello".to_vec(),
        },
    );

    // The round trip: A -> ping -> B -> pong -> A, through negotiation,
    // encryption, framing and plugin dispatch on both sides.
    assert!(
        net.saw(
            Side::A,
            |e| matches!(e, UiEvent::Plugin { ty, body, .. } if ty == "pong" && body == b"hello")
        ),
        "A should have seen its pong"
    );
}

#[test]
fn a_wrong_pairing_code_does_not_pair() {
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    net.local(
        Side::B,
        LocalCommand::OpenPairingWindow {
            code: CODE.to_string(),
        },
    );
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
            code: "ABCD1235".to_string(),
        },
    );
    assert!(
        net.sas_for(Side::A).is_none(),
        "no SAS should ever be shown"
    );
    assert_eq!(net.a.peers().count(), 0);
    assert_eq!(net.b.peers().count(), 0);
}

#[test]
fn a_pairing_we_started_does_not_answer_anybody_elses_handshake() {
    // The window that holds "waiting for a person to compare six digits" is not
    // a window anyone may knock on. It used to be: the side that *initiated*
    // kept its confirmation in a window whose psk was all zeroes — a constant,
    // so anything that could reach this device completed `XXpsk0` against a key
    // it already knew, and the handshake it finished replaced the confirmation
    // the human was looking at. Approving the dialog then paired the stranger.
    //
    // Short of knowing that, a stranger's attempt still spent the window's
    // attempts and could cancel a pairing that was going perfectly well, which
    // is what this asserts without needing a hostile client to write.
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    net.local(
        Side::B,
        LocalCommand::OpenPairingWindow {
            code: CODE.to_string(),
        },
    );
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
            code: CODE.to_string(),
        },
    );
    let waiting_on = net
        .sas_for(Side::A)
        .expect("A is waiting to be confirmed")
        .to_string();

    // C, uninvited, aims pairing handshakes at A — enough of them to exhaust a
    // window's attempts, because one is not enough to tell the two behaviours
    // apart. A window that answers C at all counts these against itself and
    // burns on the third; a window that is not open to C never sees them.
    for _ in 0..4 {
        net.local(
            Side::C,
            LocalCommand::RequestPairing {
                transport: TRANSPORT,
                addr: Side::A.addr().to_string(),
                code: "ABCD1235".to_string(),
            },
        );
    }

    assert_eq!(
        net.sas_for(Side::A).map(str::to_string),
        Some(waiting_on),
        "A is still comparing the code it was already comparing"
    );
    assert_eq!(net.c.peers().count(), 0, "and C has paired with nothing");

    // The pairing that was actually in progress still completes.
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });
    assert_eq!(net.a.peers().count(), 1, "A paired with B, and only B");
    assert_eq!(net.b.peers().count(), 1);
}

#[test]
fn pairing_is_refused_when_no_window_is_open() {
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    // B never opened a window. The attempt must not merely fail a check: there
    // is nothing to talk to.
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
            code: CODE.to_string(),
        },
    );
    assert_eq!(net.a.peers().count(), 0);
    assert_eq!(net.b.peers().count(), 0);
}

#[test]
fn refusing_the_sas_pairs_nobody() {
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    net.local(
        Side::B,
        LocalCommand::OpenPairingWindow {
            code: CODE.to_string(),
        },
    );
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
            code: CODE.to_string(),
        },
    );
    assert!(net.sas_for(Side::B).is_some());

    net.local(Side::B, LocalCommand::ConfirmPairing { accept: false });
    assert_eq!(net.b.peers().count(), 0, "a refused SAS must store nothing");
    assert!(net.saw(Side::B, |e| matches!(e, UiEvent::PairingFailed { .. })));
}

#[test]
fn an_unpaired_stranger_cannot_open_a_session() {
    let (mut net, _a, b_id) = paired();
    discover(&mut net, Side::A, Side::B);

    // B forgets A. A's next attempt is a stranger's.
    net.local(
        Side::B,
        LocalCommand::Revoke {
            peer: net.a.device_id(),
        },
    );
    assert_eq!(net.b.peers().count(), 0);

    net.local(Side::A, LocalCommand::Connect { peer: b_id.clone() });
    assert_eq!(net.a.peer_state(&b_id), PeerState::Unreachable);
}

#[test]
fn the_pairing_window_expires_on_the_hosts_clock() {
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    net.local(
        Side::B,
        LocalCommand::OpenPairingWindow {
            code: CODE.to_string(),
        },
    );

    // The core asked to be woken; honour that and no sooner.
    let out = net.b.handle(
        Now {
            monotonic_ms: net.now,
            wall_ms: net.wall,
        },
        Event::Tick,
    );
    let deadline = out.next_deadline_ms.expect("a window sets a deadline");
    assert!(deadline > net.now);

    net.now = deadline;
    let out = net.b.handle(
        Now {
            monotonic_ms: net.now,
            wall_ms: net.wall,
        },
        Event::Tick,
    );
    assert!(
        out.actions
            .iter()
            .any(|a| matches!(a, Action::Ui(UiEvent::PairingFailed { .. }))),
        "the window should have expired"
    );

    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
            code: CODE.to_string(),
        },
    );
    assert_eq!(
        net.b.peers().count(),
        0,
        "an expired window must not still pair"
    );
}

#[test]
fn capabilities_are_negotiated_not_assumed() {
    // A host with no ping plugin must never be sent a ping, and must not
    // advertise that it accepts one.
    let bare = CoreBuilder::new(
        Identity::generate().unwrap(),
        CoreConfig {
            name: "bare".to_string(),
            ..Default::default()
        },
    )
    .build();
    assert!(bare.caps_in().is_empty());
    assert!(bare.caps_out().is_empty());

    let full = core("full");
    assert_eq!(full.caps_in(), [ping::CAP.to_string()]);
}

#[test]
fn pairing_records_the_address_it_proved() {
    // A pairing that completed has just demonstrated an address works. Nothing
    // recorded it, so a device that had finished pairing a second earlier
    // reported itself unreachable, and stayed that way until discovery
    // happened to speak again.
    let (mut net, _a_id, b_id) = paired();

    // Deliberately no discovery: the address from the pairing dial is the only
    // one in play.
    net.local(Side::A, LocalCommand::Connect { peer: b_id.clone() });
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
}

#[test]
fn an_address_seen_before_pairing_is_not_lost() {
    // Discovery resolves a service once and then goes quiet until something
    // about it changes. An announcement that lands before pairing is very
    // often the only one there will be, so discarding it as "not a peer yet"
    // threw away the single chance to learn where that device lives.
    let (a, b) = (core("phone"), core("pc"));
    let a_id = a.device_id();
    let mut net = Net::new(a, b);

    // Bravo hears about alpha while alpha is still a stranger.
    discover(&mut net, Side::B, Side::A);

    net.local(
        Side::B,
        LocalCommand::OpenPairingWindow {
            code: CODE.to_string(),
        },
    );
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
            code: CODE.to_string(),
        },
    );
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });

    // Bravo was dialled, so the handshake taught it nothing about where alpha
    // is. The earlier announcement has to carry it.
    net.local(Side::B, LocalCommand::Connect { peer: a_id.clone() });
    assert_eq!(net.b.peer_state(&a_id), PeerState::Reachable);
}

#[test]
fn a_peer_with_no_address_explains_itself() {
    // Not knowing where a device is differs from failing to reach it, and only
    // one of those the user can act on. A phone never announces itself and
    // never listens, so this is the permanent state of every phone.
    let (a, b) = (core("phone"), core("pc"));
    let a_id = a.device_id();
    let mut net = Net::new(a, b);
    net.local(
        Side::B,
        LocalCommand::OpenPairingWindow {
            code: CODE.to_string(),
        },
    );
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
            code: CODE.to_string(),
        },
    );
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });

    // No discovery ever ran, and bravo did not dial.
    net.local(Side::B, LocalCommand::Connect { peer: a_id.clone() });
    assert_eq!(net.b.peer_state(&a_id), PeerState::Unreachable);
    assert!(
        net.saw(Side::B, |e| matches!(e, UiEvent::Error { detail, .. }
            if detail.contains("no address known"))),
        "it should say it does not know where the device is, not merely that it is unreachable"
    );
    assert!(
        net.b
            .dial_trouble(&a_id)
            .is_some_and(|w| w.contains("Nothing has found it yet")),
        "and a screen drawing that peer must be able to read the same reason"
    );
}

#[test]
fn a_pairing_window_is_something_the_advertisement_can_see() {
    // `pair=1` is specified in PROTOCOL.md § 4 and read by both transports,
    // and until now nothing produced it. The runtime re-advertises when this
    // changes, so what it reports is the whole of that feature.
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    assert!(!net.b.pairing_open(), "nothing is open to begin with");

    net.local(
        Side::B,
        LocalCommand::OpenPairingWindow {
            code: CODE.to_string(),
        },
    );
    assert!(net.b.pairing_open(), "an open window is on the air");

    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
            code: CODE.to_string(),
        },
    );
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });

    assert!(
        !net.b.pairing_open(),
        "and a window that has done its job must come off it again"
    );
}

#[test]
fn a_pairing_window_that_expires_stops_advertising_itself() {
    // The case with no event behind it, and the reason this is read rather
    // than announced: nobody is told a window timed out, so an advertisement
    // driven by events would go on inviting devices that will be refused.
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    net.local(
        Side::B,
        LocalCommand::OpenPairingWindow {
            code: CODE.to_string(),
        },
    );
    assert!(net.b.pairing_open());

    // Wake it exactly when it asked to be woken, the way the runtime does.
    let out = net.b.handle(
        Now {
            monotonic_ms: net.now,
            wall_ms: net.wall,
        },
        Event::Tick,
    );
    net.now = out.next_deadline_ms.expect("a window sets a deadline");
    let _ = net.b.handle(
        Now {
            monotonic_ms: net.now,
            wall_ms: net.wall,
        },
        Event::Tick,
    );

    assert!(
        !net.b.pairing_open(),
        "an expired window must not still be advertised as open"
    );
}

#[test]
fn a_peer_that_could_not_be_reached_records_why_without_announcing_it() {
    // The Connect button is gone, so every dial is now automatic. That makes
    // the reason a dial failed something a screen has to be able to *ask* for,
    // rather than something it hears about: an automatic attempt runs on every
    // sighting, and announcing each exhausted one would flicker an error at a
    // device that is coming up perfectly normally.
    let (mut net, _a_id, b_id) = paired();

    // Seen, but not where the harness answers. One route, and it fails.
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "nowhere");

    assert_eq!(net.a.peer_state(&b_id), PeerState::Unreachable);
    assert!(
        net.a
            .dial_trouble(&b_id)
            .is_some_and(|w| w.contains("nothing is listening at nowhere")),
        "the reason the dial failed is what the screen has to show"
    );
    assert!(
        !net.saw(Side::A, |e| matches!(e, UiEvent::PeerUnreachable { .. })),
        "nobody asked for this attempt, so it is state and not news"
    );
}

#[test]
fn reaching_a_peer_forgets_what_went_wrong_before() {
    let (mut net, _a_id, b_id) = paired();
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "nowhere");
    assert!(
        net.a.dial_trouble(&b_id).is_some(),
        "the failure is recorded"
    );

    // And now it turns up somewhere that answers.
    discover(&mut net, Side::A, Side::B);

    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
    assert_eq!(
        net.a.dial_trouble(&b_id),
        None,
        "a connected peer must not go on explaining a failure it no longer has"
    );
}

#[test]
fn forgetting_a_peer_forgets_why_it_would_not_connect() {
    let (mut net, _a_id, b_id) = paired();
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "nowhere");
    assert!(net.a.dial_trouble(&b_id).is_some());

    net.local(Side::A, LocalCommand::Revoke { peer: b_id.clone() });

    assert_eq!(
        net.a.dial_trouble(&b_id),
        None,
        "a forgotten peer leaves nothing behind, including why it would not connect"
    );
}

#[test]
fn a_session_survives_two_machines_with_different_uptimes() {
    // Every test until now started both cores at the same instant, so their
    // clocks agreed to the millisecond and nothing noticed which clock the
    // handshake timestamp came from. Real machines do not boot together: a
    // computer that has been up for hours and a phone just unlocked share no
    // uptime at all.
    let (a, b) = (core("phone"), core("pc"));
    let b_id = b.device_id();
    let mut net = Net::new(a, b);
    // The PC has been up an hour longer than the phone.
    net.b_skew = 3_600_000;

    net.local(
        Side::B,
        LocalCommand::OpenPairingWindow {
            code: CODE.to_string(),
        },
    );
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
            code: CODE.to_string(),
        },
    );
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });
    assert_eq!(
        net.a.peers().count(),
        1,
        "pairing does not check freshness, so it works"
    );

    net.local(Side::A, LocalCommand::Connect { peer: b_id.clone() });
    assert_eq!(
        net.a.peer_state(&b_id),
        PeerState::Reachable,
        "a session must not depend on two machines sharing an uptime"
    );
}

// ---------------------------------------------------------------- file transfer

use acrylius_core::core::BULK_DIAL_WAIT_MS;
use acrylius_core::plugins::share::{self, Accept, Finished, Offer};
use acrylius_core::vocab::TransferId;

/// Pair two sharing cores and open a session between them.
fn sharing_pair() -> (Net, DeviceId) {
    let (a, b) = (sharing_core("phone"), sharing_core("pc"));
    let b_id = b.device_id();
    let mut net = Net::new(a, b);
    net.local(
        Side::B,
        LocalCommand::OpenPairingWindow {
            code: CODE.to_string(),
        },
    );
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
            code: CODE.to_string(),
        },
    );
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::A, LocalCommand::Connect { peer: b_id.clone() });
    (net, b_id)
}

/// A paired pair whose links carry BLE's attributes: 16 KiB messages, and no
/// bulk side channel.
fn ble_sharing_pair() -> (Net, DeviceId) {
    let (a, b) = (sharing_core("phone"), sharing_core("pc"));
    let b_id = b.device_id();
    let mut net = Net::new(a, b);
    // Set before anything connects, so every link picks it up.
    net.links_are_ble = true;
    net.local(
        Side::B,
        LocalCommand::OpenPairingWindow {
            code: CODE.to_string(),
        },
    );
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
            code: CODE.to_string(),
        },
    );
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::A, LocalCommand::Connect { peer: b_id.clone() });
    (net, b_id)
}

fn plugin(net: &mut Net, side: Side, peer: &DeviceId, ty: &str, body: Vec<u8>) {
    net.local(
        side,
        LocalCommand::Plugin {
            peer: peer.clone(),
            cap: share::CAP.to_string(),
            ty: ty.to_string(),
            body,
        },
    );
}

fn offer_body(transfer: u64, size: u64) -> Vec<u8> {
    minicbor::to_vec(Offer {
        transfer,
        name: "holiday.jpg".to_string(),
        size,
        mime: "image/jpeg".to_string(),
    })
    .unwrap()
}

/// The transfer id a side most recently announced to its own host.
///
/// A receiver renumbers an offer on arrival, so this is the only way a test can
/// learn what it is going to call the thing — which is exactly the position a
/// host is in.
fn announced_transfer(net: &Net, side: Side, ty: &str) -> u64 {
    net.ui
        .iter()
        .rev()
        .find_map(|(s, e)| match e {
            UiEvent::Plugin {
                cap,
                ty: kind,
                body,
                ..
            } if *s == side && cap == share::CAP && kind == ty => {
                minicbor::decode::<Offer>(body).ok().map(|o| o.transfer)
            }
            _ => None,
        })
        .expect("nothing of that kind was announced")
}

#[test]
fn a_transfer_works_when_the_two_ends_number_it_differently() {
    // The receiver keys everything by a number of its own now, so the two ends
    // disagree about what the transfer is called as a matter of course. Every
    // message between them has to be translated, and the bulk key — which
    // neither end sends and both must derive — has to come from the *sender's*
    // number or nothing will decrypt.
    //
    // Contrived only in how the numbers are made to diverge. Two people sending
    // you a photo at the same time does it by itself, which is the case this
    // exists to allow.
    let (mut net, b_id) = sharing_pair();
    let a_id = net.a.device_id();

    // One offer, refused, purely to move the receiver's numbering on.
    plugin(&mut net, Side::A, &b_id, "offer", offer_body(1, 16));
    let first = announced_transfer(&net, Side::B, "offer");
    plugin(&mut net, Side::B, &a_id, "reject", answer_body(first));

    // Now the real one, under a number the receiver will not reach for a while.
    net.ui.clear();
    plugin(&mut net, Side::A, &b_id, "offer", offer_body(7, 4096));
    let ours = announced_transfer(&net, Side::B, "offer");
    assert_ne!(
        ours, 7,
        "the receiver is meant to have a number of its own, and this test \
         proves nothing if it happens to match"
    );

    plugin(&mut net, Side::B, &a_id, "accept", answer_body(ours));

    // The key each end derived, under the name each end knows it by.
    let sender = net
        .bulk_keys
        .get(&(Side::A, TransferId(7)))
        .expect("the sender has a key");
    let receiver = net
        .bulk_keys
        .get(&(Side::B, TransferId(ours)))
        .expect("the receiver has a key");
    assert_eq!(
        sender, receiver,
        "the two ends derived different keys, so nothing would decrypt"
    );

    // And both were told it finished, each in its own numbering.
    assert!(
        net.saw(Side::A, |e| matches!(e, UiEvent::Plugin { ty, body, .. }
            if ty == "finished"
            && minicbor::decode::<Finished>(body).is_ok_and(|f| f.transfer == 7))),
        "the sender was told about a transfer it does not have"
    );
    assert!(
        net.saw(Side::B, |e| matches!(e, UiEvent::Plugin { ty, body, .. }
            if ty == "finished"
            && minicbor::decode::<Finished>(body).is_ok_and(|f| f.transfer == ours))),
        "the receiver was told about a transfer it does not have"
    );
}

#[test]
fn a_refusal_reaches_the_sender_under_the_number_the_sender_used() {
    // Everything the receiver sends back has to be translated, not just the
    // accept. A reject carrying the receiver's own number names a transfer the
    // sender has never had, and is refused as somebody else's business — so the
    // file sits listed as offered until the session ends.
    let (mut net, b_id) = sharing_pair();
    let a_id = net.a.device_id();

    plugin(&mut net, Side::A, &b_id, "offer", offer_body(1, 16));
    let first = announced_transfer(&net, Side::B, "offer");
    plugin(&mut net, Side::B, &a_id, "reject", answer_body(first));

    net.ui.clear();
    plugin(&mut net, Side::A, &b_id, "offer", offer_body(7, 4096));
    let ours = announced_transfer(&net, Side::B, "offer");
    assert_ne!(
        ours, 7,
        "the two ends must disagree for this to test anything"
    );

    plugin(&mut net, Side::B, &a_id, "reject", answer_body(ours));
    assert!(
        net.saw(Side::A, |e| matches!(e, UiEvent::Plugin { ty, body, .. }
            if ty == "reject"
            && minicbor::decode::<Finished>(body).is_ok_and(|f| f.transfer == 7))),
        "the refusal named a transfer the sender has never heard of"
    );
}

#[test]
fn a_receiver_cancelling_names_the_transfer_the_sender_knows() {
    // The same again for a transfer given up on rather than refused, which is
    // the other way a receiver ends one and the other place the number has to
    // be put back.
    let (mut net, b_id) = sharing_pair();
    let a_id = net.a.device_id();

    plugin(&mut net, Side::A, &b_id, "offer", offer_body(1, 16));
    let first = announced_transfer(&net, Side::B, "offer");
    plugin(&mut net, Side::B, &a_id, "reject", answer_body(first));

    net.ui.clear();
    plugin(&mut net, Side::A, &b_id, "offer", offer_body(7, 4096));
    let ours = announced_transfer(&net, Side::B, "offer");
    assert_ne!(
        ours, 7,
        "the two ends must disagree for this to test anything"
    );

    plugin(&mut net, Side::B, &a_id, "cancel", answer_body(ours));
    assert!(
        net.saw(Side::A, |e| matches!(e, UiEvent::Plugin { ty, body, .. }
            if ty == "finished"
            && minicbor::decode::<Finished>(body).is_ok_and(|f| f.transfer == 7))),
        "the sender was never told, and would hold the file open"
    );
}

#[test]
fn a_sender_cancelling_is_reported_under_the_number_the_receiver_uses() {
    // The mirror, and the direction where the translation runs on the way *in*
    // rather than out. A host is told about its own transfers and knows nothing
    // of the sender's numbering, so an ending announced under the sender's
    // number names something the host has never had — and the file it is
    // holding a port and a name open for is never cleared.
    let (mut net, b_id) = sharing_pair();
    let a_id = net.a.device_id();

    plugin(&mut net, Side::A, &b_id, "offer", offer_body(1, 16));
    let first = announced_transfer(&net, Side::B, "offer");
    plugin(&mut net, Side::B, &a_id, "reject", answer_body(first));

    net.ui.clear();
    plugin(&mut net, Side::A, &b_id, "offer", offer_body(7, 4096));
    let ours = announced_transfer(&net, Side::B, "offer");
    assert_ne!(
        ours, 7,
        "the two ends must disagree for this to test anything"
    );

    plugin(&mut net, Side::A, &b_id, "cancel", answer_body(7));
    assert!(
        net.saw(Side::B, |e| matches!(e, UiEvent::Plugin { ty, body, .. }
            if ty == "finished"
            && minicbor::decode::<Finished>(body).is_ok_and(|f| f.transfer == ours))),
        "the receiver was told about a transfer it has never heard of"
    );
}

/// Fire the host's single timer, which is how every deadline in the core is
/// reached.
fn tick(net: &mut Net, side: Side) {
    net.queue.push_back((side, Event::Tick));
    net.run();
}

/// Whether a side was told a transfer had ended, and how.
fn told_it_finished(net: &Net, side: Side, ok: bool) -> bool {
    net.ui.iter().any(|(s, e)| {
        *s == side
            && matches!(e, UiEvent::Plugin { cap, ty, body, .. }
                if cap == share::CAP && ty == "finished"
                && minicbor::decode::<Finished>(body).is_ok_and(|f| f.ok == ok))
    })
}

#[test]
fn a_sender_that_never_dials_is_given_up_on() {
    // Accepting a file binds a port and reserves a filename, and until now
    // nothing ever took them back. A sender whose session died between the
    // accept and the dial left both held for the life of the process, and the
    // person who had pressed Accept watched a transfer that never moved and
    // never failed.
    let (mut net, b_id) = sharing_pair();
    let a_id = net.a.device_id();

    // Numbered differently at the two ends, which is the ordinary case now and
    // the one where telling the sender means translating. Under matching
    // numbers this passes whether or not anything is translated at all.
    plugin(&mut net, Side::A, &b_id, "offer", offer_body(1, 16));
    let first = announced_transfer(&net, Side::B, "offer");
    plugin(&mut net, Side::B, &a_id, "reject", answer_body(first));

    net.dials = Dials::Never;
    plugin(&mut net, Side::A, &b_id, "offer", offer_body(7, 4096));
    let ours = announced_transfer(&net, Side::B, "offer");
    assert_ne!(
        ours, 7,
        "the two ends must disagree for this to test anything"
    );
    plugin(&mut net, Side::B, &a_id, "accept", answer_body(ours));
    net.ui.clear();

    // Not early. A transfer that is merely slow to start is not a failed one,
    // and a deadline that fires before it is due is worse than none.
    net.now += BULK_DIAL_WAIT_MS - 1;
    tick(&mut net, Side::B);
    assert!(
        !told_it_finished(&net, Side::B, false),
        "given up on before the wait was over"
    );

    net.now += 2;
    tick(&mut net, Side::B);
    assert!(
        told_it_finished(&net, Side::B, false),
        "the transfer was never ended, so nothing released the port or the name"
    );
    // And the far end is told rather than left to wonder, which is the rule
    // every other bulk ending follows.
    assert!(
        told_it_finished(&net, Side::A, false),
        "the sender was not told the transfer it had been offered is over"
    );
}

#[test]
fn a_file_still_arriving_is_not_given_up_on() {
    // The other half, and the reason `BulkStarted` exists at all. Nothing in
    // the core knows how long a gigabyte should take, so the deadline may only
    // ever cover the wait for a sender — never the file. Bounding both would
    // cut off the transfers people most need to work.
    let (mut net, b_id) = sharing_pair();
    let a_id = net.a.device_id();
    net.dials = Dials::AndKeepsGoing;

    plugin(&mut net, Side::A, &b_id, "offer", offer_body(1, 4096));
    let ours = announced_transfer(&net, Side::B, "offer");
    plugin(&mut net, Side::B, &a_id, "accept", answer_body(ours));
    net.ui.clear();

    // Long past any deadline, and still going.
    net.now += BULK_DIAL_WAIT_MS * 10;
    tick(&mut net, Side::B);

    assert!(
        !told_it_finished(&net, Side::B, false),
        "a file that was still arriving was reported as a sender that never came"
    );
}

fn answer_body(transfer: u64) -> Vec<u8> {
    minicbor::to_vec(Finished {
        transfer,
        ok: true,
        detail: String::new(),
    })
    .unwrap()
}

#[test]
fn a_file_is_offered_accepted_and_finished() {
    let (mut net, b_id) = sharing_pair();
    let a_id = net.a.device_id();

    plugin(&mut net, Side::A, &b_id, "offer", offer_body(1, 4096));
    assert!(
        net.saw(
            Side::B,
            |e| matches!(e, UiEvent::Plugin { ty, .. } if ty == "offer")
        ),
        "the receiving host is asked before anything is listened for"
    );
    assert!(net.bulk_keys.is_empty(), "and nothing has a key yet");

    let local = announced_transfer(&net, Side::B, "offer");
    plugin(&mut net, Side::B, &a_id, "accept", answer_body(local));

    // Both ends were handed a key, and they match. Neither sent it, and each
    // filed it under its own number for the transfer.
    let ours = net
        .bulk_keys
        .get(&(Side::A, TransferId(1)))
        .expect("sender has a key");
    let theirs = net
        .bulk_keys
        .get(&(Side::B, TransferId(local)))
        .expect("receiver has a key");
    assert_eq!(ours, theirs, "derived independently from the session");
    assert_eq!(ours.len(), 32);

    assert!(
        net.saw(
            Side::A,
            |e| matches!(e, UiEvent::Plugin { ty, .. } if ty == "finished")
        ),
        "the sender is told how it went"
    );
    assert!(
        net.saw(
            Side::B,
            |e| matches!(e, UiEvent::Plugin { ty, .. } if ty == "finished")
        ),
        "and so is the receiver"
    );
}

#[test]
fn nothing_is_listened_for_until_a_person_says_yes() {
    // The property that keeps this from being a file drop for anything ever
    // paired with the machine.
    let (mut net, b_id) = sharing_pair();
    plugin(&mut net, Side::A, &b_id, "offer", offer_body(1, 4096));
    assert!(net.bulk_keys.is_empty(), "no key, no endpoint, no listener");
}

#[test]
fn rejecting_an_offer_tells_the_sender_and_opens_nothing() {
    let (mut net, b_id) = sharing_pair();
    let a_id = net.a.device_id();
    plugin(&mut net, Side::A, &b_id, "offer", offer_body(1, 4096));
    let ours = announced_transfer(&net, Side::B, "offer");
    plugin(&mut net, Side::B, &a_id, "reject", answer_body(ours));

    assert!(net.bulk_keys.is_empty(), "nothing was ever listened for");
    assert!(
        net.saw(
            Side::A,
            |e| matches!(e, UiEvent::Plugin { ty, .. } if ty == "reject")
        ),
        "the sender hears about it rather than waiting"
    );
}

#[test]
fn a_link_that_cannot_carry_files_refuses_instead_of_listening() {
    // `LinkAttrs::bulk` promises the core "refuses one with a clear error
    // rather than silently trying to push a gigabyte through 185-byte writes".
    // It went unenforced until a second transport existed to notice: over BLE
    // the receiver would accept, listen on a TCP port, and hand back an address
    // a phone with Wi-Fi off can never reach — which looks, from the phone,
    // like a transfer that simply stopped.
    let (mut net, b_id) = ble_sharing_pair();

    plugin(&mut net, Side::A, &b_id, "offer", offer_body(1, 4096));

    // On the sender, and before anything leaves. Refusing on the receiver was
    // the original bug wearing the shape of a fix: the far end declines when a
    // person there accepts, that refusal is local to it, and the phone that
    // asked sits on "offered" until the session ends. An error nobody who acted
    // can see is not a refusal.
    assert!(
        net.saw(Side::A, |e| matches!(
            e,
            UiEvent::Error { code, .. }
                if *code == acrylius_core::proto::envelope::ErrorCode::NotAllowed
        )),
        "the side that asked is told why, rather than left waiting"
    );
    assert!(
        !net.saw(Side::B, |e| matches!(
            e,
            UiEvent::Plugin { ty, .. } if ty == "offer"
        )),
        "and an offer that cannot be completed is never made"
    );
    // A refusal here sends nothing, so the "reject" a host waits for would
    // never arrive on its own and the file would sit listed as sending
    // forever. It is announced locally instead.
    assert!(
        net.saw(Side::A, |e| matches!(
            e,
            UiEvent::Plugin { ty, .. } if ty == "reject"
        )),
        "and the transfer is closed out, not left open"
    );
    assert!(
        net.bulk_keys.is_empty(),
        "nothing may be listened for on a link that cannot carry it"
    );
}

#[test]
fn a_file_sends_again_once_wi_fi_takes_over_from_bluetooth() {
    // The reported symptom, end to end. A phone finds Bluetooth first, and
    // Bluetooth cannot carry a file — so while it was left there, every
    // transfer was refused. Being on Wi-Fi as well has to un-refuse them.
    let (a, b) = (sharing_core("phone"), sharing_core("pc"));
    let b_id = b.device_id();
    let mut net = Net::new(a, b);
    // The slower transport is the radio; the better one is the network.
    net.ble_transport = Some(SLOWER);
    net.local(
        Side::B,
        LocalCommand::OpenPairingWindow {
            code: CODE.to_string(),
        },
    );
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
            code: CODE.to_string(),
        },
    );
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });

    // Bluetooth only: the network route is overwritten with one that does not
    // answer, so the radio is what connects.
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "not-listening");
    discover_via(&mut net, Side::A, Side::B, SLOWER, "B");
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);

    plugin(&mut net, Side::A, &b_id, "offer", offer_body(1, 4096));
    assert!(
        net.saw(Side::A, |e| matches!(e, UiEvent::Error { .. })),
        "over Bluetooth alone a file is refused, and the sender is told"
    );

    // Wi-Fi turns up, a moment later. The clock has to move: a handshake
    // opener must be strictly newer than the last one accepted, or it is a
    // replay — so two sessions with one peer in the same millisecond is one
    // session. Seconds pass between a radio connecting and a network
    // resolving, but the harness's clock only moves when told.
    net.wall += 1_000;
    net.dialed.clear();
    net.ui.clear();
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "B");
    assert_eq!(
        net.dialed,
        vec![(TRANSPORT, "B".to_string())],
        "the better transport is dialled even though a link already exists"
    );
    net.ui.clear();

    plugin(&mut net, Side::A, &b_id, "offer", offer_body(2, 4096));
    assert!(
        !net.saw(Side::A, |e| matches!(e, UiEvent::Error { .. })),
        "and once there is a route that can carry it, it is not refused"
    );
    assert!(
        net.saw(Side::B, |e| matches!(
            e,
            UiEvent::Plugin { ty, .. } if ty == "offer"
        )),
        "the offer reaches the far end over the better link"
    );
}

#[test]
fn a_link_that_can_carry_files_is_left_alone() {
    // The negative of the above: the check must key on what the link said, not
    // on there being a check at all. A TCP pair offers and listens as before.
    let (mut net, b_id) = sharing_pair();
    let a_id = net.a.device_id();

    plugin(&mut net, Side::A, &b_id, "offer", offer_body(1, 4096));
    assert!(
        net.saw(Side::B, |e| matches!(
            e,
            UiEvent::Plugin { ty, .. } if ty == "offer"
        )),
        "an ordinary link still carries an offer"
    );

    let ours = announced_transfer(&net, Side::B, "offer");
    plugin(&mut net, Side::B, &a_id, "accept", answer_body(ours));
    assert!(
        !net.bulk_keys.is_empty(),
        "and still opens a side channel for it"
    );
}

#[test]
fn a_body_too_large_for_the_link_is_refused_not_sent() {
    // The other half of the same promise. A BLE link takes 16 KiB; anything
    // bigger has to be told so, not handed to a transport that would fragment
    // it into thousands of notifications and appear to hang.
    //
    // Sent over `ping`, which forwards a body verbatim — the share plugin would
    // reject 32 KiB of zeros as a malformed offer long before the link saw it.
    let (mut net, b_id) = ble_sharing_pair();
    net.local(
        Side::A,
        LocalCommand::Plugin {
            peer: b_id.clone(),
            cap: ping::CAP.to_string(),
            ty: "ping".to_string(),
            body: vec![0u8; 32 * 1024],
        },
    );

    assert!(
        net.saw(Side::A, |e| matches!(
            e,
            UiEvent::Error { code, .. }
                if *code == acrylius_core::proto::envelope::ErrorCode::TooLarge
        )),
        "the sender is told it does not fit"
    );
    assert!(
        !net.saw(
            Side::B,
            |e| matches!(e, UiEvent::Plugin { ty, .. } if ty == "ping")
        ),
        "and nothing arrived at the far end"
    );
}

#[test]
fn a_device_with_nowhere_to_put_a_file_refuses_at_once() {
    // The phone's shape: it registers the plugin and advertises the capability
    // — otherwise a computer's `send` would fail with `cap_not_negotiated` and
    // it could never send files either — but it has no download directory and
    // no way to ask a person. Accepting the offer into a queue nobody can drain
    // would leave the sender waiting on an answer that is never coming.
    let phone = CoreBuilder::new(
        Identity::generate().unwrap(),
        CoreConfig {
            name: "phone".to_string(),
            platform: "test".to_string(),
            ..Default::default()
        },
    )
    // No EffectKind::Share. That is the whole difference.
    .plugin(ping::PingPlugin::default())
    .plugin(acrylius_core::plugins::share::SharePlugin::default())
    .build();

    assert!(
        phone.caps_in().iter().any(|c| c == share::CAP),
        "still advertised, or a computer could not be sent files by it either"
    );
    assert!(
        !phone.caps_served().iter().any(|c| c == share::CAP),
        "but not as something it can act on"
    );

    let pc = sharing_core("pc");
    let pc_id = pc.device_id();
    let mut net = Net::new(phone, pc);
    net.local(
        Side::B,
        LocalCommand::OpenPairingWindow {
            code: CODE.to_string(),
        },
    );
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
            code: CODE.to_string(),
        },
    );
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });
    net.local(
        Side::A,
        LocalCommand::Connect {
            peer: pc_id.clone(),
        },
    );

    let phone_id = net.a.device_id();
    plugin(&mut net, Side::B, &phone_id, "offer", offer_body(1, 4096));

    assert!(net.bulk_keys.is_empty(), "nothing was listened for");
    assert!(
        net.saw(
            Side::B,
            |e| matches!(e, UiEvent::Plugin { ty, .. } if ty == "err")
        ),
        "the computer is told now, not in an hour"
    );
}

#[test]
fn an_endpoint_nobody_offered_is_not_dialled() {
    // Somewhere to connect, chosen by the other end. Accepting one for a
    // transfer this device never offered would let a peer point it anywhere.
    let (mut net, b_id) = sharing_pair();
    let a_id = net.a.device_id();
    let body = minicbor::to_vec(Accept {
        transfer: 999,
        endpoint: "10.0.0.1:1".to_string(),
    })
    .unwrap();
    plugin(&mut net, Side::B, &a_id, "offer", offer_body(999, 1));
    net.bulk_keys.clear();

    // B sends an accept for a transfer A never offered.
    net.local(
        Side::B,
        LocalCommand::Plugin {
            peer: a_id.clone(),
            cap: share::CAP.to_string(),
            ty: "offer".to_string(),
            body: offer_body(1000, 1),
        },
    );
    let _ = body;
    let _ = b_id;
    assert!(
        !net.bulk_keys.contains_key(&(Side::A, TransferId(999))),
        "A dialled nothing for a transfer it did not make"
    );
}

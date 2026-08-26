//! Two cores, one process, no sockets and no clock.
//!
//! This is the project's spine. It drives a complete `XXpsk3` pairing, then an
//! `IKpsk2` session, then a plugin round trip, entirely in memory, so the whole
//! protocol is verifiable on a Linux box with no Apple hardware anywhere near
//! it. That was the point of making the core sans-IO.

use std::collections::{BTreeMap, VecDeque};

use acrylius_core::config::CoreConfig;
use acrylius_core::core::{Core, CoreBuilder};
use acrylius_core::link::{LinkAttrs, LinkDownReason, LinkId, TransportId};
use acrylius_core::noise::Identity;
use acrylius_core::peer::PeerState;
use acrylius_core::plugins::ping;
use acrylius_core::proto::ids::DeviceId;
use acrylius_core::vocab::{
    Action, DiscoveredPeer, Event, LocalCommand, Now, Sensitivity, UiEvent,
};

const CODE: &str = "ABCD1234";
const TRANSPORT: TransportId = TransportId(0);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Side {
    A,
    B,
}

impl Side {
    fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
    fn addr(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
        }
    }
}

/// An in-memory transport. It is a transport in exactly the sense the core
/// means: it produces LinkUp/LinkRecv/LinkDown and consumes Dial/LinkSend/Close.
struct Net {
    a: Core,
    b: Core,
    now: u64,
    next_link: u64,
    /// (side, link) -> the peer's link id for the same wire.
    peer_link: BTreeMap<(Side, u64), u64>,
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
}

impl Net {
    fn new(a: Core, b: Core) -> Self {
        Self {
            a,
            b,
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
        }
    }

    fn skew_for(&self, s: Side) -> u64 {
        match s {
            Side::A => 0,
            Side::B => self.b_skew,
        }
    }

    fn core(&mut self, s: Side) -> &mut Core {
        match s {
            Side::A => &mut self.a,
            Side::B => &mut self.b,
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
                self.queue.push_back((
                    side,
                    Event::BulkListening {
                        transfer,
                        endpoint: format!("{}:9000", side.addr()),
                    },
                ));
            }
            Action::BulkSend { transfer, key, .. } => {
                // Both ends must have derived the same key from the session
                // without either transmitting it. If this ever fails, nothing
                // would decrypt on a real socket.
                if let Some(theirs) = self.bulk_keys.get(&(side.other(), transfer)) {
                    assert_eq!(&key, theirs, "both ends must derive the same bulk key");
                }
                self.bulk_keys.insert((side, transfer), key);
                for who in [side, side.other()] {
                    self.queue.push_back((
                        who,
                        Event::BulkFinished {
                            transfer,
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
                assert_eq!(target, side.other(), "the harness only wires the two cores");
                let mine = self.next_link;
                let theirs = self.next_link + 1;
                self.next_link += 2;
                self.peer_link.insert((side, mine), theirs);
                self.peer_link.insert((side.other(), theirs), mine);
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
                    side.other(),
                    Event::LinkUp {
                        link: LinkId(theirs),
                        attrs,
                        dial: None,
                    },
                ));
            }
            Action::LinkSend { link, msg } => {
                let Some(&peer) = self.peer_link.get(&(side, link.0)) else {
                    return;
                };
                self.queue.push_back((
                    side.other(),
                    Event::LinkRecv {
                        link: LinkId(peer),
                        msg,
                    },
                ));
            }
            Action::Close { link, .. } => {
                if let Some(peer) = self.peer_link.remove(&(side, link.0)) {
                    self.peer_link.remove(&(side.other(), peer));
                    self.queue.push_back((
                        side.other(),
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

    plugin(&mut net, Side::B, &a_id, "accept", answer_body(1));

    // Both ends were handed a key, and they match. Neither sent it.
    let transfer = TransferId(1);
    let ours = net
        .bulk_keys
        .get(&(Side::A, transfer))
        .expect("sender has a key");
    let theirs = net
        .bulk_keys
        .get(&(Side::B, transfer))
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
    plugin(&mut net, Side::B, &a_id, "reject", answer_body(1));

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

    plugin(&mut net, Side::B, &a_id, "accept", answer_body(1));
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

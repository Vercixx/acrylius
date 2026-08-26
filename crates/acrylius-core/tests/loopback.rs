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
use acrylius_core::vocab::{Action, DiscoveredPeer, Event, LocalCommand, Sensitivity, UiEvent};

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
    pub ui: Vec<(Side, UiEvent)>,
    pub persisted: Vec<(Side, String, bool)>,
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
            ui: Vec::new(),
            persisted: Vec::new(),
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
            let now = self.now;
            let out = self.core(side).handle(now, ev);
            for action in out.actions {
                self.apply(side, action);
            }
        }
    }

    fn apply(&mut self, side: Side, action: Action) {
        match action {
            Action::Ui(e) => self.ui.push((side, e)),
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
            Action::Dial { addr, dial, .. } => {
                let target = if addr == "A" { Side::A } else { Side::B };
                assert_eq!(target, side.other(), "the harness only wires the two cores");
                let mine = self.next_link;
                let theirs = self.next_link + 1;
                self.next_link += 2;
                self.peer_link.insert((side, mine), theirs);
                self.peer_link.insert((side.other(), theirs), mine);
                let attrs = LinkAttrs::loopback(TRANSPORT);
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
    let fp = match of {
        Side::A => net.a.fingerprint(),
        Side::B => net.b.fingerprint(),
    };
    net.queue.push_back((
        side,
        Event::Discovered {
            transport: TRANSPORT,
            peer: DiscoveredPeer {
                fingerprint: Some(fp),
                name: "peer".to_string(),
                addr: of.addr().to_string(),
                pairing: false,
            },
        },
    ));
    net.run();
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
    let out = net.b.handle(net.now, Event::Tick);
    let deadline = out.next_deadline_ms.expect("a window sets a deadline");
    assert!(deadline > net.now);

    net.now = deadline;
    let out = net.b.handle(net.now, Event::Tick);
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

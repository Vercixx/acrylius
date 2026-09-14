//! Two cores, one process, no sockets and no clock.
//!
//! Drives a full `XXpsk3` pairing, an `IKpsk2` session, and a plugin round
//! trip in memory, so the protocol is verifiable with no Apple hardware.

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

const TRANSPORT: TransportId = TransportId(0);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Side {
    A,
    B,
    /// A second computer, so a test can check the core routes to the *right*
    /// peer rather than just *a* peer.
    C,
}

impl Side {
    fn addr(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
        }
    }
}

/// An in-memory transport: produces LinkUp/LinkRecv/LinkDown, consumes
/// Dial/LinkSend/Close.
struct Net {
    a: Core,
    b: Core,
    /// A second computer, built for every `Net` but left alone unless a test
    /// pairs with it.
    c: Core,
    now: u64,
    next_link: u64,
    /// (side, link) -> the far end of the same wire, tracked from both sides.
    peer_link: BTreeMap<(Side, u64), (Side, u64)>,
    queue: VecDeque<(Side, Event)>,
    /// Milliseconds added to B's clock; machines don't share an uptime.
    pub b_skew: u64,
    /// Milliseconds since the epoch, shared by both sides like NTP-synced
    /// machines.
    pub wall: u64,
    /// Keys handed to each side's host, so a test can check both derived the
    /// same one without either sending it.
    pub bulk_keys: BTreeMap<(Side, acrylius_core::vocab::TransferId), Vec<u8>>,
    pub ui: Vec<(Side, UiEvent)>,
    pub persisted: Vec<(Side, String, bool)>,
    /// Every dial the core asked for, in order, so a test can check which route
    /// was preferred and how far the fallback walked.
    pub dialed: Vec<(TransportId, String)>,
    /// Give new links BLE's attributes rather than loopback's: `BulkSupport::None`
    /// and a 16 KiB `max_message`.
    pub links_are_ble: bool,
    /// Same, but for one transport only, so a pair can be on BLE and Wi-Fi
    /// at once.
    pub ble_transport: Option<TransportId>,
    /// How far a transfer gets once the sender has been told where to dial.
    pub dials: Dials,
    /// Which side is listening on which endpoint, and what it calls the
    /// transfer. The two ends number a transfer separately, so a dial is
    /// matched to its listener by endpoint.
    listening: BTreeMap<String, (Side, TransferId)>,
    /// Every link the harness has brought up, and which transport carried it,
    /// so a test can take one down by name.
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
            // Its own origin, so C can't accidentally share an uptime with B.
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
            // Each side keeps its own monotonic origin; the wall clock is shared.
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

            // Simulated bulk channel: no socket, no bytes. Tests the
            // negotiation only; byte transport is tested against real sockets.
            Action::BulkListen { transfer, key, .. } => {
                assert_eq!(key.len(), 32, "a host is handed a real key");
                self.bulk_keys.insert((side, transfer), key);
                // One endpoint per transfer: the receiver numbers transfers
                // itself, so a dial is matched to its listener by endpoint.
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
                // Both ends must derive the same key without either
                // transmitting it. Looked up by endpoint since the listener
                // files it under its own transfer id.
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
                // Only the listening side learns a connection was made;
                // that's what `BulkStarted` is for.
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
                // A dial that is never answered, either way. Mirrors
                // Network.framework waiting on a connection with no viable path.
                if addr == "silent" {
                    return;
                }
                // Only the two cores are wired; anything else did not answer.
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
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
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
fn paired_twice() -> (Net, DeviceId, DeviceId) {
    let (mut net, _a_id, b_id) = paired();
    let c_id = net.c.device_id();

    // A second session with a second peer needs a later opener than the first.
    net.wall += 1_000;
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::C.addr().to_string(),
        },
    );
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::C, LocalCommand::ConfirmPairing { accept: true });
    // Fail here, not in every test built on this helper.
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

/// The same, but naming which transport saw it and where.
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

/// How far a dial gets once a sender is told where to connect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dials {
    /// Connects and finishes, the way a small file on a local network does.
    AndFinishes,
    /// Connects, and is still going: a large file, a slow disk.
    AndKeepsGoing,
    /// Never arrives: session died between accept and dial, app killed, or
    /// Wi-Fi went away.
    Never,
}

/// Deliver a well-formed transport frame that will not decrypt, on a live link.
///
/// Junk is refused by framing before reaching an established session; this
/// exercises the established-session path. The link stays in the harness's
/// table on purpose since the core, not the harness, must drop it.
fn deliver_undecryptable(net: &mut Net, side: Side, transport: TransportId) {
    // The last live link, not the first: pairing leaves closed ones in front.
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
    // Switching Wi-Fi off does not close a TCP connection: the peer just
    // stops answering. Once told, the core must fall back at once instead of
    // keeping the dead link as the preferred route.
    let (mut net, _a_id, b_id) = paired();
    net.ble_transport = Some(SLOWER);

    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "not-listening");
    discover_via(&mut net, Side::A, Side::B, SLOWER, "B");
    // A strictly later opener timestamp than the last one seen.
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
    // Routing stayed correct but the peer was reported unreachable anyway.
    // `PeerUnreachable` makes a host discard peer state, so losing one of
    // two live routes must not fire it.
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
    // The route change must be announced, not left silent.
    assert!(
        net.saw(Side::A, |e| matches!(e, UiEvent::PeerReachable { .. })),
        "nothing told the host the route had changed"
    );
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
}

/// Switch Wi-Fi off on one side only, which is the only way it ever happens.
///
/// Unlike `lose_link`, only `noticed_by` learns of it; the other end keeps an
/// ESTABLISHED socket to a peer that has simply stopped answering.
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

/// Both routes up at once: Bluetooth comes up first, Wi-Fi dialled alongside it.
fn both_routes_up(net: &mut Net) {
    discover_via(net, Side::A, Side::B, TRANSPORT, "not-listening");
    discover_via(net, Side::A, Side::B, SLOWER, "B");
    // A second session needs a strictly later opener timestamp than the last.
    net.wall += 1_000;
    discover_via(net, Side::A, Side::B, TRANSPORT, "B");
}

#[test]
fn the_route_a_peer_is_actually_using_is_the_one_it_is_answered_over() {
    // Bluetooth is already up when Wi-Fi dies, so nothing is dialled and this
    // end is told nothing. It must still learn the route from the direction
    // an answer actually arrives, not keep preferring the dead socket.
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

    // Two links that last spoke in the same millisecond are equally proven,
    // which is a tie, and a tie is what transport preference is for.
    net.now += 2_000;

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
    // Misjudging which route carries must cost one message the slower way,
    // not close the route it judged dead.
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
    // Wi-Fi drops and returns, leaving this end holding two sockets on one
    // transport, the older one dead. It must answer over the new one, not
    // keep picking the older of two equal transports.
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
    // Dropping a link because the peer sent nonsense is right; doing it in
    // silence, leaving the device `Reachable` with no route, is not.
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
    // A dial that connects has proved a socket, not a peer: the preferred
    // address has the wrong machine on it, so the untried routes must not be
    // dropped just because a connection opened.
    let (mut net, _a_id, b_id) = paired();
    // The preferred transport points at the wrong machine, the other at B.
    discover_via(&mut net, Side::A, Side::B, TransportId(1), "B");
    discover_via(&mut net, Side::A, Side::B, TransportId(0), "C");
    net.local(Side::A, LocalCommand::Disconnect { peer: b_id.clone() });
    net.ui.clear();
    net.dialed.clear();
    // A second opener in the same millisecond as the first is a replay.
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
    // The case above must not be bought by never reporting a departure at all.
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
    // Overwrite the working route pairing recorded, so nothing connects and
    // the address book itself is what's left to inspect.
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "not-listening");
    discover_via(&mut net, Side::A, Side::B, SLOWER, "also-not-listening");

    // Only what Connect does is under test here.
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
    // Nobody presses anything: a sighting alone must bring the session back,
    // the way a Bluetooth radio reconnecting does on a phone.
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
    // A phone finds Bluetooth first, but "a link exists" must not mean
    // "nothing more to do": Bluetooth cannot carry a file.
    let (mut net, _a_id, b_id) = paired();

    // Pairing's route is overwritten with one that does not answer, so the
    // fallback on the worse transport is what connects.
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "not-listening");
    discover_via(&mut net, Side::A, Side::B, SLOWER, "B");
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);

    net.dialed.clear();
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "B");

    assert_eq!(
        net.dialed,
        vec![(TRANSPORT, "B".to_string())],
        "being reachable already is no reason to stay on the worse transport"
    );
    // The worse link is still there behind it, not torn down.
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
}

#[test]
fn reconsider_routes_dials_a_strictly_better_route_set_locally() {
    // The shape a USB-attach watcher uses: no discovery event, just
    // `SetPeerAddress` then `ReconsiderRoutes`, both sent locally.
    let (a, b) = (core("phone"), core("pc"));
    let b_id = b.device_id();
    let mut net = Net::new(a, b);
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: SLOWER,
            addr: Side::B.addr().to_string(),
        },
    );
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });
    // The armed heartbeat wakes the runtime immediately; this is that tick.
    net.queue.push_back((Side::A, Event::Tick));
    net.run();
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);

    net.dialed.clear();
    net.local(
        Side::A,
        LocalCommand::SetPeerAddress {
            peer: b_id.clone(),
            transport: TRANSPORT,
            addr: "B".to_string(),
        },
    );
    assert!(
        net.dialed.is_empty(),
        "recording an address must not dial anything on its own"
    );

    net.local(Side::A, LocalCommand::ReconsiderRoutes);
    assert_eq!(
        net.dialed,
        vec![(TRANSPORT, "B".to_string())],
        "a strictly better route recorded locally is dialled with no discovery involved"
    );
}

#[test]
fn reconsider_routes_leaves_a_worse_route_alone() {
    // The reason a new transport must sit below whatever it should be able to
    // upgrade from: a route no better than what already carries is never tried.
    let (mut net, _a_id, b_id) = paired();
    discover(&mut net, Side::A, Side::B);
    assert_eq!(
        net.a.transport_for(&b_id),
        Some(TransportKind::UnixLoopback)
    );

    net.local(
        Side::A,
        LocalCommand::SetPeerAddress {
            peer: b_id.clone(),
            transport: SLOWER,
            addr: "B".to_string(),
        },
    );
    net.dialed.clear();
    net.local(Side::A, LocalCommand::ReconsiderRoutes);

    assert!(
        net.dialed.is_empty(),
        "SLOWER is not strictly better than what already carries, so nothing is tried"
    );
}

#[test]
fn a_plain_connect_does_not_dial_a_better_route_while_already_reachable() {
    // Only `ReconsiderRoutes` may act on a route becoming available; `Connect`
    // is what a person pressing "try again" sends, not an upgrade request.
    let (a, b) = (core("phone"), core("pc"));
    let b_id = b.device_id();
    let mut net = Net::new(a, b);
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: SLOWER,
            addr: Side::B.addr().to_string(),
        },
    );
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });
    net.queue.push_back((Side::A, Event::Tick));
    net.run();
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);

    net.local(
        Side::A,
        LocalCommand::SetPeerAddress {
            peer: b_id.clone(),
            transport: TRANSPORT,
            addr: "B".to_string(),
        },
    );
    net.dialed.clear();
    net.local(Side::A, LocalCommand::Connect { peer: b_id.clone() });

    assert!(
        net.dialed.is_empty(),
        "a plain Connect must not try a better route on its own"
    );
}

#[test]
fn forgetting_a_route_removes_it_from_the_fallback_chain() {
    let (mut net, _a_id, b_id) = paired();
    lose_link(&mut net, TRANSPORT);
    assert_eq!(net.a.peer_state(&b_id), PeerState::Unreachable);

    // The route pairing left on file is broken, and a fallback is added, then
    // withdrawn before anything dials it.
    net.local(
        Side::A,
        LocalCommand::SetPeerAddress {
            peer: b_id.clone(),
            transport: TRANSPORT,
            addr: "not-listening".to_string(),
        },
    );
    net.local(
        Side::A,
        LocalCommand::SetPeerAddress {
            peer: b_id.clone(),
            transport: SLOWER,
            addr: "B".to_string(),
        },
    );
    net.local(
        Side::A,
        LocalCommand::ForgetPeerAddress {
            peer: b_id.clone(),
            transport: SLOWER,
            addr: "B".to_string(),
        },
    );

    net.dialed.clear();
    net.local(Side::A, LocalCommand::Connect { peer: b_id.clone() });

    assert_eq!(
        net.dialed,
        vec![(TRANSPORT, "not-listening".to_string())],
        "the forgotten route must not be tried as a fallback"
    );
    assert_eq!(net.a.peer_state(&b_id), PeerState::Unreachable);
}

#[test]
fn a_hello_no_newer_than_the_last_one_is_refused() {
    // `Noise_IKpsk2` message 1 is replayable, so every opener carries a
    // timestamp and a peer's watermark only ever moves forward (PROTOCOL.md
    // §7, enforced in `accept_hello`).
    let (mut net, _a_id, b_id) = paired();
    net.ble_transport = Some(SLOWER);

    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "not-listening");
    discover_via(&mut net, Side::A, Side::B, SLOWER, "B");
    assert_eq!(
        net.a.transport_for(&b_id),
        Some(TransportKind::BleGatt),
        "the worse transport is what connected"
    );

    // The clock has deliberately not moved, so the Hello that comes back
    // repeats a timestamp already seen.
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
fn a_refused_pairing_goes_quiet_for_longer_than_one_that_merely_lapsed() {
    // The cooldown is a security control, not a politeness: refusing the
    // digits has to cost more than walking away, or the six-digit bound on a
    // relay attack isn't worth anything.
    let cfg = CoreConfig::default();
    assert!(
        cfg.pair_denied_cooldown_ms > cfg.pair_cooldown_ms,
        "a mismatch is the one sign of a relay; it must not be the cheaper outcome"
    );

    let (mut net, _) = pairing_in_flight();
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: false });

    // Still inside the long cooldown: A will not even start another.
    net.now += cfg.pair_cooldown_ms + 1;
    net.wall += cfg.pair_cooldown_ms + 1;
    net.ui.clear();
    ask_to_pair(&mut net);
    assert!(
        net.saw(Side::A, |e| matches!(e, UiEvent::PairingFailed { .. })),
        "a device that just reported a mismatch must not retry on the short cooldown"
    );

    // B never heard the refusal, so its own half is still waiting on a
    // person. Let that lapse, costing B the ordinary cooldown in turn.
    net.now += cfg.pair_denied_cooldown_ms;
    net.wall += cfg.pair_denied_cooldown_ms;
    tick(&mut net, Side::B);

    // Past both, pairing is possible again: a cooldown, not a permanent ban.
    net.now += cfg.pair_cooldown_ms + 1;
    net.wall += cfg.pair_cooldown_ms + 1;
    net.ui.clear();
    ask_to_pair(&mut net);
    assert!(
        net.sas_for(Side::A).is_some(),
        "the cooldown must expire, or one mistake locks the pair out forever"
    );
}

/// A pairing A started and neither side has answered yet.
fn pairing_in_flight() -> (Net, String) {
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    ask_to_pair(&mut net);
    let sas = net.sas_for(Side::A).expect("A shows a SAS").to_string();
    (net, sas)
}

/// A taps B. There is nothing to type, so this is the whole gesture.
fn ask_to_pair(net: &mut Net) {
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
        },
    );
}

#[test]
fn a_device_already_reached_is_not_dialled_again() {
    // Discovery repeats; dialling per sighting would be a storm aimed at a
    // device whose only offence is being switched on.
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
    // `best_link` must filter the link table by peer before choosing, not
    // just return whichever link sorts first.
    let (mut net, b_id, c_id) = paired_twice();
    discover(&mut net, Side::A, Side::B);
    net.wall += 1_000;
    discover(&mut net, Side::A, Side::C);

    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
    assert_eq!(net.a.peer_state(&c_id), PeerState::Reachable);

    // Addressed to C, deliberately not the link that sorts first: B was
    // paired earlier and holds the lower id.
    net.local(
        Side::A,
        LocalCommand::Plugin {
            peer: c_id.clone(),
            cap: ping::CAP.to_string(),
            ty: "ping".to_string(),
            body: b"for-c".to_vec(),
        },
    );

    // A ping delivered to the wrong computer is still answered; only the
    // name on the pong differs.
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
    // Unlike the test above, the addressed peer's link is not the newest
    // here, so `best_link` must not fall back to picking by recency.
    let (mut net, b_id, c_id) = paired_twice();
    discover(&mut net, Side::A, Side::B);
    net.wall += 1_000;
    discover(&mut net, Side::A, Side::C);

    // B speaks last. Sent *from* B, not asked for by A, so driving this
    // from A instead wouldn't leave the trap sprung.
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
fn a_pairing_we_started_does_not_answer_anybody_elses_handshake() {
    // Pairing is plain `XX`: anybody can complete a handshake with this
    // device. The only guard is that a confirmation already on screen is
    // never replaced by a stranger's handshake.
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
        },
    );
    let waiting_on = net
        .sas_for(Side::A)
        .expect("A is waiting to be confirmed")
        .to_string();

    // More than a pairing's whole attempt budget: a device that answers C at
    // all counts these against itself and gives up on the third.
    for _ in 0..4 {
        net.local(
            Side::C,
            LocalCommand::RequestPairing {
                transport: TRANSPORT,
                addr: Side::A.addr().to_string(),
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

/// The best route a side was last offered for a machine, and on which transport.
fn offered(net: &Net, side: Side) -> Option<(TransportId, String)> {
    net.ui.iter().rev().find_map(|(s, e)| match e {
        UiEvent::Discovered {
            addr, transport, ..
        } if *s == side => Some((*transport, addr.clone())),
        _ => None,
    })
}

#[test]
fn a_worse_transport_seeing_a_machine_does_not_replace_its_better_address() {
    // Bluetooth repeats and Bonjour does not, so a repeated sighting on the
    // worse transport must not replace the working address.
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);

    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "b-over-wifi");
    assert_eq!(
        offered(&net, Side::A),
        Some((TRANSPORT, "b-over-wifi".to_string()))
    );

    // The same machine, seen again on the worse transport.
    discover_via(&mut net, Side::A, Side::B, SLOWER, "b-over-bluetooth");
    assert_eq!(
        offered(&net, Side::A),
        Some((TRANSPORT, "b-over-wifi".to_string())),
        "a sighting on a worse transport must not become the offered route"
    );

    // The worse route is still remembered, for falling back to.
    discover_via(&mut net, Side::A, Side::B, SLOWER, "b-over-bluetooth");
    net.queue.push_back((
        Side::A,
        Event::Undiscovered {
            transport: TRANSPORT,
            addr: "b-over-wifi".to_string(),
        },
    ));
    net.run();
    discover_via(&mut net, Side::A, Side::B, SLOWER, "b-over-bluetooth");
    assert_eq!(
        offered(&net, Side::A),
        Some((SLOWER, "b-over-bluetooth".to_string())),
        "with the better route gone, the worse one is what is left"
    );
}

#[test]
fn what_is_nearby_is_what_is_not_already_paired() {
    // Nearby must list only unpaired machines: this list only feeds
    // `pair with`, and an already-paired address there is always refused.
    let (mut net, _a_id, _b_id) = paired();
    discover(&mut net, Side::A, Side::B);
    discover_via(&mut net, Side::A, Side::C, TRANSPORT, "c-over-wifi");

    let nearby: Vec<String> = net.a.nearby().map(|n| n.addr.clone()).collect();
    assert_eq!(
        nearby,
        vec!["c-over-wifi".to_string()],
        "only the machine that is not paired, and on the route to reach it"
    );

    // Pair with it too, and there is nothing left to offer.
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::C.addr().to_string(),
        },
    );
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::C, LocalCommand::ConfirmPairing { accept: true });
    assert_eq!(
        net.a.nearby().count(),
        0,
        "a machine becomes un-offerable the moment it is paired"
    );
}

#[test]
fn forgetting_a_device_says_so_and_offers_it_again() {
    // Revoke must announce itself (a host reading the peer list right after
    // can otherwise miss the removal), and the machine must be offered again
    // rather than staying quiet until discovery re-resolves it.
    let (mut net, _a_id, b_id) = paired();
    discover(&mut net, Side::A, Side::B);
    net.ui.clear();

    net.local(Side::A, LocalCommand::Revoke { peer: b_id.clone() });

    assert!(
        net.saw(
            Side::A,
            |e| matches!(e, UiEvent::Revoked { peer } if peer == &b_id)
        ),
        "a revoke must announce itself, or a screen can only guess when it happened"
    );
    assert!(
        offered(&net, Side::A).is_some(),
        "and the machine is on the network still, so it must be offered again"
    );
    assert_eq!(net.a.peers().count(), 0, "and it really is forgotten");
}

#[test]
fn a_machine_never_seen_is_not_offered_after_forgetting_one() {
    // `announce` reads a cache of sightings; a peer paired by typing an
    // address was never in it, so it must not be offered either.
    let (mut net, _a_id, b_id) = paired();
    net.ui.clear();
    net.local(Side::A, LocalCommand::Revoke { peer: b_id });
    assert!(
        offered(&net, Side::A).is_none(),
        "nothing discovery has not seen may be offered"
    );
}

#[test]
fn nobody_had_to_open_anything_for_a_tap_to_reach_a_screen() {
    // A tap alone puts six digits on both screens; B never has to open anything.
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    ask_to_pair(&mut net);

    assert_eq!(
        net.sas_for(Side::A),
        net.sas_for(Side::B),
        "both ends must be comparing the same digits"
    );
    assert!(net.sas_for(Side::A).is_some(), "and there must be digits");

    // But still nothing stored until a person on each side agrees.
    assert_eq!(net.a.peers().count(), 0, "a handshake alone pairs nobody");
    assert_eq!(net.b.peers().count(), 0);
}

#[test]
fn rubbish_where_a_handshake_should_be_costs_the_sender_a_cooldown() {
    // A handshake that does not complete gets one strike and a cooldown,
    // not three attempts.
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    garbled_pairing_frame(&mut net, Side::B);

    assert!(net.sas_for(Side::B).is_none(), "no digits from rubbish");
    assert!(
        !net.b.pairing_open(),
        "and no slot left claimed by a handshake that went nowhere"
    );

    // The cooldown has to actually bite on the very next real attempt.
    ask_to_pair(&mut net);
    assert!(
        net.sas_for(Side::B).is_none(),
        "a device that just sent rubbish must wait before trying again"
    );
}

/// Something that claims to be a pairing handshake message and is not.
fn garbled_pairing_frame(net: &mut Net, side: Side) {
    // A link from nowhere, the way an inbound connection arrives.
    net.next_link += 1;
    let link = net.next_link;
    net.queue.push_back((
        side,
        Event::LinkUp {
            link: LinkId(link),
            attrs: acrylius_core::link::LinkAttrs::loopback(TRANSPORT),
            dial: None,
        },
    ));
    // Too short to be message 1; long "rubbish" would be a legitimate opener,
    // since `XX` message 1 is just a bare ephemeral key.
    let msg = acrylius_core::proto::frame::join(
        acrylius_core::proto::frame::FrameKind::PairHandshake,
        &[0xffu8; 8],
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

#[test]
fn another_links_death_does_not_cancel_a_confirmation_on_screen() {
    // A person is looking at six digits; an unrelated link going down must
    // not take the question away from under them.
    let (mut net, a_id, _b_id) = paired();

    // A second machine asks B to pair, and B is now showing digits.
    net.local(
        Side::C,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
        },
    );
    let waiting_on = net.sas_for(Side::B).expect("B shows a SAS").to_string();

    // A, already paired with B, goes away. Nothing to do with C's pairing.
    net.local(Side::B, LocalCommand::Revoke { peer: a_id });
    lose_link(&mut net, TRANSPORT);

    assert_eq!(
        net.sas_for(Side::B).map(str::to_string),
        Some(waiting_on),
        "B stopped showing the digits somebody was comparing"
    );
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::C, LocalCommand::ConfirmPairing { accept: true });
    assert_eq!(
        net.c.peers().count(),
        1,
        "and the confirmation still completed the pairing"
    );
}

#[test]
fn being_refused_does_not_cost_the_asker_its_next_attempt() {
    // `RequestPairing` claims the slot before dialling, so a refusal must
    // release it rather than lock the asker out until the deadline.
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    ask_to_pair(&mut net);

    // C tries B, which is busy comparing digits with A, and is refused.
    net.local(
        Side::C,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
        },
    );
    assert!(
        net.sas_for(Side::C).is_none(),
        "C was refused, as it should be"
    );

    // C must be free to try somewhere else at once; this is not what the
    // cooldown is for.
    assert!(
        !net.c.pairing_open(),
        "a refused asker must not still be holding its own pairing slot"
    );
}

#[test]
fn a_device_already_showing_digits_refuses_the_next_handshake() {
    // B is mid-pairing with A, so C gets nothing: not a second dialog, not a
    // replaced one.
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    ask_to_pair(&mut net);
    let waiting_on = net.sas_for(Side::B).expect("B shows a SAS").to_string();

    net.local(
        Side::C,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
        },
    );

    assert_eq!(
        net.sas_for(Side::B).map(str::to_string),
        Some(waiting_on),
        "B is still comparing the digits it was already comparing"
    );
    assert_eq!(net.c.peers().count(), 0, "and C paired with nothing");
}

#[test]
fn refusing_the_sas_pairs_nobody() {
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
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
fn a_pairing_nobody_answers_expires_on_the_hosts_clock() {
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    ask_to_pair(&mut net);

    // The pairing's own budget is what this advances by, not whatever the
    // next scheduled wake-up (e.g. the reconnect heartbeat) happens to be for.
    let out = net.b.handle(
        Now {
            monotonic_ms: net.now,
            wall_ms: net.wall,
        },
        Event::Tick,
    );
    assert!(
        out.next_deadline_ms.is_some_and(|d| d > net.now),
        "digits waiting on a person mean something is scheduled"
    );

    net.now += CoreConfig::default().pairing_window_ms + 1;
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
        "the pairing should have lapsed"
    );

    // Answering it afterwards must store nothing: the digits are stale.
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });
    assert_eq!(
        net.b.peers().count(),
        0,
        "a lapsed pairing must not still pair"
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
    // A pairing that completed has just demonstrated an address works; that
    // must be recorded, not discarded until discovery speaks again.
    let (mut net, _a_id, b_id) = paired();

    // Deliberately no discovery: the address from the pairing dial is the
    // only one in play.
    net.local(Side::A, LocalCommand::Connect { peer: b_id.clone() });
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
}

#[test]
fn an_address_seen_before_pairing_is_not_lost() {
    // An announcement landing before pairing is often the only one there
    // will be, so it must not be discarded as "not a peer yet".
    let (a, b) = (core("phone"), core("pc"));
    let a_id = a.device_id();
    let mut net = Net::new(a, b);

    // Bravo hears about alpha while alpha is still a stranger.
    discover(&mut net, Side::B, Side::A);

    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
        },
    );
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });

    // Bravo was dialled, so the handshake taught it nothing about alpha's
    // address; the earlier announcement has to carry it.
    net.local(Side::B, LocalCommand::Connect { peer: a_id.clone() });
    assert_eq!(net.b.peer_state(&a_id), PeerState::Reachable);
}

#[test]
fn a_peer_with_no_address_explains_itself() {
    // Not knowing where a device is differs from failing to reach it, and
    // only one of those the user can act on.
    let (a, b) = (core("phone"), core("pc"));
    let a_id = a.device_id();
    let mut net = Net::new(a, b);
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
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
            .is_some_and(|w| w.contains("asleep or unreachable")),
        "and a screen drawing that peer must be able to read the same reason, got {:?}",
        net.b.dial_trouble(&a_id)
    );
}

#[test]
fn a_machine_busy_pairing_says_so_in_its_advertisement() {
    // `pair=1` (PROTOCOL.md § 4) means *busy*, not *ready*: it lets a phone
    // grey out a row instead of offering a tap that cannot work.
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    assert!(!net.b.pairing_open(), "nobody is pairing to begin with");

    ask_to_pair(&mut net);
    assert!(
        net.b.pairing_open(),
        "a machine comparing digits is busy, and says so"
    );

    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });

    assert!(
        !net.b.pairing_open(),
        "and a pairing that has done its job must come off the air again"
    );
}

#[test]
fn a_pairing_that_lapses_stops_advertising_itself() {
    // Nobody is told a pairing timed out, so an advertisement driven by
    // events would go on claiming to be busy forever; this must be read live.
    let (a, b) = (core("phone"), core("pc"));
    let mut net = Net::new(a, b);
    ask_to_pair(&mut net);
    assert!(net.b.pairing_open());

    net.now += CoreConfig::default().pairing_window_ms + 1;
    let _ = net.b.handle(
        Now {
            monotonic_ms: net.now,
            wall_ms: net.wall,
        },
        Event::Tick,
    );

    assert!(
        !net.b.pairing_open(),
        "a lapsed pairing must not still be advertised as busy"
    );
}

#[test]
fn pairing_opens_a_session_rather_than_waiting_to_be_dialled() {
    // A phone always dials and is never dialled, so if `confirm_pairing`
    // leaves it to the peer to open a session "when it wants one", the
    // phone's side never connects.
    let (a, b) = (core("phone"), core("pc"));
    let b_id = b.device_id();
    let mut net = Net::new(a, b);

    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
        },
    );
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });

    // The armed heartbeat wakes the runtime immediately; this is that tick.
    net.queue.push_back((Side::A, Event::Tick));
    net.run();

    assert_eq!(
        net.a.peer_state(&b_id),
        PeerState::Reachable,
        "the side that just paired must not have to wait for a sighting that may never come"
    );
}

#[test]
fn a_peer_nothing_can_reach_is_tried_again_without_a_new_sighting() {
    // Auto-connect fires on a sighting, but mDNS resolves a service once and
    // then says nothing, so a lost session with no fresh advertisement needs
    // its own retry.
    let (mut net, _a_id, b_id) = paired();
    discover(&mut net, Side::A, Side::B);
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);

    // No second sighting follows: mDNS resolved it once.
    lose_link(&mut net, TRANSPORT);
    assert_eq!(net.a.peer_state(&b_id), PeerState::Unreachable);

    // Both clocks: monotonic drives the heartbeat, wall stamps the next
    // `Hello` so it isn't refused as a replay.
    net.now += CoreConfig::default().reconnect_every_ms + 1;
    net.wall += CoreConfig::default().reconnect_every_ms + 1;
    net.queue.push_back((Side::A, Event::Tick));
    net.run();

    assert_eq!(
        net.a.peer_state(&b_id),
        PeerState::Reachable,
        "the address is still on file, so something must try it again"
    );
}

#[test]
fn a_dial_nobody_answers_does_not_hold_the_remaining_routes_hostage() {
    // A dial that hangs forever (Network.framework waiting on a connection
    // with no viable path, rather than failing) must not block the retry
    // heartbeat from ever trying the routes behind it.
    let (mut net, _a_id, b_id) = paired();
    net.ble_transport = Some(SLOWER);

    // Up on the better transport first, the situation a takeover starts from.
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "B");
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
    // Recorded without dialling: the peer is already reachable over the best
    // transport there is.
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "silent");
    discover_via(&mut net, Side::A, Side::B, SLOWER, "B");

    lose_link(&mut net, TRANSPORT);
    assert_eq!(net.a.peer_state(&b_id), PeerState::Unreachable);
    net.dialed.clear();

    // The retry begins at once and finds the route that no longer answers.
    net.wall += 1_000;
    net.queue.push_back((Side::A, Event::Tick));
    net.run();
    assert_eq!(
        net.dialed,
        vec![(TRANSPORT, "silent".to_string())],
        "the better route is tried first, and it is the one that hangs"
    );
    assert_eq!(
        net.a.peer_state(&b_id),
        PeerState::Connecting,
        "a dial is out, so this is a device being reached and not one given up on"
    );

    // Not a moment before the budget is up: abandoning a dial early is the
    // same bug pointing the other way.
    net.now += CoreConfig::default().dial_timeout_ms - 1;
    net.wall += CoreConfig::default().dial_timeout_ms - 1;
    net.queue.push_back((Side::A, Event::Tick));
    net.run();
    assert_eq!(
        net.dialed,
        vec![(TRANSPORT, "silent".to_string())],
        "a dial still inside its budget is still a dial, not a spent route"
    );

    // The dial is given up on, and the walk carries on to the working route.
    net.now += 2;
    net.wall += 2;
    net.queue.push_back((Side::A, Event::Tick));
    net.run();

    assert!(
        net.dialed.contains(&(SLOWER, "B".to_string())),
        "a dial that never answered has to spend its route, or the ones behind \
         it are never tried: {:?}",
        net.dialed
    );
    assert_eq!(
        net.a.peer_state(&b_id),
        PeerState::Reachable,
        "the second radio was there the whole time"
    );
    assert_eq!(
        net.a.transport_for(&b_id),
        Some(TransportKind::BleGatt),
        "and it is what carries now"
    );
}

#[test]
fn an_automatic_dial_waits_for_the_one_already_out() {
    // Sightings arrive at whatever rate two radios feel like, and dialling
    // on each would open a fresh connection while the last was still coming
    // up, so an automatic attempt must stand down when one is outstanding.
    let (mut net, _a_id, b_id) = paired();
    // Pairing dialled to get here; this is about what happens afterwards.
    net.dialed.clear();

    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "silent");
    assert_eq!(
        net.dialed,
        vec![(TRANSPORT, "silent".to_string())],
        "the first sighting dials"
    );

    // Seen again, and again, with the first dial still in the air.
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "silent");
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "silent");
    assert_eq!(
        net.dialed.len(),
        1,
        "every sighting dialled again while one was already outstanding: {:?}",
        net.dialed
    );
    // A dial in the air is not a link, but it must not be reported as
    // unreachable either.
    assert_eq!(net.a.peer_state(&b_id), PeerState::Connecting);

    // Nor does a person asking start a second dial on top.
    net.ui.clear();
    net.local(Side::A, LocalCommand::Connect { peer: b_id.clone() });
    assert_eq!(
        net.dialed.len(),
        1,
        "a request piled a second dial onto one already in flight: {:?}",
        net.dialed
    );

    // They are still owed the outcome: an automatic attempt says nothing
    // when it runs out of routes, so whoever asked must not be left waiting
    // on an event that never comes.
    net.now += CoreConfig::default().dial_timeout_ms + 1;
    net.wall += CoreConfig::default().dial_timeout_ms + 1;
    net.queue.push_back((Side::A, Event::Tick));
    net.run();
    assert!(
        net.saw(Side::A, |e| matches!(
            e,
            UiEvent::PeerUnreachable { peer } if *peer == b_id
        )),
        "a person asked, the attempt ended, and nothing told them"
    );
}

#[test]
fn asking_for_a_peer_that_is_already_connected_is_answered_at_once() {
    // The answer arrived before the question, so it must be repeated rather
    // than leaving the caller to wait out its whole timeout for nothing.
    let (mut net, _a_id, b_id) = paired();
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "B");
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);

    net.dialed.clear();
    net.ui.clear();
    net.local(Side::A, LocalCommand::Connect { peer: b_id.clone() });

    assert!(
        net.dialed.is_empty(),
        "a connected peer was dialled again: {:?}",
        net.dialed
    );
    assert!(
        net.saw(Side::A, |e| matches!(
            e,
            UiEvent::PeerReachable { peer, .. } if *peer == b_id
        )),
        "nothing answered a request about a peer that was already there"
    );
}

#[test]
fn the_core_gives_up_on_a_dial_later_than_the_hosts_do() {
    // The core's timeout is a backstop for a host that doesn't bound its own
    // dial. If the backstop fired first, it would take the answer away from
    // the host that could still clean up its connection.
    assert!(
        CoreConfig::default().dial_timeout_ms > acrylius_core::link::DIAL_TIMEOUT_MS,
        "the backstop must outlast the bound the hosts are told to use"
    );
}

#[test]
fn every_route_gets_its_own_budget_before_the_next_is_tried() {
    // One deadline per dial, not one for the whole walk, and a walk where
    // every route hangs must still end rather than sit at "Connecting" forever.
    let (mut net, _a_id, b_id) = paired();

    // Both routes on file up front and neither ever answers; a pair of
    // sightings couldn't arrange this since the first would dial and the
    // second would stand down behind it.
    net.local(
        Side::A,
        LocalCommand::SetPeerAddress {
            peer: b_id.clone(),
            transport: TRANSPORT,
            addr: "silent".to_string(),
        },
    );
    net.local(
        Side::A,
        LocalCommand::SetPeerAddress {
            peer: b_id.clone(),
            transport: SLOWER,
            addr: "silent".to_string(),
        },
    );
    net.dialed.clear();
    net.local(Side::A, LocalCommand::Connect { peer: b_id.clone() });
    assert_eq!(
        net.dialed,
        vec![(TRANSPORT, "silent".to_string())],
        "the better route goes first"
    );

    // The first route's budget runs out and the second is tried.
    net.now += CoreConfig::default().dial_timeout_ms + 1;
    net.wall += CoreConfig::default().dial_timeout_ms + 1;
    net.queue.push_back((Side::A, Event::Tick));
    net.run();
    assert_eq!(
        net.dialed,
        vec![
            (TRANSPORT, "silent".to_string()),
            (SLOWER, "silent".to_string()),
        ],
        "the walk carried on to the route behind it"
    );

    // The second route gets the same wait as the first, not an already-spent
    // deadline.
    net.now += CoreConfig::default().dial_timeout_ms - 1;
    net.wall += CoreConfig::default().dial_timeout_ms - 1;
    net.queue.push_back((Side::A, Event::Tick));
    net.run();
    assert_eq!(
        net.a.peer_state(&b_id),
        PeerState::Connecting,
        "the second route was given up on before it had its turn"
    );

    // The walk ends with a reason on file, not a peer stuck "Connecting".
    net.now += 2;
    net.wall += 2;
    net.queue.push_back((Side::A, Event::Tick));
    net.run();
    assert_eq!(net.a.peer_state(&b_id), PeerState::Unreachable);
    assert!(
        net.a
            .dial_trouble(&b_id)
            .is_some_and(|w| w.contains("didn't answer")),
        "a walk that ended in silence still owes the screen an explanation, got {:?}",
        net.a.dial_trouble(&b_id)
    );
}

#[test]
fn a_better_transport_is_dialled_once_while_a_worse_one_carries() {
    // Wi-Fi is dialled behind an already-working Bluetooth link (a Bluetooth
    // link cannot carry a file); repeat sightings while that dial is in
    // flight must not open a second one.
    let (mut net, _a_id, b_id) = paired();
    net.ble_transport = Some(SLOWER);

    // Pairing left a Wi-Fi address on file; it has to stop working first,
    // the same setup `both_routes_up` uses.
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "not-listening");
    discover_via(&mut net, Side::A, Side::B, SLOWER, "B");
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
    assert_eq!(net.a.transport_for(&b_id), Some(TransportKind::BleGatt));

    net.dialed.clear();
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "silent");
    assert_eq!(
        net.dialed,
        vec![(TRANSPORT, "silent".to_string())],
        "the better transport is worth dialling even while the worse one works"
    );

    // Seen again, twice, with that dial still in the air.
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "silent");
    discover_via(&mut net, Side::A, Side::B, SLOWER, "B");
    assert_eq!(
        net.dialed.len(),
        1,
        "a sighting opened a second connection while one was already in \
         flight: {:?}",
        net.dialed
    );
    // Still carried by what actually works, throughout.
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
    assert_eq!(net.a.transport_for(&b_id), Some(TransportKind::BleGatt));
}

#[test]
fn the_network_coming_back_moves_a_peer_off_the_worse_radio() {
    // Wi-Fi returning announces nothing to the core (the peer never stopped
    // being reachable), and mDNS won't produce a fresh sighting on its own,
    // so a phone could sit on Bluetooth indefinitely with Wi-Fi available.
    let (mut net, _a_id, b_id) = paired();
    net.ble_transport = Some(SLOWER);

    // On Bluetooth, with Wi-Fi not answering.
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "not-listening");
    discover_via(&mut net, Side::A, Side::B, SLOWER, "B");
    assert_eq!(net.a.transport_for(&b_id), Some(TransportKind::BleGatt));

    // Wi-Fi comes back at the address already on file; nothing tells the core.
    net.local(
        Side::A,
        LocalCommand::SetPeerAddress {
            peer: b_id.clone(),
            transport: TRANSPORT,
            addr: "B".to_string(),
        },
    );
    net.dialed.clear();
    net.wall += 1_000;

    net.local(Side::A, LocalCommand::ReconsiderRoutes);

    assert_eq!(
        net.dialed,
        vec![(TRANSPORT, "B".to_string())],
        "the better transport was never tried"
    );
    assert_eq!(
        net.a.transport_for(&b_id),
        Some(TransportKind::UnixLoopback),
        "Wi-Fi came back and the session stayed on Bluetooth"
    );
    // The Bluetooth link is left alone rather than torn down.
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
}

#[test]
fn reconsidering_routes_leaves_a_peer_already_on_the_best_one_alone() {
    // This fires on every network change, so a peer with nothing better to
    // move to must not be dialled again each time.
    let (mut net, _a_id, b_id) = paired();
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "B");
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);

    net.dialed.clear();
    net.local(Side::A, LocalCommand::ReconsiderRoutes);
    net.local(Side::A, LocalCommand::ReconsiderRoutes);

    assert!(
        net.dialed.is_empty(),
        "a peer on the best route it has was dialled anyway: {:?}",
        net.dialed
    );
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
}

#[test]
fn a_machine_that_leaves_the_network_stops_being_offered() {
    // A machine switched off must come off the pairing screen, not stay
    // listed as something to pair with forever.
    let (mut net, _a_id, _b_id) = paired();
    let stranger = net.c.fingerprint();

    net.queue.push_back((
        Side::A,
        Event::Discovered {
            transport: TRANSPORT,
            peer: DiscoveredPeer {
                fingerprint: Some(stranger.clone()),
                name: "bravo".to_string(),
                addr: "10.0.0.9:1971".to_string(),
                pairing: false,
            },
        },
    ));
    net.run();
    assert!(
        net.saw(Side::A, |e| matches!(e, UiEvent::Discovered { .. })),
        "a stranger on the network is worth mentioning"
    );

    net.ui.clear();
    net.queue.push_back((
        Side::A,
        Event::Undiscovered {
            transport: TRANSPORT,
            addr: "10.0.0.9:1971".to_string(),
        },
    ));
    net.run();

    assert!(
        net.saw(Side::A, |e| matches!(
            e,
            UiEvent::Undiscovered { fingerprint } if *fingerprint == stranger
        )),
        "nothing took the machine back off the list"
    );
}

#[test]
fn a_withdrawal_naming_an_older_address_leaves_a_newer_one_alone() {
    // A machine that changes address is resolved at the new one and then
    // withdrawn at the old; removing by transport alone would un-list it.
    let (mut net, _a_id, _b_id) = paired();
    let stranger = net.c.fingerprint();

    for addr in ["10.0.0.9:1971", "10.0.0.12:1971"] {
        net.queue.push_back((
            Side::A,
            Event::Discovered {
                transport: TRANSPORT,
                peer: DiscoveredPeer {
                    fingerprint: Some(stranger.clone()),
                    name: "bravo".to_string(),
                    addr: addr.to_string(),
                    pairing: false,
                },
            },
        ));
    }
    net.run();
    net.ui.clear();

    net.queue.push_back((
        Side::A,
        Event::Undiscovered {
            transport: TRANSPORT,
            addr: "10.0.0.9:1971".to_string(),
        },
    ));
    net.run();

    assert!(
        !net.saw(Side::A, |e| matches!(e, UiEvent::Undiscovered { .. })),
        "the old address expiring took the machine off the list with it"
    );
}

#[test]
fn a_machine_seen_two_ways_stays_listed_while_either_can_see_it() {
    // Wi-Fi and Bluetooth come and go independently, so Wi-Fi lapsing must
    // not remove a machine that Bluetooth can still see.
    let (mut net, _a_id, _b_id) = paired();
    let stranger = net.c.fingerprint();

    for (transport, addr) in [(TRANSPORT, "10.0.0.9:1971"), (SLOWER, "ble:abcd")] {
        net.queue.push_back((
            Side::A,
            Event::Discovered {
                transport,
                peer: DiscoveredPeer {
                    fingerprint: Some(stranger.clone()),
                    name: "bravo".to_string(),
                    addr: addr.to_string(),
                    pairing: false,
                },
            },
        ));
    }
    net.run();
    net.ui.clear();

    // Wi-Fi loses it.
    net.queue.push_back((
        Side::A,
        Event::Undiscovered {
            transport: TRANSPORT,
            addr: "10.0.0.9:1971".to_string(),
        },
    ));
    net.run();
    assert!(
        !net.saw(Side::A, |e| matches!(e, UiEvent::Undiscovered { .. })),
        "one radio losing sight of a machine took it off the list entirely"
    );

    // The other transport loses it too, so the machine is actually gone now.
    net.queue.push_back((
        Side::A,
        Event::Undiscovered {
            transport: SLOWER,
            addr: "ble:abcd".to_string(),
        },
    ));
    net.run();
    assert!(
        net.saw(Side::A, |e| matches!(
            e,
            UiEvent::Undiscovered { fingerprint } if *fingerprint == stranger
        )),
        "nothing can see it any more and it is still on offer"
    );
}

#[test]
fn a_paired_peer_leaving_the_network_is_not_announced_as_a_stranger() {
    // A paired peer's row comes from session state, not from mDNS. Reporting
    // it as `Undiscovered` would ask a host to remove something it never
    // added via `Discovered`.
    let (mut net, _a_id, b_id) = paired();
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "B");
    net.ui.clear();

    net.queue.push_back((
        Side::A,
        Event::Undiscovered {
            transport: TRANSPORT,
            addr: "B".to_string(),
        },
    ));
    net.run();

    assert!(
        !net.saw(Side::A, |e| matches!(e, UiEvent::Undiscovered { .. })),
        "a paired peer was reported as a stranger leaving"
    );
    // The last-reached address survives; the retry heartbeat has nothing
    // else to go on.
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);
    net.dialed.clear();
    lose_link(&mut net, TRANSPORT);
    net.now += CoreConfig::default().reconnect_every_ms + 1;
    net.wall += CoreConfig::default().reconnect_every_ms + 1;
    net.queue.push_back((Side::A, Event::Tick));
    net.run();
    assert!(
        net.dialed.contains(&(TRANSPORT, "B".to_string())),
        "an mDNS record lapsing threw away the address a paired peer works at"
    );
}

#[test]
fn a_peer_that_could_not_be_reached_records_why_without_announcing_it() {
    // Every dial is automatic, run on every sighting, so a failed dial must
    // be state a screen can ask for, not an announcement that flickers an
    // error at a device coming up normally.
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
    // Real machines do not boot together; a computer up for hours and a
    // phone just unlocked share no uptime at all.
    let (a, b) = (core("phone"), core("pc"));
    let b_id = b.device_id();
    let mut net = Net::new(a, b);
    // The PC has been up an hour longer than the phone.
    net.b_skew = 3_600_000;

    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
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
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
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
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
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
/// A receiver renumbers an offer on arrival, so this is the only way a test
/// can learn what it is going to call the thing.
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
    // The receiver keys transfers by its own number, so the two ends
    // disagree about what a transfer is called; every message between them
    // must be translated, and the bulk key must derive from the sender's
    // number or nothing decrypts.
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
    // Not just accept: a reject carrying the receiver's own number would
    // name a transfer the sender never had.
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
    // The other way a receiver can end a transfer, so the number has to be
    // translated back here too.
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
    // The mirror direction: translation on the way in. A host knows nothing
    // of the sender's numbering, so an ending must arrive under its own.
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
    // Accepting a file binds a port and reserves a filename; a sender whose
    // session dies between accept and dial must not hold both forever.
    let (mut net, b_id) = sharing_pair();
    let a_id = net.a.device_id();

    // Numbered differently at the two ends, so telling the sender requires
    // translation.
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

    // Not early: a slow-to-start transfer is not a failed one.
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
    // The far end is told too, the rule every other bulk ending follows.
    assert!(
        told_it_finished(&net, Side::A, false),
        "the sender was not told the transfer it had been offered is over"
    );
}

#[test]
fn a_file_still_arriving_is_not_given_up_on() {
    // Nothing in the core knows how long a gigabyte should take, so the
    // deadline covers only the wait for a sender, never the file itself.
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

    // Both ends were handed a key, and they match, filed under each one's own
    // number.
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
    // Otherwise this is a file drop for anything ever paired with the machine.
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
    // A link that can't carry bulk data must refuse with a clear error, not
    // silently try to push a gigabyte through 185-byte BLE writes.
    let (mut net, b_id) = ble_sharing_pair();

    plugin(&mut net, Side::A, &b_id, "offer", offer_body(1, 4096));

    // On the sender, before anything leaves: refusing only on the receiver
    // leaves the asking phone stuck on "offered" with no visible error.
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
    // A refusal here sends nothing, so it must be announced locally rather
    // than leaving the file listed as sending forever.
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
    // A phone finds Bluetooth first, and Bluetooth cannot carry a file;
    // Wi-Fi coming up has to un-refuse transfers, not leave them stuck.
    let (a, b) = (sharing_core("phone"), sharing_core("pc"));
    let b_id = b.device_id();
    let mut net = Net::new(a, b);
    // The slower transport is the radio; the better one is the network.
    net.ble_transport = Some(SLOWER);
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
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

    // The clock has to move or the new opener looks like a replay.
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
fn a_bulk_transfer_is_offered_even_when_the_freshest_link_cannot_carry_it() {
    // A link with no side channel (BLE, or USB) can still be the freshest;
    // an older link that has one must still be found for a bulk transfer.
    let (a, b) = (sharing_core("phone"), sharing_core("pc"));
    let b_id = b.device_id();
    let mut net = Net::new(a, b);
    // Stands in for USB: preferred (lower id) but no side channel of its own.
    net.ble_transport = Some(TRANSPORT);
    net.local(
        Side::A,
        LocalCommand::RequestPairing {
            transport: SLOWER,
            addr: Side::B.addr().to_string(),
        },
    );
    net.local(Side::A, LocalCommand::ConfirmPairing { accept: true });
    net.local(Side::B, LocalCommand::ConfirmPairing { accept: true });
    net.queue.push_back((Side::A, Event::Tick));
    net.run();
    assert_eq!(net.a.peer_state(&b_id), PeerState::Reachable);

    // TRANSPORT then takes over as the freshest link, but it cannot carry bulk.
    net.wall += 1_000;
    discover_via(&mut net, Side::A, Side::B, TRANSPORT, "B");
    assert_eq!(
        net.a.transport_for(&b_id),
        Some(TransportKind::BleGatt),
        "the freshest link is the one with no side channel"
    );

    plugin(&mut net, Side::A, &b_id, "offer", offer_body(1, 4096));
    assert!(
        !net.saw(Side::A, |e| matches!(e, UiEvent::Error { .. })),
        "an older link with a side channel must still be found"
    );
    assert!(
        net.saw(Side::B, |e| matches!(
            e,
            UiEvent::Plugin { ty, .. } if ty == "offer"
        )),
        "the offer reached the far end over the link that can carry it"
    );
}

#[test]
fn a_link_that_can_carry_files_is_left_alone() {
    // The check must key on what the link said, not on there being a check
    // at all: a TCP pair still offers and listens as before.
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
    // A BLE link takes 16 KiB; anything bigger must be refused, not
    // fragmented. Sent over `ping`, which forwards a body verbatim.
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
    // A phone advertises the capability, so `send` doesn't fail with
    // `cap_not_negotiated`, but has no download directory and no way to
    // ask a person; it must refuse rather than queue the offer forever.
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
        Side::A,
        LocalCommand::RequestPairing {
            transport: TRANSPORT,
            addr: Side::B.addr().to_string(),
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

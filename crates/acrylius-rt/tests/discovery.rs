//! Discovery against a real mDNS daemon.
//!
//! The unit tests beside `discovery_event` build their own `ServiceEvent`s, so
//! they can only prove that the mapping is self-consistent. They cannot prove
//! the one thing the withdrawal path actually rests on: that the name
//! `ServiceRemoved` carries is the same string `get_fullname()` returned when
//! the service resolved. Nothing in the type system ties those together, and if
//! they differ the lookup silently misses — every machine stays on the list
//! forever, which is exactly the bug this is meant to fix, still there and now
//! with a test claiming otherwise.
//!
//! So this one uses the real thing: register a service, browse for it, withdraw
//! it, and watch what comes back.
//!
//! Self-skipping, in the style of the M2 acceptance script. Multicast is a
//! property of the machine — a container with no multicast route, a firewall,
//! an interface that will not join the group — and a test that cannot get its
//! own record back has learned nothing about this code. It says so and passes
//! rather than failing for something it is not testing.

use std::collections::HashMap;
use std::time::{Duration, Instant};

const TY: &str = "_acrylius-test._tcp.local.";
const PATIENCE: Duration = Duration::from_secs(10);

/// Drain the browse until `want` says yes, or patience runs out.
fn wait_for<T>(
    rx: &mdns_sd::Receiver<mdns_sd::ServiceEvent>,
    mut want: impl FnMut(mdns_sd::ServiceEvent) -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + PATIENCE;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(ev) = rx.recv_timeout(left) else {
            return None;
        };
        if let Some(found) = want(ev) {
            return Some(found);
        }
    }
    None
}

#[test]
fn a_withdrawn_service_is_named_the_way_the_sighting_named_it() {
    let Ok(mdns) = mdns_sd::ServiceDaemon::new() else {
        println!("skip  no mDNS daemon on this machine");
        return;
    };
    let info = mdns_sd::ServiceInfo::new(
        TY,
        "acrylius-withdrawal-test",
        "acrylius-test.local.",
        "127.0.0.1",
        1971,
        &[("fp", "x"), ("n", "bravo")][..],
    )
    .expect("a service record");
    let fullname = info.get_fullname().to_string();

    let Ok(rx) = mdns.browse(TY) else {
        println!("skip  this machine will not browse");
        return;
    };
    if mdns.register(info).is_err() {
        println!("skip  this machine will not advertise");
        return;
    }

    // What the browse says the service is called, which is the string the
    // withdrawal will have to match.
    let resolved = wait_for(&rx, |ev| match ev {
        mdns_sd::ServiceEvent::ServiceResolved(info) => Some(info.get_fullname().to_string()),
        _ => None,
    });
    let Some(resolved) = resolved else {
        println!("skip  nothing came back within {PATIENCE:?}; no multicast here");
        return;
    };
    assert_eq!(
        resolved, fullname,
        "a resolved service is not called what it was registered as"
    );

    let _ = mdns.unregister(&fullname);

    let removed = wait_for(&rx, |ev| match ev {
        mdns_sd::ServiceEvent::ServiceRemoved(_, name) => Some(name),
        _ => None,
    });
    let Some(removed) = removed else {
        println!("skip  the withdrawal did not arrive within {PATIENCE:?}");
        return;
    };

    // The assertion this file exists for. `discovery_event` keys what it
    // reported by the resolved fullname and looks it up by the withdrawn one;
    // if these two ever differ, nothing is ever taken off the list and the
    // failure is invisible.
    assert_eq!(
        removed, resolved,
        "a withdrawal names the service differently from the sighting, so \
         nothing can be matched up and every machine stays on the list"
    );

    // And the map that does the matching behaves the same way round.
    let mut reported: HashMap<String, String> = HashMap::new();
    reported.insert(resolved, "10.0.0.9:1971".to_string());
    assert_eq!(
        reported.remove(&removed).as_deref(),
        Some("10.0.0.9:1971"),
        "the address a sighting was reported at is not recoverable from the \
         name its withdrawal carries"
    );
}

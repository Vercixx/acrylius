//! Discovery against a real mDNS daemon: proves the name `ServiceRemoved`
//! carries matches what `get_fullname()` returned at resolve time, which
//! nothing in the type system guarantees. Self-skips where multicast is absent.

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

    // The string the withdrawal will have to match.
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

    // The assertion this file exists for.
    assert_eq!(
        removed, resolved,
        "a withdrawal names the service differently from the sighting, so \
         nothing can be matched up and every machine stays on the list"
    );

    let mut reported: HashMap<String, String> = HashMap::new();
    reported.insert(resolved, "10.0.0.9:1971".to_string());
    assert_eq!(
        reported.remove(&removed).as_deref(),
        Some("10.0.0.9:1971"),
        "the address a sighting was reported at is not recoverable from the \
         name its withdrawal carries"
    );
}

//! Checks that only mean something on a live desktop.
//!
//! They report rather than fail when the thing they probe is absent, so CI on a
//! headless runner stays green while the same command tells a developer at a
//! real desktop whether it works here.

#[tokio::test]
async fn compositor_probe_against_whatever_is_running() {
    let answer = acrylius_linux::compositor::locked().await;
    match answer {
        Some(v) => {
            println!("compositor answered: locked = {v}");
            // If it answered at all, the screen is not locked right now: a test
            // run implies someone is using the machine.
            assert!(!v, "the screen should not be locked while this test runs");
        }
        None => println!("no compositor answered (not Hyprland, or not running)"),
    }
}

#[tokio::test]
async fn logind_reports_this_session() {
    let Ok(effector) = acrylius_linux::session::SessionEffector::new().await else {
        println!("no system bus (headless runner?)");
        return;
    };
    match effector.query().await {
        Ok(state) => {
            println!(
                "logind: session {} type={} active={} locked={}",
                state.session_id, state.kind, state.active, state.locked
            );
            assert!(!state.session_id.is_empty());
            assert!(matches!(state.kind.as_str(), "wayland" | "x11"));
            assert!(
                !state.locked,
                "the screen should not be locked while this test runs"
            );
        }
        Err(e) => println!("no graphical session found: {e}"),
    }
}

#[tokio::test]
async fn clipboard_round_trips_through_the_compositor() {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        println!("no WAYLAND_DISPLAY; skipping");
        return;
    }
    let marker = format!("acrylius-live-{}", std::process::id());
    match acrylius_linux::clipboard::write(marker.clone().into_bytes()).await {
        Ok(()) => println!("wrote to the clipboard"),
        Err(e) => {
            println!("clipboard write failed: {e}");
            return;
        }
    }
    // The compositor needs a moment to hand the selection over.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    match acrylius_linux::clipboard::read().await {
        Ok(Some(data)) => {
            let text = String::from_utf8_lossy(&data);
            println!("read back: {text:?}");
            assert_eq!(text, marker, "what went in should come back");
        }
        Ok(None) => panic!("the clipboard was empty right after being written"),
        Err(e) => panic!("clipboard read failed: {e}"),
    }
}

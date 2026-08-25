//! Asking the compositor whether the screen is actually locked.
//!
//! logind's `LockedHint` is only as good as the screen locker maintaining it,
//! and some maintain it not at all. Noctalia on Hyprland locks the screen while
//! leaving the hint reading `no`. Believing the hint meant unlock answered
//! "already unlocked" and did nothing, and lock reported failure over a screen
//! it had just locked.
//!
//! So a hint of `yes` is trusted as-is, and only a `no` on an active Wayland
//! session is escalated to the compositor. A compositor that cannot answer
//! returns [`None`], meaning no opinion, never "unlocked". That distinction
//! is the whole point: a failed probe must leave the hint standing rather than
//! assert the more dangerous of the two answers.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Candidate Hyprland IPC sockets, best first.
///
/// `HYPRLAND_INSTANCE_SIGNATURE` is tried first but cannot be trusted alone:
/// systemd's user manager caches the environment it was given, so after a
/// Hyprland restart the variable still names an instance that is gone. The
/// fallback scans the directory newest-first.
fn candidates() -> Vec<PathBuf> {
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) else {
        return Vec::new();
    };
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").map(PathBuf::from);
    let hypr = runtime.join("hypr");

    let mut found: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&hypr)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((modified, e.path()))
        })
        .collect();
    // Newest first: after a Hyprland restart the freshest directory is the
    // live one.
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));

    order(
        &hypr,
        signature.as_deref(),
        found.into_iter().map(|(_, p)| p),
    )
}

/// Which sockets to try, best first.
///
/// Split out from the directory listing so the ordering and the traversal check
/// can be tested without a filesystem or environment variables.
fn order(
    hypr: &std::path::Path,
    signature: Option<&std::path::Path>,
    newest_first: impl IntoIterator<Item = PathBuf>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(sig) = signature {
        // A signature names one directory. Anything with a separator in it is
        // not a signature, and joining it would be a path traversal.
        if sig.components().count() == 1 {
            out.push(hypr.join(sig).join(".socket.sock"));
        }
    }
    out.extend(newest_first.into_iter().map(|d| d.join(".socket.sock")));
    out.dedup();
    out
}

/// Ask Hyprland whether the session is locked.
///
/// Speaks the socket directly rather than shelling out to `hyprctl`: the binary
/// may not be on `PATH` for a systemd user unit, while the socket is already
/// inside the paths such a unit is allowed to touch.
pub async fn locked() -> Option<bool> {
    for path in candidates() {
        match tokio::time::timeout(PROBE_TIMEOUT, ask(&path)).await {
            Ok(Ok(v)) => return Some(v),
            Ok(Err(e)) => tracing::debug!(path = %path.display(), error = %e, "probe failed"),
            Err(_) => tracing::debug!(path = %path.display(), "probe timed out"),
        }
    }
    // No opinion. The caller must leave the logind hint standing.
    None
}

async fn ask(path: &std::path::Path) -> anyhow::Result<bool> {
    let mut sock = UnixStream::connect(path).await?;
    sock.write_all(b"j/locked").await?;
    sock.flush().await?;
    let mut reply = String::new();
    sock.read_to_string(&mut reply).await?;
    let value: serde_json::Value = serde_json::from_str(&reply)?;
    value
        .get("locked")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| anyhow::anyhow!("no `locked` field in {reply:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hypr() -> PathBuf {
        PathBuf::from("/run/user/1000/hypr")
    }

    #[test]
    fn a_signature_with_a_separator_is_not_used_as_a_path() {
        // A traversal here would have us connect to an arbitrary socket.
        let out = order(&hypr(), Some(std::path::Path::new("../../../tmp/evil")), []);
        assert!(
            out.is_empty(),
            "a signature containing separators must be ignored"
        );
    }

    #[test]
    fn the_signature_is_tried_first_then_newest_first() {
        // systemd's user manager caches the environment it was started with, so
        // after a Hyprland restart the signature names an instance that is gone.
        // It is still worth trying first, but it must not be the only candidate.
        let out = order(
            &hypr(),
            Some(std::path::Path::new("stale")),
            [hypr().join("newest"), hypr().join("older")],
        );
        assert_eq!(
            out,
            vec![
                hypr().join("stale").join(".socket.sock"),
                hypr().join("newest").join(".socket.sock"),
                hypr().join("older").join(".socket.sock"),
            ]
        );
    }

    #[test]
    fn the_signature_is_not_tried_twice() {
        let out = order(
            &hypr(),
            Some(std::path::Path::new("live")),
            [hypr().join("live")],
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn no_signature_still_scans() {
        let out = order(&hypr(), None, [hypr().join("only")]);
        assert_eq!(out, vec![hypr().join("only").join(".socket.sock")]);
    }
}

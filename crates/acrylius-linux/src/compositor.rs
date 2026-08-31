//! Asking the compositor whether the screen is actually locked.
//!
//! logind's `LockedHint` can be wrong (Hyprland/Noctalia reports `no` while
//! locked); a `no` on an active Wayland session is escalated to the
//! compositor, but `None` means no opinion, never "unlocked".

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Candidate Hyprland IPC sockets, best first.
///
/// `HYPRLAND_INSTANCE_SIGNATURE` may be stale after a Hyprland restart
/// (systemd caches the env it started with), so the rest scan newest-first.
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
    // Newest first: the freshest directory is the live one after a restart.
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));

    order(
        &hypr,
        signature.as_deref(),
        found.into_iter().map(|(_, p)| p),
    )
}

/// Which sockets to try, best first; split out so ordering is testable without
/// a filesystem or environment variables.
fn order(
    hypr: &std::path::Path,
    signature: Option<&std::path::Path>,
    newest_first: impl IntoIterator<Item = PathBuf>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(sig) = signature {
        // A separator means it's not a bare signature; joining it would be a path traversal.
        if sig.components().count() == 1 {
            out.push(hypr.join(sig).join(".socket.sock"));
        }
    }
    out.extend(newest_first.into_iter().map(|d| d.join(".socket.sock")));
    out.dedup();
    out
}

/// Ask Hyprland whether the session is locked. Speaks the socket directly
/// rather than `hyprctl`, which may not be on `PATH` for a systemd user unit.
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
        // A traversal here would connect to an arbitrary socket.
        let out = order(&hypr(), Some(std::path::Path::new("../../../tmp/evil")), []);
        assert!(
            out.is_empty(),
            "a signature containing separators must be ignored"
        );
    }

    #[test]
    fn the_signature_is_tried_first_then_newest_first() {
        // The signature can be stale after a restart, but still worth trying first.
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

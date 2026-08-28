//! The machine's output volume.
//!
//! MPRIS gives every player a `Volume` property, and it is writable, and a
//! great many players accept the write and ignore it — Chromium and anything
//! built on it do exactly that while reporting `CanControl: true`. A remote
//! whose volume slider works only for some of what you play is worse than one
//! that always moves the same thing, so a volume command with no player named
//! moves the machine.
//!
//! This shells out, which the rest of this crate deliberately does not do: the
//! clipboard talks `ext-data-control` directly rather than running `wl-copy`.
//! There is no equivalent here. Every Rust route to PipeWire or PulseAudio is a
//! binding to their C library, which is a build dependency and a versioning
//! problem for one property; `wpctl` and `pactl` ship with the servers
//! themselves, so if there is audio at all one of them is present.

use std::process::Stdio;

use tokio::process::Command;

/// The output volume, 0 to 100, or `None` when there is no sound server.
///
/// A muted sink reports zero. That is what a person sees on their own screen,
/// and a remote showing 40 while nothing can be heard is a remote that is
/// wrong.
pub async fn volume() -> Option<u8> {
    if let Some(text) = run("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"]).await {
        // "Volume: 0.20" or "Volume: 0.20 [MUTED]"
        if text.contains("[MUTED]") {
            return Some(0);
        }
        return text
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.parse::<f64>().ok())
            .map(to_percent);
    }
    if let Some(text) = run("pactl", &["get-sink-volume", "@DEFAULT_SINK@"]).await {
        // "Volume: front-left: 32768 /  50% / -18.06 dB, ..."
        return text
            .split('/')
            .nth(1)
            .and_then(|v| v.trim().trim_end_matches('%').parse::<u8>().ok())
            .map(|p| p.min(100));
    }
    None
}

/// Set it. Reads back, because that is the whole reason this exists.
pub async fn set_volume(percent: u8) -> anyhow::Result<u8> {
    let percent = percent.min(100);
    let arg = format!("{percent}%");
    let done = run("wpctl", &["set-volume", "@DEFAULT_AUDIO_SINK@", &arg])
        .await
        .is_some()
        || run("pactl", &["set-sink-volume", "@DEFAULT_SINK@", &arg])
            .await
            .is_some();
    if !done {
        anyhow::bail!("no sound server here to set a volume on");
    }
    // Unmute, or a slider dragged up from zero appears to do nothing. Failing
    // is fine: a sink that was not muted has nothing to undo.
    //
    // Both tools, for the same reason the set above tries both. `wpctl` is
    // PipeWire's and does not exist on a PulseAudio-only machine, so this used
    // to set a volume there and leave the sink muted — the number moved, the
    // slider moved, and the room stayed silent.
    if percent > 0 {
        let unmuted = run("wpctl", &["set-mute", "@DEFAULT_AUDIO_SINK@", "0"])
            .await
            .is_some();
        if !unmuted {
            let _ = run("pactl", &["set-sink-mute", "@DEFAULT_SINK@", "0"]).await;
        }
    }
    let landed = volume()
        .await
        .ok_or_else(|| anyhow::anyhow!("the volume could not be read back"))?;
    if landed.abs_diff(percent) > 5 {
        anyhow::bail!("the volume did not move: it is still {landed}%");
    }
    Ok(landed)
}

fn to_percent(fraction: f64) -> u8 {
    (fraction.clamp(0.0, 1.0) * 100.0).round() as u8
}

/// Run a tool, or `None` if it is not installed or refused.
///
/// stdin is closed and stderr discarded: these are being asked a question, not
/// given a terminal, and a tool that decides to prompt would otherwise hang the
/// effect until its deadline.
async fn run(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fraction_becomes_a_percentage() {
        assert_eq!(to_percent(0.0), 0);
        assert_eq!(to_percent(0.2), 20);
        assert_eq!(to_percent(1.0), 100);
        // Some sinks allow over-amplification. A remote showing 140% would be
        // showing something its own slider cannot express.
        assert_eq!(to_percent(1.4), 100);
    }

    #[tokio::test]
    async fn asking_a_machine_with_no_sound_server_is_not_an_error() {
        // Nothing is asserted about the value: a build machine has no audio and
        // a test that demands some fails in CI for the wrong reason. What must
        // hold is that it answers rather than panicking or hanging.
        let _ = volume().await;
    }
}

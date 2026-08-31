//! The machine's output volume.
//!
//! Shells out to `wpctl`/`pactl` rather than binding PipeWire/PulseAudio in
//! Rust, since both tools ship with the servers themselves.

use std::process::Stdio;

use tokio::process::Command;

/// The output volume, 0-100, or `None` with no sound server. A muted sink
/// reports zero.
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
    // Unmute too, or dragging up from zero silently does nothing. Try both
    // tools: `wpctl` doesn't exist on PulseAudio-only machines.
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

/// Run a tool, or `None` if missing/refused. stdin closed, stderr discarded,
/// so a tool that tries to prompt can't hang the effect.
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
        // Some sinks allow over-amplification past 100%; clamp for the slider.
        assert_eq!(to_percent(1.4), 100);
    }

    #[tokio::test]
    async fn asking_a_machine_with_no_sound_server_is_not_an_error() {
        // No assertion on the value: CI has no audio; just must not panic or hang.
        let _ = volume().await;
    }
}

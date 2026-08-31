//! Media control through MPRIS.
//!
//! Every player speaks `org.mpris.MediaPlayer2` on the session bus. MPRIS is
//! widely implemented but rarely fully, so every property here is read defensively.

use std::collections::HashMap;

use acrylius_core::plugins::media::{MediaPlayer, MediaState, landed};
use acrylius_core::vocab::MediaAction;
use zbus::zvariant::{ObjectPath, OwnedValue};

/// The prefix every player's bus name carries.
const PREFIX: &str = "org.mpris.MediaPlayer2.";

/// How long to wait for a reading to reflect a command before answering anyway.
/// Lives in core next to the client's wait budget, so the two stay ordered.
const CONTROL_CONFIRM: std::time::Duration =
    std::time::Duration::from_millis(acrylius_core::plugins::media::CONTROL_CONFIRM_MS);

/// How often to re-read while waiting; short since most players act in well
/// under a tenth of a second.
const CONTROL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(60);

/// `playerctld` mirrors whichever player is active; skipped so it doesn't
/// duplicate an entry already listed.
const AGGREGATOR: &str = "playerctld";

#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2",
    default_path = "/org/mpris/MediaPlayer2"
)]
trait MediaPlayer2 {
    #[zbus(property)]
    fn identity(&self) -> zbus::Result<String>;
}

#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2.Player",
    default_path = "/org/mpris/MediaPlayer2"
)]
trait Player {
    fn play(&self) -> zbus::Result<()>;
    fn pause(&self) -> zbus::Result<()>;
    fn play_pause(&self) -> zbus::Result<()>;
    fn next(&self) -> zbus::Result<()>;
    fn previous(&self) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;
    fn seek(&self, offset_us: i64) -> zbus::Result<()>;
    fn set_position(&self, track: &ObjectPath<'_>, position_us: i64) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn metadata(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
    #[zbus(property)]
    fn position(&self) -> zbus::Result<i64>;
    #[zbus(property)]
    fn volume(&self) -> zbus::Result<f64>;
    #[zbus(property)]
    fn set_volume(&self, level: f64) -> zbus::Result<()>;
    #[zbus(property)]
    fn can_go_next(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn can_go_previous(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn can_seek(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn can_control(&self) -> zbus::Result<bool>;
}

pub struct MediaEffector {
    connection: zbus::Connection,
}

/// Read a string from a metadata value; `xesam:artist` is spec'd as an array
/// but plenty of players send a bare string.
fn as_text(value: Option<&OwnedValue>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    if let Ok(s) = <&str>::try_from(value) {
        return s.to_string();
    }
    if let Ok(list) = <Vec<String>>::try_from(value.clone()) {
        return list.join(", ");
    }
    String::new()
}

/// Read `mpris:trackid`, whichever of its two types it arrived as.
///
/// Spec says object path, but many players send the same text as a plain
/// string; the two D-Bus types don't convert to each other.
fn as_track_id(value: Option<&OwnedValue>) -> Option<ObjectPath<'static>> {
    let value = value?;
    if let Ok(path) = ObjectPath::try_from(value.clone()) {
        return Some(path.into_owned());
    }
    let text = <&str>::try_from(value).ok()?;
    ObjectPath::try_from(text.to_string()).ok()
}

/// Read a length, in microseconds, whatever integer width it arrived as.
fn as_micros(value: Option<&OwnedValue>) -> u64 {
    let Some(value) = value else { return 0 };
    if let Ok(n) = i64::try_from(value) {
        return u64::try_from(n).unwrap_or(0);
    }
    u64::try_from(value).unwrap_or(0)
}

/// The state everything else is measured against: nothing playing anywhere.
fn nothing() -> MediaState {
    MediaState::default()
}

impl MediaEffector {
    pub async fn new() -> anyhow::Result<Self> {
        Ok(Self {
            connection: zbus::Connection::session().await?,
        })
    }

    /// Bus names of every player, aggregators excluded.
    async fn names(&self) -> anyhow::Result<Vec<String>> {
        let dbus = zbus::fdo::DBusProxy::new(&self.connection).await?;
        let mut names: Vec<String> = dbus
            .list_names()
            .await?
            .into_iter()
            .map(|n| n.as_str().to_string())
            .filter(|n| n.starts_with(PREFIX))
            .filter(|n| n.strip_prefix(PREFIX) != Some(AGGREGATOR))
            .collect();
        // Stable order so an unchanged state compares equal.
        names.sort();
        Ok(names)
    }

    async fn read(&self, bus: &str) -> anyhow::Result<MediaPlayer> {
        let player = PlayerProxy::builder(&self.connection)
            .destination(bus.to_string())?
            .build()
            .await?;
        let app = MediaPlayer2Proxy::builder(&self.connection)
            .destination(bus.to_string())?
            .build()
            .await?;

        let id = bus.strip_prefix(PREFIX).unwrap_or(bus).to_string();
        let metadata = player.metadata().await.unwrap_or_default();

        // A player that answers nothing still gets an entry, so it's
        // distinguishable from having closed.
        Ok(MediaPlayer {
            name: app.identity().await.unwrap_or_else(|_| id.clone()),
            id,
            status: player
                .playback_status()
                .await
                .unwrap_or_else(|_| "stopped".to_string())
                .to_lowercase(),
            title: as_text(metadata.get("xesam:title")),
            artist: as_text(metadata.get("xesam:artist")),
            album: as_text(metadata.get("xesam:album")),
            length_ms: as_micros(metadata.get("mpris:length")) / 1000,
            position_ms: u64::try_from(player.position().await.unwrap_or(0)).unwrap_or(0) / 1000,
            volume_percent: player
                .volume()
                .await
                .ok()
                .map(|v| (v.clamp(0.0, 1.0) * 100.0).round() as u8),
            can_go_next: player.can_go_next().await.unwrap_or(false),
            can_go_previous: player.can_go_previous().await.unwrap_or(false),
            can_seek: player.can_seek().await.unwrap_or(false),
            can_control: player.can_control().await.unwrap_or(false),
        })
    }

    /// Every player, and which one a command with no name goes to.
    pub async fn state(&self) -> MediaState {
        let Ok(names) = self.names().await else {
            return nothing();
        };
        let mut players = Vec::new();
        for bus in names {
            match self.read(&bus).await {
                Ok(p) => players.push(p),
                // One bad or just-exited player must not break the rest of the reading.
                Err(e) => tracing::debug!(bus, error = %e, "skipping a player"),
            }
        }
        let active = pick_active(&players);
        MediaState {
            players,
            active,
            system_volume: crate::mixer::volume().await,
        }
    }

    /// Carry out a command, and hand back the reading from before it ran (for
    /// `control_and_settle` to compare against). `None` for the machine-volume path.
    pub async fn control(
        &self,
        player: &str,
        action: MediaAction,
    ) -> anyhow::Result<Option<MediaState>> {
        // No player named means machine volume, not a player's; works even
        // with nothing playing.
        if let (MediaAction::SetVolume { percent }, true) = (&action, player.is_empty()) {
            crate::mixer::set_volume(*percent).await?;
            return Ok(None);
        }
        let state = self.state().await;
        let target = if player.is_empty() {
            state.active.clone()
        } else {
            player.to_string()
        };
        if target.is_empty() {
            anyhow::bail!("nothing is playing");
        }
        let Some(found) = state.players.iter().find(|p| p.id == target) else {
            anyhow::bail!("no player called {target}");
        };
        // Refused rather than attempted: a call to an uncontrollable player is
        // silently ignored.
        if !found.can_control {
            anyhow::bail!("{} does not accept control", found.name);
        }

        let bus = format!("{PREFIX}{target}");
        let proxy = PlayerProxy::builder(&self.connection)
            .destination(bus)?
            .build()
            .await?;

        match action {
            MediaAction::Play => proxy.play().await?,
            MediaAction::Pause => proxy.pause().await?,
            MediaAction::PlayPause => proxy.play_pause().await?,
            MediaAction::Next => proxy.next().await?,
            MediaAction::Previous => proxy.previous().await?,
            MediaAction::Stop => proxy.stop().await?,
            MediaAction::Seek { offset_ms } => proxy.seek(offset_ms.saturating_mul(1000)).await?,
            MediaAction::SetPosition { ms } => {
                // Track id pins the seek to this track, not whatever plays next; required.
                let metadata = proxy.metadata().await.unwrap_or_default();
                let track = as_track_id(metadata.get("mpris:trackid"))
                    .ok_or_else(|| anyhow::anyhow!("{} reports no track id", found.name))?;
                let us = i64::try_from(ms.saturating_mul(1000)).unwrap_or(i64::MAX);
                if us == 0 {
                    // Chromium ignores `SetPosition(track, 0)` (nonzero values
                    // work); seek past the start instead, which MPRIS defines
                    // as clamping to zero.
                    let here = proxy.position().await.unwrap_or(0);
                    proxy.seek(-(here.saturating_add(1_000_000))).await?;
                } else {
                    proxy.set_position(&track, us).await?;
                }
            }
            MediaAction::SetVolume { percent } => {
                proxy.set_volume(f64::from(percent) / 100.0).await?;
                // Read back rather than trust the write: Chromium accepts
                // `Volume` writes (`CanControl: true`) but ignores them. Wait a
                // moment first since a real change applies asynchronously.
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                let landed = proxy
                    .volume()
                    .await
                    .map(|v| (v.clamp(0.0, 1.0) * 100.0).round() as u8)
                    .unwrap_or(percent);
                // Rounding/clamping is fine; only a value that didn't move at all is a failure.
                if landed.abs_diff(percent) > 5 {
                    anyhow::bail!(
                        "{} ignores volume changes; use the player's own controls",
                        found.name
                    );
                }
            }
        }
        Ok(Some(state))
    }

    /// Carry out a command, then re-read until the state reflects it (or the
    /// budget runs out) — an MPRIS call returns before the player has acted,
    /// so the first reading afterwards is often stale. Budget stays well
    /// under the phone's own wait, same reasoning as `LOCK_CONFIRM`.
    pub async fn control_and_settle(
        &self,
        player: &str,
        action: MediaAction,
    ) -> anyhow::Result<MediaState> {
        let before = self.control(player, action).await?;
        let deadline = std::time::Instant::now() + CONTROL_CONFIRM;
        loop {
            tokio::time::sleep(CONTROL_INTERVAL).await;
            let now = self.state().await;
            // Nothing to compare against: this is the answer.
            let Some(before) = before.as_ref() else {
                return Ok(now);
            };
            if landed(&action, player, before, &now) != Some(false) {
                return Ok(now);
            }
            if std::time::Instant::now() >= deadline {
                return Ok(now);
            }
        }
    }
}

/// Which player a nameless command targets: playing beats merely open.
fn pick_active(players: &[MediaPlayer]) -> String {
    let by = |want: &str| {
        players
            .iter()
            .find(|p| p.status == want && p.can_control)
            .or_else(|| players.iter().find(|p| p.status == want))
    };
    by("playing")
        .or_else(|| by("paused"))
        .or_else(|| players.first())
        .map(|p| p.id.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(id: &str, status: &str, can_control: bool) -> MediaPlayer {
        MediaPlayer {
            id: id.to_string(),
            status: status.to_string(),
            can_control,
            ..Default::default()
        }
    }

    #[test]
    fn something_playing_wins_over_something_merely_open() {
        let players = vec![
            player("vlc", "paused", true),
            player("spotify", "playing", true),
        ];
        assert_eq!(pick_active(&players), "spotify");
    }

    #[test]
    fn a_player_that_accepts_control_is_preferred_to_one_that_does_not() {
        // A browser tab that reports playing but refuses commands would
        // otherwise capture every button press.
        let players = vec![
            player("chromium", "playing", false),
            player("spotify", "playing", true),
        ];
        assert_eq!(pick_active(&players), "spotify");
    }

    #[test]
    fn a_paused_player_is_better_than_a_stopped_one() {
        let players = vec![
            player("mpv", "stopped", true),
            player("vlc", "paused", true),
        ];
        assert_eq!(pick_active(&players), "vlc");
    }

    #[test]
    fn nothing_running_names_nothing() {
        assert_eq!(pick_active(&[]), "");
    }

    #[test]
    fn a_stopped_player_is_still_named_rather_than_nothing() {
        // So the remote can offer play on something that is open.
        let players = vec![player("mpv", "stopped", true)];
        assert_eq!(pick_active(&players), "mpv");
    }
}

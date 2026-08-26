//! Media control through MPRIS.
//!
//! Every player worth controlling on Linux speaks `org.mpris.MediaPlayer2` on
//! the session bus, so this needs no per-application anything: a browser, a
//! music player and a video player all look the same from here.
//!
//! What it does not do is trust them. MPRIS is a specification a great many
//! programs implement partially, so every property here is read defensively —
//! a player that omits `Metadata`, reports `Position` as the wrong integer
//! width, or refuses `CanControl` gets reported as what it is rather than
//! breaking the reading of every other player on the machine.

use std::collections::HashMap;

use acrylius_core::plugins::media::{MediaPlayer, MediaState};
use acrylius_core::vocab::MediaAction;
use zbus::zvariant::{ObjectPath, OwnedValue};

/// The prefix every player's bus name carries.
const PREFIX: &str = "org.mpris.MediaPlayer2.";

/// A proxy that mirrors whichever player is active.
///
/// Skipped, because it duplicates a player that is already listed and a remote
/// showing the same track twice looks broken. Anyone running it is served by
/// the real entries beside it.
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

/// Read a string out of a metadata value, whatever shape the player chose.
///
/// `xesam:artist` is specified as an array and shipped as a bare string by
/// enough players that handling only the specified form would leave the artist
/// blank on a good few of them.
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
        // Stable order, so a state that has not changed compares equal and
        // nothing is broadcast for a reshuffle nobody can see.
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

        // A player that answers nothing still gets an entry. Its absence would
        // be indistinguishable from it having closed, and the remote would show
        // a gap rather than something it can at least name.
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
                // One player that has just exited, or is answering badly, must
                // not take the rest of the reading with it.
                Err(e) => tracing::debug!(bus, error = %e, "skipping a player"),
            }
        }
        let active = pick_active(&players);
        MediaState { players, active }
    }

    pub async fn control(&self, player: &str, action: MediaAction) -> anyhow::Result<()> {
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
        // Refused rather than attempted. A player that says it cannot be
        // controlled will ignore the call, and reporting success for something
        // that did nothing is the failure this whole project keeps running into.
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
                // SetPosition names the track it applies to, so a seek cannot
                // land on whatever started playing in the meantime. A player
                // that does not report a track id cannot be positioned at all.
                let metadata = proxy.metadata().await.unwrap_or_default();
                let track = metadata
                    .get("mpris:trackid")
                    .and_then(|v| <&str>::try_from(v).ok())
                    .and_then(|s| ObjectPath::try_from(s.to_string()).ok())
                    .ok_or_else(|| anyhow::anyhow!("{} reports no track id", found.name))?;
                let us = i64::try_from(ms.saturating_mul(1000)).unwrap_or(i64::MAX);
                proxy.set_position(&track, us).await?;
            }
            MediaAction::SetVolume { percent } => {
                proxy.set_volume(f64::from(percent) / 100.0).await?;
                // Read it back, because the write is not the answer. `Volume`
                // is a writable MPRIS property and a player is free to accept
                // the write and do nothing with it — Chromium does exactly
                // that, with `CanControl` reporting true — so trusting the call
                // means reporting a volume change that never happened and
                // leaving a slider to snap back with no explanation.
                //
                // A moment first: a player that does honour it applies the
                // change asynchronously, and reading immediately would call it
                // a refusal. Effects run off the pump, so this stalls nothing.
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                let landed = proxy
                    .volume()
                    .await
                    .map(|v| (v.clamp(0.0, 1.0) * 100.0).round() as u8)
                    .unwrap_or(percent);
                // A player may round or clamp, and that is not a failure. Only
                // a value that did not move towards the target is.
                if landed.abs_diff(percent) > 5 {
                    anyhow::bail!(
                        "{} ignores volume changes; use the player's own controls",
                        found.name
                    );
                }
            }
        }
        Ok(())
    }
}

/// Which player a command with no name goes to.
///
/// Something that is playing, in preference to something that is merely open.
/// A machine with a paused video and a playing album should answer the album,
/// because that is the one the person is listening to.
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
        // otherwise capture every button press on the phone.
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

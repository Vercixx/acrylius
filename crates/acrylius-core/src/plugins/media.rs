//! `org.acrylius.media/1`: control whatever is playing.
//!
//! The protocol half only. Which player a machine has, and what "the active
//! one" means there, is the host's business — on Linux that is MPRIS, and on
//! another host it would be something else entirely.
//!
//! Two things are decided here rather than in the host, because a second host
//! would have to make the same promises:
//!
//! * A command with no player named goes to the active one. A remote whose
//!   buttons stop working because a second player appeared is worse than one
//!   that occasionally guesses, and the guess is visible: `state` says which
//!   player is active, so a caller that cares can name one.
//! * `position` is reported, never counted. A phone that ticked a position
//!   forward on its own would drift, and would keep ticking after the media
//!   stopped somewhere it could not see.
//!
//! Album art is deliberately absent. MPRIS hands over a URL, usually to a file
//! on the machine the phone cannot read, and the image itself is far past what
//! an envelope should carry — it belongs on the bulk channel, once there is one.

use std::collections::BTreeMap;

use crate::plugin::{Cx, Plugin, PluginError, PluginManifest};
use crate::proto::envelope::Envelope;
use crate::proto::ids::DeviceId;
use crate::vocab::{Effect, EffectKind, EffectResult, EffectToken, MediaAction, UiEvent};

pub const CAP: &str = "org.acrylius.media/1";

/// How long a host may spend waiting for a command to show up in a reading
/// before it answers with whatever it has.
///
/// A player acts on an MPRIS call asynchronously, so the first reading after one
/// is routinely the state we started from. Short, because most players act in
/// well under a tenth of a second, and a player that has not moved by now was
/// probably never going to.
pub const CONTROL_CONFIRM_MS: u64 = 1_500;

/// How long a client waits for the answer to a media command.
///
/// The host's budget plus [`crate::plugin::REPLY_SLACK_MS`], for the reason
/// spelled out on [`crate::plugins::session::LOCK_REPLY_BUDGET_MS`].
pub const CONTROL_REPLY_BUDGET_MS: u64 = CONTROL_CONFIRM_MS + crate::plugin::REPLY_SLACK_MS;

// The same rule as the session budgets, refused at compile time. See
// `plugins::session` for the bug it exists to prevent.
const _: () = assert!(CONTROL_REPLY_BUDGET_MS > CONTROL_CONFIRM_MS);

/// The values [`MediaPlayer::status`] may take. A host lower-cases whatever its
/// own player vocabulary is down to one of these.
pub const PLAYING: &str = "playing";
/// See [`PLAYING`].
pub const PAUSED: &str = "paused";
/// See [`PLAYING`].
pub const STOPPED: &str = "stopped";

/// One player, as the host found it.
#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct MediaPlayer {
    /// Stable enough to name in a command. On Linux, the MPRIS bus suffix.
    #[n(0)]
    pub id: String,
    /// What to show a person: "Spotify", "Firefox".
    #[n(1)]
    pub name: String,
    /// `playing`, `paused` or `stopped`.
    #[n(2)]
    pub status: String,
    #[n(3)]
    pub title: String,
    #[n(4)]
    pub artist: String,
    #[n(5)]
    pub album: String,
    /// Milliseconds. Zero when the player does not say, common for streams.
    #[n(6)]
    pub length_ms: u64,
    /// Milliseconds, as read at the moment the state was taken. Never counted
    /// forward by a receiver: it would drift, and would keep counting after the
    /// media stopped somewhere the receiver cannot see.
    #[n(7)]
    pub position_ms: u64,
    /// 0 to 100, or absent when the player has no volume of its own.
    #[n(8)]
    pub volume_percent: Option<u8>,
    #[n(9)]
    pub can_go_next: bool,
    #[n(10)]
    pub can_go_previous: bool,
    #[n(11)]
    pub can_seek: bool,
    /// False for a player that only reports. Sending it commands is refused
    /// rather than attempted, so a dead button is visibly dead.
    #[n(12)]
    pub can_control: bool,
}

/// The host's answer to [`Effect::MediaQuery`], and the payload of `state`.
#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct MediaState {
    #[n(0)]
    pub players: Vec<MediaPlayer>,
    /// The id a command with no player named goes to. Empty when there is
    /// nothing playing anywhere.
    #[n(1)]
    pub active: String,
    /// The machine's own output volume, 0 to 100, or `None` where there is no
    /// mixer to ask.
    ///
    /// Separate from a player's, and not the same question. MPRIS gives every
    /// player a writable `Volume` that a great many of them ignore, so a remote
    /// that offered only that would have a slider working for some of what you
    /// play and silently not for the rest. This one always moves something.
    #[n(2)]
    pub system_volume: Option<u8>,
}

/// A command, from a peer or from a local UI.
#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct MediaCommand {
    /// Empty for "whichever is active".
    #[n(0)]
    pub player: String,
    /// Milliseconds for `seek` (may be negative) and `position`; 0 to 100 for
    /// `volume`. Ignored otherwise.
    #[n(1)]
    pub value: i64,
}

static MANIFEST: PluginManifest = PluginManifest {
    id: "org.acrylius.media",
    outgoing: &[CAP],
    incoming: &[CAP],
    requires: &[EffectKind::Media],
};

struct Pending {
    peer: DeviceId,
    request: u32,
    /// `state` for a query, `broadcast` for something nobody asked for.
    reply: &'static str,
}

#[derive(Default)]
pub struct MediaPlugin {
    pending: BTreeMap<EffectToken, Pending>,
    connected: Vec<DeviceId>,
    last: Option<MediaState>,
}

/// The verbs that carry no value.
fn simple_action(ty: &str) -> Option<MediaAction> {
    Some(match ty {
        "play" => MediaAction::Play,
        "pause" => MediaAction::Pause,
        "playpause" => MediaAction::PlayPause,
        "next" => MediaAction::Next,
        "previous" => MediaAction::Previous,
        "stop" => MediaAction::Stop,
        _ => return None,
    })
}

/// Whether a reading taken after `action` shows the player having acted on it.
///
/// Here rather than in either host for the same reason `safe_name` is in
/// `acrylius_proto`: both ends need this answer and they do not share a runtime.
/// A desktop waits on it before it answers a command, and a phone uses it to
/// decide whether the command it sent landed. A second implementation of "did it
/// land" is how one of them ends up reporting success for something that did
/// nothing — which is the failure this project keeps running into.
///
/// `player` is the id the command named, or empty for "whichever was active",
/// which is resolved against `before` because that is the reading the command
/// was aimed at.
///
/// `None` means a reading cannot answer the question, and a caller that gets it
/// must stop waiting rather than guess. A seek moves a position that also moves
/// on its own, so nothing in a later reading tells a seek that worked from one
/// that was ignored; a volume set is confirmed by the host that wrote it,
/// against the value it asked for, which is not something a reading shows.
#[must_use]
pub fn landed(
    action: &MediaAction,
    player: &str,
    before: &MediaState,
    now: &MediaState,
) -> Option<bool> {
    let target: &str = if player.is_empty() {
        &before.active
    } else {
        player
    };
    let find = |s: &MediaState| s.players.iter().find(|p| p.id == target).cloned();
    let was = find(before);
    // A player that has gone away since is not going to report anything. It has
    // certainly stopped; it has certainly not started.
    let Some(now) = find(now) else {
        return Some(matches!(action, MediaAction::Stop));
    };
    // What identifies a track, without the position, which moves on its own.
    let track = |p: &MediaPlayer| {
        (
            p.title.clone(),
            p.artist.clone(),
            p.album.clone(),
            p.length_ms,
        )
    };
    match action {
        MediaAction::Play => Some(now.status == PLAYING),
        MediaAction::Pause => Some(now.status == PAUSED),
        MediaAction::Stop => Some(now.status == STOPPED),
        // Nothing absolute to compare against: the answer is whichever way it
        // was pointing before.
        MediaAction::PlayPause => was.map(|w| w.status != now.status),
        MediaAction::Next | MediaAction::Previous => was.map(|w| track(&w) != track(&now)),
        MediaAction::Seek { .. }
        | MediaAction::SetPosition { .. }
        | MediaAction::SetVolume { .. } => None,
    }
}

/// Whether a new reading is worth telling anyone about.
///
/// Everything except where the track has got to. A playing track's position
/// changes with every reading, so comparing whole states would find a
/// difference every time and broadcast a message a second, forever, to every
/// connected device. What a listener actually needs to hear about is a track
/// change, a pause, a volume move, or a player coming and going.
///
/// A phone still gets a position: it is in the state it receives, and it asks
/// again when someone is looking at it.
fn worth_announcing(before: Option<&MediaState>, now: &MediaState) -> bool {
    let Some(before) = before else {
        return true;
    };
    if before.active != now.active
        || before.players.len() != now.players.len()
        || before.system_volume != now.system_volume
    {
        return true;
    }
    before.players.iter().zip(&now.players).any(|(a, b)| {
        MediaPlayer {
            position_ms: 0,
            ..a.clone()
        } != MediaPlayer {
            position_ms: 0,
            ..b.clone()
        }
    })
}

impl MediaPlugin {
    fn broadcast(&mut self, cx: &mut Cx, state: &MediaState) {
        let Ok(body) = minicbor::to_vec(state) else {
            return;
        };
        for peer in &self.connected {
            cx.send(peer, CAP, "state", body.clone());
        }
    }

    /// Turn a message into an effect, refusing what cannot be honoured.
    ///
    /// Free rather than private, because a client needs the same mapping to ask
    /// [`landed`] whether the verb it sent has taken effect, and a second copy of
    /// "what does `playpause` mean" is how the two ends come to disagree.
    pub fn action_for(ty: &str, cmd: &MediaCommand) -> Result<MediaAction, PluginError> {
        if let Some(a) = simple_action(ty) {
            return Ok(a);
        }
        match ty {
            // Relative, because that is what a skip button means and it needs
            // no agreement about where the track currently is.
            "seek" => Ok(MediaAction::Seek {
                offset_ms: cmd.value,
            }),
            "position" => {
                let ms = u64::try_from(cmd.value).map_err(|_| PluginError::NotAllowed)?;
                Ok(MediaAction::SetPosition { ms })
            }
            "volume" => {
                // Range-checked here rather than at the host: every host would
                // otherwise have to remember, and one that forgot would hand a
                // player something it may or may not check itself.
                if !(0..=100).contains(&cmd.value) {
                    return Err(PluginError::NotAllowed);
                }
                Ok(MediaAction::SetVolume {
                    percent: u8::try_from(cmd.value).map_err(|_| PluginError::NotAllowed)?,
                })
            }
            other => Err(PluginError::UnknownType(other.to_string())),
        }
    }
}

impl Plugin for MediaPlugin {
    fn manifest(&self) -> &'static PluginManifest {
        &MANIFEST
    }

    fn on_peer_connected(&mut self, cx: &mut Cx, peer: &DeviceId) {
        if !self.connected.contains(peer) {
            self.connected.push(peer.clone());
        }
        // Say what is playing without being asked. A remote that shows nothing
        // until you press something is a remote people assume is broken.
        if let Some(state) = self.last.clone()
            && let Ok(body) = minicbor::to_vec(&state)
        {
            cx.send(peer, CAP, "state", body);
        }
    }

    fn on_peer_disconnected(&mut self, _cx: &mut Cx, peer: &DeviceId) {
        self.connected.retain(|p| p != peer);
    }

    fn on_message(
        &mut self,
        cx: &mut Cx,
        peer: &DeviceId,
        env: &Envelope<'_>,
    ) -> Result<(), PluginError> {
        match env.ty {
            "query" => {
                let token = cx.effect(Effect::MediaQuery);
                self.pending.insert(
                    token,
                    Pending {
                        peer: peer.clone(),
                        request: env.id,
                        reply: "state",
                    },
                );
                Ok(())
            }
            // A device that only sends may receive these and ignore them.
            "state" => {
                cx.ui(UiEvent::Plugin {
                    peer: peer.clone(),
                    cap: CAP.to_string(),
                    ty: env.ty.to_string(),
                    body: env.body.to_vec(),
                });
                Ok(())
            }
            ty => {
                // An empty body is a bare verb: `next` needs no arguments, and
                // requiring an empty map for it would be ceremony.
                let cmd: MediaCommand = if env.body.is_empty() {
                    MediaCommand::default()
                } else {
                    minicbor::decode(env.body).map_err(|_| PluginError::BadBody)?
                };
                let action = Self::action_for(ty, &cmd)?;
                let token = cx.effect(Effect::MediaControl {
                    player: cmd.player,
                    action,
                });
                // Answered with the state afterwards rather than an
                // acknowledgement. What a caller wants to know is what happened
                // to the music, and reading it back is the only honest answer:
                // a player may ignore a command, or clamp a seek, or stop.
                self.pending.insert(
                    token,
                    Pending {
                        peer: peer.clone(),
                        request: env.id,
                        reply: "state",
                    },
                );
                Ok(())
            }
        }
    }

    fn on_local(
        &mut self,
        cx: &mut Cx,
        peer: &DeviceId,
        ty: &str,
        body: &[u8],
    ) -> Result<(), PluginError> {
        match ty {
            // The host noticed something change. The peer argument is ignored.
            "notify" => {
                let token = cx.effect(Effect::MediaQuery);
                self.pending.insert(
                    token,
                    Pending {
                        peer: peer.clone(),
                        request: 0,
                        reply: "broadcast",
                    },
                );
                Ok(())
            }
            "query" => {
                cx.send(peer, CAP, "query", Vec::new());
                Ok(())
            }
            ty => {
                // A local UI sends the same verbs a peer does, so the same
                // validation applies and there is one place it lives.
                let cmd: MediaCommand = if body.is_empty() {
                    MediaCommand::default()
                } else {
                    minicbor::decode(body).map_err(|_| PluginError::BadBody)?
                };
                Self::action_for(ty, &cmd)?;
                cx.send(peer, CAP, ty, body.to_vec());
                Ok(())
            }
        }
    }

    fn on_effect_result(&mut self, cx: &mut Cx, token: EffectToken, result: &EffectResult) {
        let Some(p) = self.pending.remove(&token) else {
            return;
        };
        match result {
            EffectResult::Ok(data) => {
                let Ok(state) = minicbor::decode::<MediaState>(data) else {
                    if p.reply != "broadcast" {
                        cx.send_error(&p.peer, CAP, p.request, "internal", "unreadable state");
                    }
                    return;
                };
                let changed = worth_announcing(self.last.as_ref(), &state);
                self.last = Some(state.clone());

                if p.reply == "broadcast" {
                    if changed {
                        self.broadcast(cx, &state);
                    }
                    return;
                }
                match minicbor::to_vec(&state) {
                    Ok(body) => cx.send_reply(&p.peer, CAP, "state", body, p.request),
                    Err(_) => cx.send_error(&p.peer, CAP, p.request, "internal", "encode failed"),
                }
            }
            EffectResult::Failed(detail) => {
                if p.reply != "broadcast" {
                    cx.send_error(&p.peer, CAP, p.request, "effect_failed", detail);
                }
            }
            EffectResult::Unsupported => {
                if p.reply != "broadcast" {
                    cx.send_error(
                        &p.peer,
                        CAP,
                        p.request,
                        "not_allowed",
                        "this device has nothing to control",
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::harness::{envelope, run};

    fn peer() -> DeviceId {
        DeviceId::of(&[3u8; 32])
    }

    fn state(status: &str) -> MediaState {
        MediaState {
            players: vec![MediaPlayer {
                id: "spotify".to_string(),
                name: "Spotify".to_string(),
                status: status.to_string(),
                title: "A Song".to_string(),
                can_control: true,
                ..Default::default()
            }],
            active: "spotify".to_string(),
            system_volume: Some(40),
        }
    }

    fn player(status: &str, position_ms: u64) -> MediaPlayer {
        MediaPlayer {
            id: "spotify".to_string(),
            name: "Spotify".to_string(),
            status: status.to_string(),
            title: "A Song".to_string(),
            position_ms,
            can_control: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_broadcast_skips_the_peer_that_left_and_reaches_the_one_that_stayed() {
        // See the twin of this in `plugins::session`. The third of three
        // identical untested `retain`s that mutation testing turned up.
        let mut p = MediaPlugin::default();
        let gone = peer();
        let stayed = DeviceId::of(&[9u8; 32]);
        run(0, |cx| p.on_peer_connected(cx, &gone));
        run(0, |cx| p.on_peer_connected(cx, &stayed));
        run(0, |cx| p.on_peer_disconnected(cx, &gone));

        let r = run(0, |cx| p.on_local(cx, &gone, "notify", b"").unwrap());
        let r2 = run(r.next_token, |cx| {
            p.on_effect_result(
                cx,
                r.token(),
                &EffectResult::Ok(minicbor::to_vec(state(PLAYING)).unwrap()),
            );
        });

        let told: Vec<&DeviceId> = r2
            .sends
            .iter()
            .filter(|s| s.ty == "state")
            .map(|s| &s.peer)
            .collect();
        assert_eq!(told, vec![&stayed]);
    }

    #[test]
    fn a_pause_has_landed_only_once_the_player_says_paused() {
        // The whole point: a reading taken before the player acted looks exactly
        // like a player that ignored the command, and answering the first one
        // with "done" is how a phone ends up showing a running timeline on a
        // track that is already paused.
        let before = state(PLAYING);
        assert_eq!(
            landed(&MediaAction::Pause, "", &before, &state(PLAYING)),
            Some(false),
            "still playing: the command has not landed yet"
        );
        assert_eq!(
            landed(&MediaAction::Pause, "", &before, &state(PAUSED)),
            Some(true)
        );
    }

    #[test]
    fn a_position_that_moved_on_its_own_is_not_a_command_landing() {
        // The bug this function exists to remove: comparing whole states makes a
        // playing track answer "landed" for any command at all, because its
        // position moves between any two readings.
        let before = MediaState {
            players: vec![player(PLAYING, 1_000)],
            ..state(PLAYING)
        };
        let later = MediaState {
            players: vec![player(PLAYING, 2_000)],
            ..state(PLAYING)
        };
        assert_ne!(before, later, "the states do differ, which is the trap");
        assert_eq!(
            landed(&MediaAction::Pause, "", &before, &later),
            Some(false),
            "a moving position must not be mistaken for a pause"
        );
    }

    #[test]
    fn play_and_stop_are_answered_from_the_status_the_player_reports() {
        // The other two absolute verbs, each needing both answers. A test that
        // only checks the success case passes just as happily when the
        // comparison has been inverted — which is how the pause arm ended up
        // being the only one of the three that was really pinned.
        let paused = state(PAUSED);
        assert_eq!(
            landed(&MediaAction::Play, "", &paused, &state(PAUSED)),
            Some(false)
        );
        assert_eq!(
            landed(&MediaAction::Play, "", &paused, &state(PLAYING)),
            Some(true)
        );

        let playing = state(PLAYING);
        assert_eq!(
            landed(&MediaAction::Stop, "", &playing, &state(PLAYING)),
            Some(false)
        );
        assert_eq!(
            landed(&MediaAction::Stop, "", &playing, &state(STOPPED)),
            Some(true)
        );
    }

    #[test]
    fn play_pause_is_answered_against_where_it_started() {
        let playing = state(PLAYING);
        let paused = state(PAUSED);
        assert_eq!(
            landed(&MediaAction::PlayPause, "", &playing, &paused),
            Some(true)
        );
        assert_eq!(
            landed(&MediaAction::PlayPause, "", &playing, &playing),
            Some(false)
        );
    }

    #[test]
    fn a_skip_has_landed_when_the_track_changed_and_not_when_it_only_moved() {
        let before = MediaState {
            players: vec![player(PLAYING, 1_000)],
            ..state(PLAYING)
        };
        let same_track_later = MediaState {
            players: vec![player(PLAYING, 9_000)],
            ..state(PLAYING)
        };
        assert_eq!(
            landed(&MediaAction::Next, "", &before, &same_track_later),
            Some(false),
            "the same song further along is not the next song"
        );

        let next_track = MediaState {
            players: vec![MediaPlayer {
                title: "Another Song".to_string(),
                ..player(PLAYING, 0)
            }],
            ..state(PLAYING)
        };
        assert_eq!(
            landed(&MediaAction::Next, "", &before, &next_track),
            Some(true)
        );
    }

    #[test]
    fn a_player_that_went_away_has_stopped_and_has_not_started() {
        let before = state(PLAYING);
        let gone = MediaState {
            players: vec![],
            active: String::new(),
            system_volume: Some(40),
        };
        assert_eq!(landed(&MediaAction::Stop, "", &before, &gone), Some(true));
        assert_eq!(landed(&MediaAction::Play, "", &before, &gone), Some(false));
    }

    #[test]
    fn what_a_reading_cannot_answer_is_not_guessed_at() {
        // A caller that gets `None` stops waiting. Returning `false` here would
        // make every seek wait out its deadline and then report a failure.
        let s = state(PLAYING);
        for action in [
            MediaAction::Seek { offset_ms: 30_000 },
            MediaAction::SetPosition { ms: 0 },
            MediaAction::SetVolume { percent: 50 },
        ] {
            assert_eq!(landed(&action, "", &s, &s), None, "{action:?}");
        }
    }

    #[test]
    fn a_named_player_is_answered_and_not_the_active_one() {
        // A command that names a player must be judged on that player, or a
        // second player happening to pause would answer for it.
        let before = MediaState {
            players: vec![
                MediaPlayer {
                    id: "vlc".to_string(),
                    ..player(PLAYING, 0)
                },
                player(PLAYING, 0),
            ],
            active: "spotify".to_string(),
            system_volume: None,
        };
        let vlc_paused = MediaState {
            players: vec![
                MediaPlayer {
                    id: "vlc".to_string(),
                    ..player(PAUSED, 0)
                },
                player(PLAYING, 0),
            ],
            ..before.clone()
        };
        assert_eq!(
            landed(&MediaAction::Pause, "vlc", &before, &vlc_paused),
            Some(true)
        );
        assert_eq!(
            landed(&MediaAction::Pause, "", &before, &vlc_paused),
            Some(false),
            "the active player is spotify, which is still playing"
        );
    }

    fn command(body: &MediaCommand) -> Vec<u8> {
        minicbor::to_vec(body).unwrap()
    }

    #[test]
    fn a_bare_verb_needs_no_body() {
        // `next` with an empty body is the common case, and requiring an empty
        // map for it would be ceremony a hand-written client would get wrong.
        let mut p = MediaPlugin::default();
        let r = run(0, |cx| {
            p.on_message(cx, &peer(), &envelope(1, CAP, "next", b""))
                .unwrap();
        });
        assert_eq!(r.effects.len(), 1);
        assert!(matches!(
            &r.effects[0].1,
            Effect::MediaControl { action: MediaAction::Next, player } if player.is_empty()
        ));
    }

    #[test]
    fn a_command_may_name_its_player() {
        let mut p = MediaPlugin::default();
        let body = command(&MediaCommand {
            player: "firefox".to_string(),
            value: 0,
        });
        let r = run(0, |cx| {
            p.on_message(cx, &peer(), &envelope(1, CAP, "pause", &body))
                .unwrap();
        });
        assert!(matches!(
            &r.effects[0].1,
            Effect::MediaControl { player, .. } if player == "firefox"
        ));
    }

    #[test]
    fn a_volume_outside_the_range_is_refused_here() {
        // Refused in the plugin so every host does not have to remember, and so
        // a host that forgot cannot hand a player something out of range.
        let mut p = MediaPlugin::default();
        for bad in [-1, 101] {
            let body = command(&MediaCommand {
                player: String::new(),
                value: bad,
            });
            let r = run(0, |cx| {
                assert_eq!(
                    p.on_message(cx, &peer(), &envelope(1, CAP, "volume", &body))
                        .unwrap_err(),
                    PluginError::NotAllowed,
                    "{bad} should be refused"
                );
            });
            assert!(r.effects.is_empty(), "nothing should reach a player");
        }
    }

    #[test]
    fn seek_is_relative_and_may_go_backwards() {
        let mut p = MediaPlugin::default();
        let body = command(&MediaCommand {
            player: String::new(),
            value: -10_000,
        });
        let r = run(0, |cx| {
            p.on_message(cx, &peer(), &envelope(1, CAP, "seek", &body))
                .unwrap();
        });
        assert!(matches!(
            &r.effects[0].1,
            Effect::MediaControl {
                action: MediaAction::Seek { offset_ms: -10_000 },
                ..
            }
        ));
    }

    #[test]
    fn an_unknown_verb_is_named_in_the_error() {
        let mut p = MediaPlugin::default();
        run(0, |cx| {
            assert_eq!(
                p.on_message(cx, &peer(), &envelope(1, CAP, "eject", b""))
                    .unwrap_err(),
                PluginError::UnknownType("eject".to_string())
            );
        });
    }

    #[test]
    fn a_command_is_answered_with_what_happened_to_the_music() {
        // Not an acknowledgement. A player may ignore a command, clamp a seek,
        // or stop of its own accord, and only reading it back afterwards says
        // which.
        let mut p = MediaPlugin::default();
        let r = run(0, |cx| {
            p.on_message(cx, &peer(), &envelope(7, CAP, "playpause", b""))
                .unwrap();
        });
        let token = r.effects[0].0;
        let answer = minicbor::to_vec(state("playing")).unwrap();
        let r = run(0, |cx| {
            p.on_effect_result(cx, token, &EffectResult::Ok(answer));
        });
        let sent = r.sent("state").expect("answered with state");
        assert_eq!(sent.re, Some(7));
        let got: MediaState = minicbor::decode(&sent.body).unwrap();
        assert_eq!(got.players[0].status, "playing");
    }

    #[test]
    fn a_peer_that_connects_is_told_what_is_playing() {
        let mut p = MediaPlugin {
            last: Some(state("playing")),
            ..Default::default()
        };
        let r = run(0, |cx| p.on_peer_connected(cx, &peer()));
        assert!(r.sent("state").is_some(), "without being asked");
    }

    #[test]
    fn nothing_is_broadcast_when_nothing_changed() {
        // A player reports its position, so a poll that broadcast every answer
        // would send a message a second forever.
        let mut p = MediaPlugin::default();
        run(0, |cx| p.on_peer_connected(cx, &peer()));

        let first = run(0, |cx| p.on_local(cx, &peer(), "notify", b"").unwrap());
        let token = first.effects[0].0;
        let answer = minicbor::to_vec(state("playing")).unwrap();
        let r = run(0, |cx| {
            p.on_effect_result(cx, token, &EffectResult::Ok(answer.clone()))
        });
        assert!(r.sent("state").is_some(), "the first answer is news");

        let second = run(0, |cx| p.on_local(cx, &peer(), "notify", b"").unwrap());
        let token = second.effects[0].0;
        let r = run(0, |cx| {
            p.on_effect_result(cx, token, &EffectResult::Ok(answer))
        });
        assert!(r.sent("state").is_none(), "the same answer is not");
    }

    #[test]
    fn a_track_playing_on_does_not_announce_itself_every_second() {
        // The case that matters, and the one identical states do not cover: a
        // playing track's position moves with every reading, so comparing whole
        // states would broadcast to every connected device once a second for as
        // long as anything is playing.
        let mut p = MediaPlugin::default();
        run(0, |cx| p.on_peer_connected(cx, &peer()));

        let mut announce = |players: Vec<MediaPlayer>| {
            let first = run(0, |cx| p.on_local(cx, &peer(), "notify", b"").unwrap());
            let token = first.effects[0].0;
            let body = minicbor::to_vec(MediaState {
                players,
                active: "spotify".to_string(),
                system_volume: None,
            })
            .unwrap();
            run(0, |cx| {
                p.on_effect_result(cx, token, &EffectResult::Ok(body))
            })
            .sent("state")
            .is_some()
        };

        assert!(
            announce(vec![player("playing", 1_000)]),
            "the first reading is news"
        );
        assert!(
            !announce(vec![player("playing", 2_000)]),
            "the track merely playing on is not"
        );
        assert!(
            !announce(vec![player("playing", 3_000)]),
            "and still is not, a reading later"
        );
        assert!(
            announce(vec![player("paused", 3_000)]),
            "but pausing is worth hearing about"
        );
    }

    #[test]
    fn a_device_with_nothing_to_control_says_so() {
        let mut p = MediaPlugin::default();
        let r = run(0, |cx| {
            p.on_message(cx, &peer(), &envelope(3, CAP, "next", b""))
                .unwrap();
        });
        let token = r.effects[0].0;
        let r = run(0, |cx| {
            p.on_effect_result(cx, token, &EffectResult::Unsupported);
        });
        assert!(r.sent("err").is_some());
    }
}

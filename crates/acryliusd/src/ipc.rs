//! What travels over the control socket.
//!
//! One definition, used by the daemon that answers and the CLI that asks. These
//! were two hand-maintained copies in the same crate — `control.rs` and
//! `bin/acryliusctl.rs` — which agreed only because nobody had changed one yet.
//!
//! Newline-delimited JSON. Not because the format matters, but because the
//! socket is `0600` with a uid check and the only thing on the far end is a
//! program shipped in this binary.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    Status,
    /// Wait for a device to ask to pair, and answer it from here.
    ///
    /// Arms nothing: any device may start a pairing handshake. This is the
    /// terminal's way of seeing the six digits and pressing a button, for a
    /// machine with no notification daemon or somebody on the end of an SSH
    /// connection.
    Pair,
    Approve,
    Deny,
    Devices,
    /// What is on this network and not paired with.
    Nearby,
    Revoke {
        device: String,
    },
    Connect {
        device: String,
        addr: Option<String>,
    },
    Ping {
        device: String,
    },
    /// Dial a machine and try to pair with it.
    PairWith {
        addr: String,
    },
    /// Ask a peer to lock, unlock, or describe its session.
    Session {
        device: String,
        action: String,
    },
    /// Read a peer's clipboard, or push ours to it.
    Clipboard {
        device: String,
        push: Option<String>,
    },
    Media {
        device: String,
        action: String,
        player: Option<String>,
        value: Option<i64>,
    },
    /// What a peer is willing to run.
    Commands {
        device: String,
    },
    /// Run one of them.
    Run {
        device: String,
        id: String,
    },
    /// Offer a file to a peer.
    Send {
        device: String,
        path: String,
    },
    /// Offers made to this machine that nobody has answered.
    Offers,
    /// Answer one.
    Answer {
        transfer: u64,
        accept: bool,
    },
    /// Ask a peer to wake a third machine.
    Wake {
        device: String,
        mac: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Status {
    pub name: String,
    pub device_id: String,
    pub fingerprint: String,
    pub port: u16,
    pub peers: usize,
    pub caps_in: Vec<String>,
    pub caps_out: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Device {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub fingerprint: String,
    pub reachable: bool,
}

/// A machine on this network that this one is not paired with.
///
/// No device id: that is derived from a static key, and nothing has exchanged
/// keys with this machine yet. A fingerprint is what an advertisement carries
/// and all there is to tell two rows apart by.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Nearby {
    pub fingerprint: String,
    pub name: String,
    /// Ready to hand to `pair with`.
    pub addr: String,
    pub transport: u16,
    /// Whether it says it is already busy pairing with somebody.
    pub pairing: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Response {
    Ok,
    Status(Status),
    /// A struct variant, not `Devices(Vec<Device>)`. serde's internally-tagged
    /// representation cannot encode a newtype variant wrapping a sequence, and
    /// it fails at serialisation time, so the wrapper form compiled fine and
    /// silently closed the connection with no reply.
    Devices {
        devices: Vec<Device>,
    },
    Nearby {
        nearby: Vec<Nearby>,
    },
    /// Anything the core wanted a human to see, forwarded verbatim.
    Event {
        text: String,
    },
    /// A peer's answer, decoded but not yet worded.
    Report {
        report: Report,
    },
    /// A device is waiting on a human. Sent instead of `Event` for the short
    /// authentication string, because it is the one event that needs an answer
    /// — the client can then put up its own prompt rather than print prose and
    /// leave the operator to work out what to run next.
    Confirm {
        name: String,
        fingerprint: String,
        sas: String,
    },
    Error {
        message: String,
    },
}

/// A peer's answer, as data.
///
/// The daemon used to word every reply itself and hand the CLI a finished
/// string, which is why there was no `--json` to add: the numbers had been
/// decoded and thrown away one process too early.
///
/// These deliberately mirror the plugin bodies rather than reusing them. The
/// wire types are `minicbor` and belong to the protocol; this is the CLI's
/// output schema, and a script parsing it should not break because a field
/// moved on the wire. That is a different contract, kept on purpose.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "of", rename_all = "kebab-case")]
pub enum Report {
    Refused {
        code: String,
        message: String,
    },
    Session {
        session_id: String,
        kind: String,
        locked: bool,
    },
    SessionChanged {
        session_id: String,
        was_locked: bool,
        locked: bool,
    },
    Clipboard {
        text: String,
    },
    Offer {
        ty: String,
        name: String,
        size: u64,
    },
    Transfer {
        transfer: u64,
        ok: bool,
        detail: String,
    },
    Media {
        active: String,
        players: Vec<Player>,
        system_volume: Option<u8>,
    },
    Commands {
        commands: Vec<Command>,
    },
    Exited {
        code: i32,
        truncated: bool,
    },
    /// Something this version has no shape for. Kept rather than dropped: a
    /// peer running a newer daemon should not make the CLI silent.
    Opaque {
        ty: String,
        bytes: usize,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Player {
    pub id: String,
    pub status: String,
    pub title: String,
    pub artist: String,
    pub position_ms: u64,
    pub length_ms: u64,
    pub volume_percent: Option<u8>,
    pub can_control: bool,
    /// Whether a command naming no player would go here.
    pub active: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Command {
    pub id: String,
    pub name: String,
}

impl Report {
    /// The same answer, worded.
    ///
    /// Here rather than in the daemon so that the table and `--json` are two
    /// renderings of one value, and cannot disagree about what a peer said.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Refused { code, message } => format!("refused: {message} ({code})"),
            Self::Session {
                session_id,
                kind,
                locked,
            } => format!(
                "session {session_id} ({kind}) is {}",
                if *locked { "locked" } else { "unlocked" }
            ),
            Self::SessionChanged {
                session_id,
                was_locked,
                locked,
            } => format!(
                "session {session_id} was {} and is now {}",
                if *was_locked { "locked" } else { "unlocked" },
                if *locked { "locked" } else { "unlocked" }
            ),
            Self::Clipboard { text } => text.clone(),
            Self::Offer { ty, name, size } => format!("{ty} offers {name} ({})", human(*size)),
            Self::Transfer {
                transfer,
                ok,
                detail,
            } => {
                let n = short_transfer(*transfer);
                if *ok {
                    format!("transfer {n} finished")
                } else if detail.is_empty() {
                    format!("transfer {n} was refused")
                } else {
                    format!("transfer {n} failed: {detail}")
                }
            }
            Self::Media {
                players,
                system_volume,
                ..
            } => {
                if players.is_empty() {
                    // The volume is still worth saying. It is a property of the
                    // machine and it can be turned down whether or not anything
                    // is playing through it.
                    return match system_volume {
                        Some(v) => format!("nothing is playing (output volume {v}%)"),
                        None => "nothing is playing".to_string(),
                    };
                }
                players
                    .iter()
                    .map(|p| {
                        // The active one is marked, because a command with no
                        // player named goes there and a person should be able
                        // to see which.
                        let mark = if p.active { "*" } else { " " };
                        let mut line = format!("{mark} {:<12} {:<8} {}", p.id, p.status, p.title);
                        if !p.artist.is_empty() {
                            line.push_str(&format!(" — {}", p.artist));
                        }
                        if p.length_ms > 0 {
                            line.push_str(&format!(
                                "  [{}/{}]",
                                clock(p.position_ms),
                                clock(p.length_ms)
                            ));
                        }
                        if let Some(v) = p.volume_percent {
                            line.push_str(&format!("  vol {v}%"));
                        }
                        if !p.can_control {
                            line.push_str("  (reports only)");
                        }
                        line
                    })
                    .chain(
                        // Last, and separate: this is the machine's, not a
                        // player's, and it is what a `volume` with no player
                        // named moves.
                        system_volume
                            .map(|v| format!("  {:<12} {:<8} output volume {v}%", "system", "")),
                    )
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Self::Commands { commands } => {
                if commands.is_empty() {
                    return "no commands offered".to_string();
                }
                commands
                    .iter()
                    .map(|c| format!("{}  {}", c.id, c.name))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Self::Exited { code, truncated } => format!(
                "exit {code}{}",
                if *truncated {
                    " (output truncated)"
                } else {
                    ""
                }
            ),
            Self::Opaque { ty, bytes } => format!("{ty} ({bytes} bytes)"),
        }
    }
}

/// The short form a person can retype, which is what an offer was listed
/// under. Ending a transfer under its full id would name it differently from
/// the number somebody accepted.
fn short_transfer(t: u64) -> u64 {
    acrylius_core::vocab::TransferId(t).short()
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut n = bytes as f64;
    let mut unit = 0;
    while n >= 1024.0 && unit < UNITS.len() - 1 {
        n /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{n:.1} {}", UNITS[unit])
    }
}

fn clock(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{}:{:02}", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media() -> Report {
        Report::Media {
            active: "chromium".to_string(),
            players: vec![
                Player {
                    id: "chromium".to_string(),
                    status: "playing".to_string(),
                    title: "Something".to_string(),
                    artist: "Somebody".to_string(),
                    position_ms: 61_000,
                    length_ms: 245_000,
                    volume_percent: Some(40),
                    can_control: true,
                    active: true,
                },
                Player {
                    id: "mpv".to_string(),
                    status: "paused".to_string(),
                    title: String::new(),
                    artist: String::new(),
                    position_ms: 0,
                    length_ms: 0,
                    volume_percent: None,
                    can_control: false,
                    active: false,
                },
            ],
            system_volume: Some(65),
        }
    }

    #[test]
    fn the_table_and_the_json_are_two_views_of_one_value() {
        // The point of the split. If these ever came from different places,
        // `--json` would be a second implementation of the answer and would
        // drift from what the table says.
        let r = media();
        let round: Report = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(r.render(), round.render());
    }

    #[test]
    fn the_active_player_is_marked_and_the_others_are_not() {
        let text = media().render();
        assert!(
            text.lines().next().unwrap().starts_with('*'),
            "a command naming no player goes to the active one, so it is marked"
        );
        assert!(
            text.lines().nth(1).unwrap().starts_with(' '),
            "and the others are not"
        );
        assert!(
            text.contains("[1:01/4:05]"),
            "position and length, as a clock"
        );
        assert!(
            text.contains("(reports only)"),
            "a player that refuses control says so rather than looking broken"
        );
        assert!(
            text.contains("output volume 65%"),
            "the machine's own volume is what a volume with no player moves"
        );
    }

    #[test]
    fn nothing_playing_still_reports_the_machines_volume() {
        // It is a property of the machine and can be turned down whether or not
        // anything is playing through it.
        let r = Report::Media {
            active: String::new(),
            players: Vec::new(),
            system_volume: Some(20),
        };
        assert_eq!(r.render(), "nothing is playing (output volume 20%)");
    }

    #[test]
    fn a_transfer_is_reported_under_the_number_it_was_offered_as() {
        // Ending it under the full id would name it differently from the
        // number somebody typed to accept it.
        let full = 1_u64 << 63 | 7;
        let Report::Transfer { transfer, .. } = (Report::Transfer {
            transfer: full,
            ok: true,
            detail: String::new(),
        }) else {
            unreachable!()
        };
        assert_eq!(transfer, full, "the full id is what travels");
        let text = Report::Transfer {
            transfer: full,
            ok: true,
            detail: String::new(),
        }
        .render();
        assert_eq!(text, format!("transfer {} finished", short_transfer(full)));
    }
}

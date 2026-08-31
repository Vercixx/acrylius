//! What travels over the control socket.
//!
//! Newline-delimited JSON, over a `0600` Unix socket with a uid check.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    Status,
    /// Wait for a device to ask to pair, and answer it from here. Arms
    /// nothing: any device may start a pairing handshake.
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

/// A pairing waiting on somebody at this machine.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Confirmation {
    pub name: String,
    pub fingerprint: String,
    pub sas: String,
}

/// A machine on this network that this one is not paired with.
///
/// No device id: that's derived from a key exchange, which hasn't happened yet.
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
    /// A struct variant, not `Devices(Vec<Device>)`: serde's internally-tagged
    /// enums can't encode a newtype wrapping a sequence and fail silently at
    /// serialization time.
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
    /// A device is waiting on a human. Sent instead of `Event` since this is
    /// the one event that needs an answer, so the client can put up its own prompt.
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
/// Deliberately mirrors the plugin bodies rather than reusing them: the wire
/// types are `minicbor` and belong to the protocol, while this is the CLI's
/// output schema, and a script parsing it should not break because a field
/// moved on the wire.
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
    /// Something this version has no shape for; kept rather than dropped so a
    /// newer peer doesn't go silent.
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
    /// The same answer, worded; kept here so the table and `--json` output
    /// can't disagree about what a peer said.
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
                    // Still worth reporting: it's a machine property, independent of playback.
                    return match system_volume {
                        Some(v) => format!("nothing is playing (output volume {v}%)"),
                        None => "nothing is playing".to_string(),
                    };
                }
                players
                    .iter()
                    .map(|p| {
                        // Mark the active player: a command naming none targets this one.
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
                        // Separate from players: this is the machine's own volume.
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

/// The short id an offer was listed under, so ending a transfer under it
/// matches the number somebody accepted.
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
        // Table and JSON must render identically from one value.
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
        let r = Report::Media {
            active: String::new(),
            players: Vec::new(),
            system_volume: Some(20),
        };
        assert_eq!(r.render(), "nothing is playing (output volume 20%)");
    }

    #[test]
    fn a_transfer_is_reported_under_the_number_it_was_offered_as() {
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

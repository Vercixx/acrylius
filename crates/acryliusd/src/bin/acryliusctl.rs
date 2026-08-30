//! The control CLI.
//!
//! Everything here goes over the `0600` Unix socket with a `SO_PEERCRED` uid
//! check. There is no network equivalent, and `pair` in particular has no route
//! from outside this machine, which is how "you must be at the PC" stays true
//! even if a future plugin is careless.

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use std::io::{IsTerminal, Write};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Parser, Debug)]
#[command(
    name = "acryliusctl",
    version,
    about = "Talk to the local acrylius daemon"
)]
struct Args {
    #[arg(long, env = "ACRYLIUS_STATE", global = true)]
    state: Option<PathBuf>,
    /// Print what came back as JSON instead of a table.
    ///
    /// The same value either way: the daemon answers with data, and the two
    /// renderings are the only difference.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Top,
}

// Grouped by the noun each verb is about, rather than nineteen commands in one
// flat list.
//
// Nothing was dropped to get here: the ones that looked redundant turned out to
// be the fallbacks for the cases the obvious path does not cover — `file accept`
// is what a desktop whose notifications have no buttons is told to run,
// `pair approve` is the only answer available when `pair` is not on a terminal,
// and `device connect --addr` is how you reach a machine on a network that
// filters mDNS. A shorter list would have been a list with holes in it.
//
// Every `device` positional carries `allow_hyphen_values`. A device id is
// strict base64url, whose alphabet includes '-', so roughly one id in
// sixty-four begins with one, and clap would read that as a flag and refuse the
// command. It is rare enough to look like a fluke in the field and is pinned by
// a test below.
#[derive(Subcommand, Debug)]
enum Top {
    /// Daemon identity, port and negotiated capabilities.
    Status,
    /// Open a pairing window, or answer one.
    Pair(Pair),
    /// The computers this machine knows.
    Device(Device),
    /// A peer's desktop session.
    Screen(Screen),
    /// A peer's clipboard.
    Clip(Clip),
    /// What is playing on a peer.
    Play(Play),
    /// The commands a peer offers.
    Cmd(CmdGroup),
    /// Files, in either direction.
    File(FileGroup),
    /// Ask a peer to wake a third machine by MAC.
    Wake {
        #[arg(allow_hyphen_values = true)]
        device: String,
        mac: String,
    },
}

#[derive(clap::Args, Debug)]
struct Pair {
    #[command(subcommand)]
    what: Option<PairCmd>,
    /// Accept without asking. For scripts; a human should compare the codes.
    #[arg(long)]
    yes: bool,
}

#[derive(Subcommand, Debug)]
enum PairCmd {
    /// Accept the device currently waiting, after checking the codes match.
    ///
    /// The answer for a `pair` that is not on a terminal — piped, or in a
    /// script — which has nobody to ask and says so rather than blocking on a
    /// read that will never return.
    Approve,
    /// Refuse it.
    Deny,
    /// Dial a device and try to pair with it.
    With {
        /// `host:port`.
        addr: String,
        /// Accept without asking. For scripts.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(clap::Args, Debug)]
struct Device {
    #[command(subcommand)]
    what: DeviceCmd,
}

#[derive(Subcommand, Debug)]
enum DeviceCmd {
    /// Paired devices.
    List,
    /// Machines on this network that are not paired with.
    ///
    /// The other half of `pair with`, which takes an address and had nothing
    /// anywhere that would tell you one.
    Nearby,
    /// Forget one. Its next connection is a stranger's.
    Forget {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    /// Open a session, and say why if it will not open.
    ///
    /// Dialling is automatic on every sighting, so this is not how a session
    /// normally starts. It is how you find out what is wrong with one that has
    /// not: an attempt somebody asked for is the only one that reports its
    /// failure out loud.
    Connect {
        #[arg(allow_hyphen_values = true)]
        device: String,
        /// Skip discovery and dial this `host:port` directly, for a network
        /// that filters mDNS.
        #[arg(long)]
        addr: Option<String>,
    },
    /// Round-trip a ping.
    Ping {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
}

#[derive(clap::Args, Debug)]
struct Screen {
    #[command(subcommand)]
    what: ScreenCmd,
}

#[derive(Subcommand, Debug)]
enum ScreenCmd {
    /// Is it locked?
    Query {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    Lock {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    Unlock {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
}

impl ScreenCmd {
    fn parts(self) -> (String, &'static str) {
        match self {
            Self::Query { device } => (device, "query"),
            Self::Lock { device } => (device, "lock"),
            Self::Unlock { device } => (device, "unlock"),
        }
    }
}

#[derive(clap::Args, Debug)]
struct Clip {
    #[command(subcommand)]
    what: ClipCmd,
}

#[derive(Subcommand, Debug)]
enum ClipCmd {
    /// Read a peer's clipboard.
    Get {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    /// Send it text.
    Put {
        #[arg(allow_hyphen_values = true)]
        device: String,
        text: String,
    },
}

/// Control what is playing on a peer.
///
/// A command naming no player goes to whichever one is active, which
/// `play status` marks with a `*`.
#[derive(clap::Args, Debug)]
struct Play {
    #[command(subcommand)]
    what: PlayCmd,
    /// Which player, by the id shown in `play status`.
    #[arg(long, global = true)]
    player: Option<String>,
}

#[derive(Subcommand, Debug)]
enum PlayCmd {
    /// What is playing, and on what.
    Status {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    Play {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    Pause {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    /// Whichever of the two it is not doing.
    Toggle {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    Next {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    Previous {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    Stop {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    /// Move by a number of milliseconds, which may be negative.
    Seek {
        #[arg(allow_hyphen_values = true)]
        device: String,
        #[arg(allow_hyphen_values = true)]
        offset_ms: i64,
    },
    /// Jump to a millisecond.
    Position {
        #[arg(allow_hyphen_values = true)]
        device: String,
        ms: i64,
    },
    /// Set the volume, 0 to 100.
    ///
    /// With no `--player` this moves the machine's own output, not a player's:
    /// most players accept a volume and ignore it while still reporting that
    /// they take control.
    Volume {
        #[arg(allow_hyphen_values = true)]
        device: String,
        percent: i64,
    },
}

impl PlayCmd {
    /// The device, the verb the daemon knows, and a value if the verb takes one.
    fn parts(self) -> (String, &'static str, Option<i64>) {
        match self {
            Self::Status { device } => (device, "query", None),
            Self::Play { device } => (device, "play", None),
            Self::Pause { device } => (device, "pause", None),
            Self::Toggle { device } => (device, "playpause", None),
            Self::Next { device } => (device, "next", None),
            Self::Previous { device } => (device, "previous", None),
            Self::Stop { device } => (device, "stop", None),
            Self::Seek { device, offset_ms } => (device, "seek", Some(offset_ms)),
            Self::Position { device, ms } => (device, "position", Some(ms)),
            Self::Volume { device, percent } => (device, "volume", Some(percent)),
        }
    }
}

#[derive(clap::Args, Debug)]
struct CmdGroup {
    #[command(subcommand)]
    what: CmdCmd,
}

#[derive(Subcommand, Debug)]
enum CmdCmd {
    /// What a peer is willing to run.
    List {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    /// Run one of them, by the id `cmd list` shows.
    Run {
        #[arg(allow_hyphen_values = true)]
        device: String,
        id: String,
    },
}

#[derive(clap::Args, Debug)]
struct FileGroup {
    #[command(subcommand)]
    what: FileCmd,
}

#[derive(Subcommand, Debug)]
enum FileCmd {
    /// Offer a file to a peer. It decides whether to take it.
    Send {
        #[arg(allow_hyphen_values = true)]
        device: String,
        path: String,
    },
    /// Files offered to this machine that nobody has answered yet.
    Offers,
    /// Take one. This is what a desktop with no notification buttons is told
    /// to run.
    Accept { transfer: u64 },
    /// Refuse one.
    Reject { transfer: u64 },
}

// The wire types are `acryliusd::ipc`, shared with the daemon that answers.
// They were a second hand-maintained copy in this file: same crate, same
// build, and nothing making them agree.
use acryliusd::ipc::{Request, Response};

/// Mirrors the daemon's rule: an explicitly named state directory keeps its
/// socket beside it, so two instances on one machine do not collide.
/// `println!` panics when its pipe closes, because Rust ignores SIGPIPE. A
/// tool that dies noisily when piped into `head` is not behaving like a Unix
/// tool; dropping the write error ends output quietly instead.
macro_rules! outln {
    () => {{ let _ = writeln!(std::io::stdout()); }};
    ($($arg:tt)*) => {{ let _ = writeln!(std::io::stdout(), $($arg)*); }};
}

fn socket_path(state: Option<PathBuf>) -> PathBuf {
    if let Some(s) = state {
        return s.join("acrylius.sock");
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("acrylius.sock")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let path = socket_path(args.state);
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("no daemon at {}; is acryliusd running?", path.display()))?;
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    let mut assume_yes = false;
    let (req, streaming) = match args.cmd {
        Top::Status => (Request::Status, false),
        Top::Pair(p) => match p.what {
            None => {
                assume_yes = p.yes;
                (Request::Pair, true)
            }
            Some(PairCmd::Approve) => (Request::Approve, false),
            Some(PairCmd::Deny) => (Request::Deny, false),
            Some(PairCmd::With { addr, yes }) => {
                assume_yes = yes;
                (Request::PairWith { addr }, true)
            }
        },
        Top::Device(d) => match d.what {
            DeviceCmd::List => (Request::Devices, false),
            DeviceCmd::Nearby => (Request::Nearby, false),
            DeviceCmd::Forget { device } => (Request::Revoke { device }, false),
            DeviceCmd::Connect { device, addr } => (Request::Connect { device, addr }, false),
            DeviceCmd::Ping { device } => (Request::Ping { device }, false),
        },
        Top::Screen(s) => {
            let (device, action) = s.what.parts();
            (
                Request::Session {
                    device,
                    action: action.to_string(),
                },
                false,
            )
        }
        Top::Clip(c) => match c.what {
            ClipCmd::Get { device } => (Request::Clipboard { device, push: None }, false),
            ClipCmd::Put { device, text } => (
                Request::Clipboard {
                    device,
                    push: Some(text),
                },
                false,
            ),
        },
        Top::Play(p) => {
            let (device, action, value) = p.what.parts();
            (
                Request::Media {
                    device,
                    action: action.to_string(),
                    player: p.player,
                    value,
                },
                false,
            )
        }
        Top::Cmd(c) => match c.what {
            CmdCmd::List { device } => (Request::Commands { device }, false),
            CmdCmd::Run { device, id } => (Request::Run { device, id }, false),
        },
        // Not streaming, any of them: a transfer reports once, when it is over.
        // Waiting for a second line means waiting for one the daemon has no
        // reason to send.
        Top::File(f) => match f.what {
            FileCmd::Send { device, path } => (
                Request::Send {
                    device,
                    // Resolved here, and it has to be here. The daemon has a
                    // working directory of its own — `/` under systemd — so a
                    // relative path sent verbatim is resolved against somewhere
                    // the person typing it has never been, and reports that
                    // their file does not exist.
                    path: absolute(&path)?,
                },
                false,
            ),
            FileCmd::Offers => (Request::Offers, false),
            FileCmd::Accept { transfer } => (
                Request::Answer {
                    transfer,
                    accept: true,
                },
                false,
            ),
            FileCmd::Reject { transfer } => (
                Request::Answer {
                    transfer,
                    accept: false,
                },
                false,
            ),
        },
        Top::Wake { device, mac } => (Request::Wake { device, mac }, false),
    };

    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    wr.write_all(line.as_bytes()).await?;

    let mut failed = false;
    while let Some(l) = lines.next_line().await? {
        let response: Response = serde_json::from_str(&l)?;
        // One value, two renderings. `--json` prints what the daemon actually
        // answered rather than a parse of the table below it, which is the
        // whole reason the daemon stopped wording things itself.
        if args.json {
            outln!("{}", serde_json::to_string(&response)?);
            if let Response::Error { .. } = response {
                failed = true;
            }
            // A pairing stream keeps going; everything else answers once.
            if streaming {
                continue;
            }
            break;
        }
        match response {
            Response::Ok => outln!("ok"),
            Response::Report { report } => outln!("{}", report.render()),
            Response::Status(s) => {
                outln!("{}  {}", s.name, s.device_id);
                outln!("  fingerprint  {}", s.fingerprint);
                outln!("  port         {}", s.port);
                outln!("  peers        {}", s.peers);
                outln!("  accepts      {}", join(&s.caps_in));
                outln!("  sends        {}", join(&s.caps_out));
            }
            Response::Devices { devices } if devices.is_empty() => {
                outln!("no paired devices");
            }
            Response::Devices { devices } => {
                for x in devices {
                    outln!(
                        "{}  {} ({})  {}",
                        x.device_id,
                        x.name,
                        x.platform,
                        if x.reachable {
                            "reachable"
                        } else {
                            "unreachable"
                        }
                    );
                    outln!("  fingerprint {}", x.fingerprint);
                }
            }
            Response::Nearby { nearby } if nearby.is_empty() => {
                outln!("nothing nearby that is not already paired");
            }
            Response::Nearby { nearby } => {
                for x in nearby {
                    outln!(
                        "{}  {}{}",
                        x.addr,
                        x.name,
                        if x.pairing { "  (busy pairing)" } else { "" }
                    );
                    outln!("  fingerprint {}", x.fingerprint);
                }
                outln!();
                outln!("Pair with one: acryliusctl pair with <address>");
            }
            Response::Event { text } => outln!("{text}"),
            Response::Confirm {
                name,
                fingerprint,
                sas,
            } => {
                outln!();
                outln!("{name} wants to pair.");
                outln!("  fingerprint  {fingerprint}");
                outln!();
                outln!("  It should be showing:  {sas}");
                outln!();
                let accept = if assume_yes {
                    outln!("  accepting (--yes)");
                    true
                } else if std::io::stdin().is_terminal() {
                    prompt("  Do the codes match? [Y/n] ").await?
                } else {
                    // Piped or backgrounded: there is nobody to ask. Say what to
                    // run rather than blocking forever on a read that will never
                    // return, which is what made this a two-terminal job.
                    outln!("  Not a terminal, so nothing to ask.");
                    outln!("  Run `acryliusctl pair approve` (or `pair deny`) to answer.");
                    continue;
                };
                // A second connection, because this one is busy streaming the
                // pairing. The daemon holds the window open until answered.
                answer(&path, accept).await?;
                if !accept {
                    failed = true;
                }
            }
            Response::Error { message } => {
                eprintln!("error: {message}");
                failed = true;
            }
        }
        if !streaming {
            break;
        }
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

/// Ask a yes/no question. Anything but an explicit "n" is a yes, because the
/// operator has already been shown both codes and the default is the case they
/// are in when the codes match.
async fn prompt(question: &str) -> anyhow::Result<bool> {
    use tokio::io::AsyncBufReadExt;
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "{question}");
    let _ = stdout.flush();
    let mut line = String::new();
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    reader.read_line(&mut line).await?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(!(answer.starts_with('n')))
}

/// Send the answer over a fresh connection.
async fn answer(path: &std::path::Path, accept: bool) -> anyhow::Result<()> {
    let stream = UnixStream::connect(path).await?;
    let (rd, mut wr) = stream.into_split();
    let req = if accept {
        Request::Approve
    } else {
        Request::Deny
    };
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    wr.write_all(line.as_bytes()).await?;
    // Read the acknowledgement so the daemon is not writing into a closed pipe.
    let mut lines = BufReader::new(rd).lines();
    let _ = lines.next_line().await?;
    Ok(())
}

/// Turn what someone typed into a path the daemon can open.
///
/// `~` first, because a shell only expands it unquoted and people quote paths
/// with spaces in them. Then made absolute against *this* process's working
/// directory, which is the only one that has anything to do with what they
/// meant. Symlinks are left alone: following them would send the target of a
/// link rather than the thing that was named.
fn absolute(path: &str) -> anyhow::Result<String> {
    let home = std::env::var_os("HOME").unwrap_or_default();
    Ok(resolve(path, home.as_ref(), &std::env::current_dir()?)
        .to_string_lossy()
        .into_owned())
}

/// The rule itself, asking the environment nothing, so a test can check it
/// without changing the environment out from under everything else.
fn resolve(path: &str, home: &std::ffi::OsStr, cwd: &std::path::Path) -> PathBuf {
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(home).join(rest),
        None => PathBuf::from(path),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

fn join(v: &[String]) -> String {
    if v.is_empty() {
        "(none)".to_string()
    } else {
        v.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn resolved(path: &str) -> PathBuf {
        resolve(
            path,
            std::ffi::OsStr::new("/home/someone"),
            std::path::Path::new("/home/someone/photos"),
        )
    }

    /// The daemon's working directory is `/` under systemd. A path relative to
    /// it is relative to nothing the person typing it can see, so it has to be
    /// resolved here, where the working directory is theirs.
    #[test]
    fn a_path_is_resolved_where_it_was_typed() {
        assert_eq!(
            resolved("holiday.jpg"),
            PathBuf::from("/home/someone/photos/holiday.jpg")
        );
    }

    #[test]
    fn an_absolute_path_is_left_alone() {
        assert_eq!(resolved("/etc/hostname"), PathBuf::from("/etc/hostname"));
    }

    /// A shell expands `~` only when it is unquoted, and a path with a space in
    /// it is usually quoted. Both are ordinary things to type.
    #[test]
    fn a_tilde_becomes_the_home_directory() {
        assert_eq!(
            resolved("~/Pictures/a b.png"),
            PathBuf::from("/home/someone/Pictures/a b.png")
        );
        // `~other` is another user's home, which no shell expands here either,
        // so it stays a literal rather than becoming a wrong guess.
        assert_eq!(
            resolved("~other/file"),
            PathBuf::from("/home/someone/photos/~other/file")
        );
    }

    /// A device id is strict base64url, and that alphabet contains `-`. Roughly
    /// one identity in sixty-four therefore produces an id starting with one,
    /// which clap reads as a flag unless the positional says otherwise.
    ///
    /// This was found by an M0 acceptance run that happened to generate
    /// `-4IEPRyU7ZslU335_83cWw`, and it had passed on every earlier run. Without
    /// this test it would go back to being a once-in-sixty-four mystery.
    const HYPHEN_ID: &str = "-4IEPRyU7ZslU335_83cWw";

    fn parse(args: &[&str]) -> Args {
        let mut argv = vec!["acryliusctl"];
        argv.extend_from_slice(args);
        Args::try_parse_from(argv).expect("a device id starting with '-' must parse")
    }

    /// The device this invocation names, whichever group it went through.
    ///
    /// One place, so the hyphen test below covers every device positional
    /// rather than the handful somebody remembered to list.
    fn device_of(a: Args) -> Option<String> {
        Some(match a.cmd {
            Top::Device(d) => match d.what {
                DeviceCmd::Forget { device }
                | DeviceCmd::Connect { device, .. }
                | DeviceCmd::Ping { device } => device,
                DeviceCmd::List | DeviceCmd::Nearby => return None,
            },
            Top::Screen(s) => s.what.parts().0,
            Top::Clip(c) => match c.what {
                ClipCmd::Get { device } | ClipCmd::Put { device, .. } => device,
            },
            Top::Play(p) => p.what.parts().0,
            Top::Cmd(c) => match c.what {
                CmdCmd::List { device } | CmdCmd::Run { device, .. } => device,
            },
            Top::File(f) => match f.what {
                FileCmd::Send { device, .. } => device,
                _ => return None,
            },
            Top::Wake { device, .. } => device,
            _ => return None,
        })
    }

    #[test]
    fn every_device_positional_accepts_a_leading_hyphen() {
        for argv in [
            vec!["device", "forget", HYPHEN_ID],
            vec!["device", "ping", HYPHEN_ID],
            vec!["device", "connect", HYPHEN_ID],
            vec!["screen", "query", HYPHEN_ID],
            vec!["screen", "lock", HYPHEN_ID],
            vec!["clip", "get", HYPHEN_ID],
            vec!["clip", "put", HYPHEN_ID, "hello"],
            vec!["play", "status", HYPHEN_ID],
            vec!["play", "volume", HYPHEN_ID, "40"],
            vec!["cmd", "list", HYPHEN_ID],
            vec!["cmd", "run", HYPHEN_ID, "screenshot"],
            vec!["file", "send", HYPHEN_ID, "/tmp/x"],
            vec!["wake", HYPHEN_ID, "00:11:22:33:44:55"],
        ] {
            assert_eq!(
                device_of(parse(&argv)).as_deref(),
                Some(HYPHEN_ID),
                "{argv:?} lost the device id"
            );
        }
    }

    #[test]
    fn a_negative_seek_is_an_offset_and_not_a_flag() {
        // `seek` takes a signed millisecond count, so the value itself looks
        // like a flag. Both positionals on this one need the relaxation.
        let a = parse(&["play", "seek", HYPHEN_ID, "-5000"]);
        let Top::Play(p) = a.cmd else {
            panic!("wrong subcommand")
        };
        assert_eq!(p.what.parts(), (HYPHEN_ID.to_string(), "seek", Some(-5000)));
    }

    #[test]
    fn options_still_parse_alongside_such_an_id() {
        // allow_hyphen_values must not swallow the flags that follow it.
        let a = parse(&["device", "connect", HYPHEN_ID, "--addr", "127.0.0.1:1971"]);
        let Top::Device(d) = a.cmd else {
            panic!("wrong subcommand")
        };
        let DeviceCmd::Connect { device, addr } = d.what else {
            panic!("wrong subcommand")
        };
        assert_eq!(device, HYPHEN_ID);
        assert_eq!(addr.as_deref(), Some("127.0.0.1:1971"));
    }

    #[test]
    fn a_genuinely_unknown_flag_is_still_refused() {
        // The relaxation is scoped to the positional; it must not turn the CLI
        // into one that silently accepts anything.
        assert!(
            Args::try_parse_from(["acryliusctl", "device", "ping", HYPHEN_ID, "--nonsense"])
                .is_err()
        );
    }
}

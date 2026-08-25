//! The control CLI.
//!
//! Everything here goes over the `0600` Unix socket with a `SO_PEERCRED` uid
//! check. There is no network equivalent, and `pair` in particular has no route
//! from outside this machine — which is how "you must be at the PC" stays true
//! even if a future plugin is careless.

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Parser, Debug)]
#[command(name = "acryliusctl", about = "Talk to the local acrylius daemon")]
struct Args {
    #[arg(long, env = "ACRYLIUS_STATE")]
    state: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    // Every `device` positional below carries `allow_hyphen_values`. A device
    // id is strict base64url, whose alphabet includes '-', so roughly one id in
    // sixty-four begins with one — and clap would read that as a flag and
    // refuse the command. It is rare enough to look like a fluke in the field
    // and is pinned by a test below.
    /// Daemon identity, port and negotiated capabilities.
    Status,
    /// Open a pairing window and wait. Prints a code to read out or scan.
    Pair {
        /// Use a specific code instead of a fresh random one.
        #[arg(long)]
        code: Option<String>,
    },
    /// Accept the device currently waiting, after checking the codes match.
    Approve,
    /// Refuse it.
    Deny,
    /// Paired devices.
    Devices,
    /// Forget a device. Its next connection is a stranger's.
    Revoke {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    /// Dial a device that has a pairing window open.
    PairWith {
        /// `host:port`.
        addr: String,
        /// The code shown by the other end.
        code: String,
    },
    /// Open a session to a paired device.
    Connect {
        #[arg(allow_hyphen_values = true)]
        device: String,
        /// Skip discovery and dial this `host:port` directly.
        #[arg(long)]
        addr: Option<String>,
    },
    /// Round-trip a ping.
    Ping {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    /// Ask a paired computer about its desktop session.
    Session {
        #[arg(allow_hyphen_values = true)]
        device: String,
        /// query, lock, or unlock.
        #[arg(default_value = "query")]
        action: String,
    },
    /// Read a peer's clipboard, or send it text.
    Clipboard {
        #[arg(allow_hyphen_values = true)]
        device: String,
        /// Text to push. Omit to read instead.
        #[arg(long)]
        push: Option<String>,
    },
    /// List what a peer is willing to run.
    Commands {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    /// Run one of a peer's configured commands.
    Run {
        #[arg(allow_hyphen_values = true)]
        device: String,
        id: String,
    },
    /// Ask a peer to wake a third machine by MAC.
    Wake {
        #[arg(allow_hyphen_values = true)]
        device: String,
        mac: String,
    },
}

#[derive(Serialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
enum Request {
    Status,
    Pair {
        code: Option<String>,
    },
    Approve,
    Deny,
    Devices,
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
    PairWith {
        addr: String,
        code: String,
    },
    Session {
        device: String,
        action: String,
    },
    Clipboard {
        device: String,
        push: Option<String>,
    },
    Commands {
        device: String,
    },
    Run {
        device: String,
        id: String,
    },
    Wake {
        device: String,
        mac: String,
    },
}

#[derive(Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum Response {
    Ok,
    Status(Status),
    Devices { devices: Vec<Device> },
    Event { text: String },
    Error { message: String },
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
struct Status {
    name: String,
    device_id: String,
    fingerprint: String,
    port: u16,
    peers: usize,
    caps_in: Vec<String>,
    caps_out: Vec<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
struct Device {
    device_id: String,
    name: String,
    platform: String,
    fingerprint: String,
    reachable: bool,
}

/// Mirrors the daemon's rule: an explicitly named state directory keeps its
/// socket beside it, so two instances on one machine do not collide.
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
        .with_context(|| format!("no daemon at {} — is acryliusd running?", path.display()))?;
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    let (req, streaming) = match args.cmd {
        Cmd::Status => (Request::Status, false),
        Cmd::Pair { code } => (Request::Pair { code }, true),
        Cmd::Approve => (Request::Approve, false),
        Cmd::Deny => (Request::Deny, false),
        Cmd::Devices => (Request::Devices, false),
        Cmd::Revoke { device } => (Request::Revoke { device }, false),
        Cmd::PairWith { addr, code } => (Request::PairWith { addr, code }, true),
        Cmd::Connect { device, addr } => (Request::Connect { device, addr }, false),
        Cmd::Ping { device } => (Request::Ping { device }, false),
        Cmd::Session { device, action } => (Request::Session { device, action }, false),
        Cmd::Clipboard { device, push } => (Request::Clipboard { device, push }, false),
        Cmd::Commands { device } => (Request::Commands { device }, false),
        Cmd::Run { device, id } => (Request::Run { device, id }, false),
        Cmd::Wake { device, mac } => (Request::Wake { device, mac }, false),
    };

    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    wr.write_all(line.as_bytes()).await?;

    let mut failed = false;
    while let Some(l) = lines.next_line().await? {
        match serde_json::from_str::<Response>(&l)? {
            Response::Ok => println!("ok"),
            Response::Status(s) => {
                println!("{}  {}", s.name, s.device_id);
                println!("  fingerprint  {}", s.fingerprint);
                println!("  port         {}", s.port);
                println!("  peers        {}", s.peers);
                println!("  accepts      {}", join(&s.caps_in));
                println!("  sends        {}", join(&s.caps_out));
            }
            Response::Devices { devices } if devices.is_empty() => {
                println!("no paired devices");
            }
            Response::Devices { devices } => {
                for x in devices {
                    println!(
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
                    println!("  fingerprint {}", x.fingerprint);
                }
            }
            Response::Event { text } => println!("{text}"),
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

    #[test]
    fn every_device_positional_accepts_a_leading_hyphen() {
        assert!(
            matches!(parse(&["revoke", HYPHEN_ID]).cmd, Cmd::Revoke { device } if device == HYPHEN_ID)
        );
        assert!(
            matches!(parse(&["ping", HYPHEN_ID]).cmd, Cmd::Ping { device } if device == HYPHEN_ID)
        );
        assert!(
            matches!(parse(&["connect", HYPHEN_ID]).cmd, Cmd::Connect { device, .. } if device == HYPHEN_ID)
        );
        assert!(
            matches!(parse(&["session", HYPHEN_ID]).cmd, Cmd::Session { device, .. } if device == HYPHEN_ID)
        );
        assert!(
            matches!(parse(&["clipboard", HYPHEN_ID]).cmd, Cmd::Clipboard { device, .. } if device == HYPHEN_ID)
        );
        assert!(
            matches!(parse(&["commands", HYPHEN_ID]).cmd, Cmd::Commands { device } if device == HYPHEN_ID)
        );
        assert!(
            matches!(parse(&["run", HYPHEN_ID, "screenshot"]).cmd, Cmd::Run { device, .. } if device == HYPHEN_ID)
        );
        assert!(
            matches!(parse(&["wake", HYPHEN_ID, "00:11:22:33:44:55"]).cmd, Cmd::Wake { device, .. } if device == HYPHEN_ID)
        );
    }

    #[test]
    fn options_still_parse_alongside_such_an_id() {
        // allow_hyphen_values must not swallow the flags that follow it.
        let a = parse(&["connect", HYPHEN_ID, "--addr", "127.0.0.1:1971"]);
        let Cmd::Connect { device, addr } = a.cmd else {
            panic!("wrong subcommand")
        };
        assert_eq!(device, HYPHEN_ID);
        assert_eq!(addr.as_deref(), Some("127.0.0.1:1971"));
    }

    #[test]
    fn a_genuinely_unknown_flag_is_still_refused() {
        // The relaxation is scoped to the positional; it must not turn the CLI
        // into one that silently accepts anything.
        assert!(Args::try_parse_from(["acryliusctl", "ping", HYPHEN_ID, "--nonsense"]).is_err());
    }
}

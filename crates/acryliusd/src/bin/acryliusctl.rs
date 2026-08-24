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
    Revoke { device: String },
    /// Dial a device that has a pairing window open.
    PairWith {
        /// `host:port`.
        addr: String,
        /// The code shown by the other end.
        code: String,
    },
    /// Open a session to a paired device.
    Connect {
        device: String,
        /// Skip discovery and dial this `host:port` directly.
        #[arg(long)]
        addr: Option<String>,
    },
    /// Round-trip a ping.
    Ping { device: String },
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

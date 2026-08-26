//! The control socket.
//!
//! This is where "you must be at the machine" lives, and it is deliberately a
//! property of the transport rather than a rule a handler could forget. The
//! socket is `0600` inside `$XDG_RUNTIME_DIR`, and every connection's peer
//! credentials are checked against our own uid before a single byte is read.
//!
//! `pair` has no network route at all. Not a protected one: none. That is
//! the single best structural idea carried over from `pc-helper-ios`: a future
//! plugin cannot accidentally expose pairing, because there is nothing to
//! expose it through.

use std::path::{Path, PathBuf};

use acrylius_core::proto::ids::DeviceId;
use acrylius_core::vocab::{Event, LocalCommand, UiEvent};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    Status,
    /// Open a pairing window. Reachable only here.
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
    /// Dial someone else's open pairing window.
    PairWith {
        addr: String,
        code: String,
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
    /// Anything the core wanted a human to see, forwarded verbatim.
    Event {
        text: String,
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

/// A live control socket. Unlinked on drop, but only if we are the instance
/// that bound it.
pub struct ControlSocket {
    path: PathBuf,
    bound: bool,
}

impl Drop for ControlSocket {
    fn drop(&mut self) {
        // Carried over from a real bug in the old project: an unconditional
        // unlink meant a second instance that failed to start would delete the
        // *live* instance's socket on its way out.
        if self.bound {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Where the control socket lives.
///
/// When a state directory is named explicitly, the socket goes beside it, which
/// is what lets two daemons run on one machine without fighting over a single
/// path in `$XDG_RUNTIME_DIR`. That is not just a test affordance: it is
/// also how you would run a second instance for a second user session.
#[must_use]
pub fn socket_path(state: &Path, explicit_state: bool) -> PathBuf {
    if explicit_state {
        return state.join("acrylius.sock");
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| state.to_path_buf())
        .join("acrylius.sock")
}

/// Render a UI event as a line a human can read.
fn render(e: &UiEvent) -> String {
    match e {
        UiEvent::PairingWindowOpen {
            code,
            expires_in_ms,
        } => {
            format!(
                "pairing window open, code {code}, for {}s",
                expires_in_ms / 1000
            )
        }
        UiEvent::PairingSas {
            name,
            fingerprint,
            sas,
        } => format!(
            "{name} wants to pair\n  fingerprint {fingerprint}\n  code on both screens: {sas}\n  run `acryliusctl approve` if they match"
        ),
        UiEvent::PairingComplete { peer, name } => format!("paired with {name} ({peer})"),
        UiEvent::PairingFailed { reason } => format!("pairing failed: {reason}"),
        UiEvent::PeerReachable { peer, name } => format!("{name} ({peer}) is reachable"),
        UiEvent::PeerUnreachable { peer } => format!("{peer} is unreachable"),
        UiEvent::Plugin { cap, ty, body, .. } => {
            format!("{cap} {ty} ({} bytes)", body.len())
        }
        UiEvent::Error { code, detail } => format!("error [{}]: {detail}", code.as_str()),
    }
}

pub struct Handles {
    pub transport: acrylius_core::link::TransportId,
    pub bulk: Option<std::sync::Arc<crate::files::FileBulk>>,
    pub events: mpsc::UnboundedSender<Event>,
    pub ui: broadcast::Sender<UiEvent>,
    pub status: std::sync::Arc<tokio::sync::Mutex<Option<Status>>>,
    pub devices: std::sync::Arc<tokio::sync::Mutex<Vec<Device>>>,
}

pub async fn serve(path: PathBuf, handles: Handles) -> anyhow::Result<ControlSocket> {
    // A stale socket from a crashed instance would otherwise make bind fail
    // forever. Only remove one nothing is listening on.
    if UnixStream::connect(&path).await.is_err() {
        let _ = std::fs::remove_file(&path);
    }
    let listener = UnixListener::bind(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::info!(path = %path.display(), "control socket");

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            // Mode 0600 already keeps others out, but check credentials too:
            // permissions can be changed, and a uid comparison cannot be.
            match stream.peer_cred() {
                Ok(cred) if cred.uid() == nix_getuid() => {}
                Ok(cred) => {
                    tracing::warn!(uid = cred.uid(), "refused a control connection");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "no peer credentials; refusing");
                    continue;
                }
            }
            let h = Handles {
                transport: handles.transport,
                bulk: handles.bulk.clone(),
                events: handles.events.clone(),
                ui: handles.ui.clone(),
                status: handles.status.clone(),
                devices: handles.devices.clone(),
            };
            tokio::spawn(async move {
                if let Err(e) = handle_conn(stream, h).await {
                    tracing::debug!(error = %e, "control connection ended");
                }
            });
        }
    });

    Ok(ControlSocket { path, bound: true })
}

fn nix_getuid() -> u32 {
    // Avoiding a `libc` dependency for one number.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|v| v.split_whitespace().next().map(str::to_string))
        })
        .and_then(|v| v.parse().ok())
        .unwrap_or(u32::MAX)
}

async fn handle_conn(stream: UnixStream, h: Handles) -> anyhow::Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                write(
                    &mut wr,
                    &Response::Error {
                        message: e.to_string(),
                    },
                )
                .await?;
                continue;
            }
        };

        match req {
            Request::Status => {
                let s = h.status.lock().await.clone();
                match s {
                    Some(s) => write(&mut wr, &Response::Status(s)).await?,
                    None => {
                        write(
                            &mut wr,
                            &Response::Error {
                                message: "starting".into(),
                            },
                        )
                        .await?;
                    }
                }
            }
            Request::Devices => {
                let d = h.devices.lock().await.clone();
                write(&mut wr, &Response::Devices { devices: d }).await?;
            }
            Request::Pair { code } => {
                let code = code.unwrap_or_else(random_code);
                let mut rx = h.ui.subscribe();
                h.events
                    .send(Event::Local(LocalCommand::OpenPairingWindow { code }))?;
                stream_pairing(&mut wr, &mut rx).await?;
            }
            Request::Approve => {
                h.events
                    .send(Event::Local(LocalCommand::ConfirmPairing { accept: true }))?;
                write(&mut wr, &Response::Ok).await?;
            }
            Request::Deny => {
                h.events
                    .send(Event::Local(LocalCommand::ConfirmPairing { accept: false }))?;
                write(&mut wr, &Response::Ok).await?;
            }
            Request::Revoke { device } => match DeviceId::parse(&device) {
                Ok(id) => {
                    h.events
                        .send(Event::Local(LocalCommand::Revoke { peer: id }))?;
                    write(&mut wr, &Response::Ok).await?;
                }
                Err(e) => {
                    write(
                        &mut wr,
                        &Response::Error {
                            message: e.to_string(),
                        },
                    )
                    .await?
                }
            },
            Request::PairWith { addr, code } => {
                let mut rx = h.ui.subscribe();
                h.events.send(Event::Local(LocalCommand::RequestPairing {
                    transport: h.transport,
                    addr,
                    code,
                }))?;
                stream_pairing(&mut wr, &mut rx).await?;
            }
            Request::Connect { device, addr } => match DeviceId::parse(&device) {
                Ok(id) => {
                    let mut rx = h.ui.subscribe();
                    if let Some(addr) = addr {
                        h.events.send(Event::Local(LocalCommand::SetPeerAddress {
                            peer: id.clone(),
                            transport: h.transport,
                            addr,
                        }))?;
                    }
                    h.events
                        .send(Event::Local(LocalCommand::Connect { peer: id }))?;
                    if let Ok(Ok(e)) =
                        tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await
                    {
                        write(&mut wr, &Response::Event { text: render(&e) }).await?;
                    } else {
                        write(
                            &mut wr,
                            &Response::Error {
                                message: "timed out".into(),
                            },
                        )
                        .await?;
                    }
                }
                Err(e) => {
                    write(
                        &mut wr,
                        &Response::Error {
                            message: e.to_string(),
                        },
                    )
                    .await?
                }
            },
            Request::Session { device, action } => {
                if !matches!(action.as_str(), "query" | "lock" | "unlock") {
                    write(
                        &mut wr,
                        &Response::Error {
                            message: format!("{action:?} is not query, lock or unlock"),
                        },
                    )
                    .await?;
                    continue;
                }
                plugin_request(
                    &mut wr,
                    &h,
                    &device,
                    Ask {
                        cap: acrylius_core::plugins::session::CAP,
                        ty: &action,
                        body: Vec::new(),
                        expect: &["state", "result", "err"],
                        patience: MACHINE,
                    },
                )
                .await?;
            }
            Request::Media {
                device,
                action,
                player,
                value,
            } => {
                let body = minicbor::to_vec(acrylius_core::plugins::media::MediaCommand {
                    player: player.unwrap_or_default(),
                    value: value.unwrap_or(0),
                })
                .unwrap_or_default();
                plugin_request(
                    &mut wr,
                    &h,
                    &device,
                    Ask {
                        cap: acrylius_core::plugins::media::CAP,
                        ty: &action,
                        body,
                        expect: &["state", "err"],
                        patience: MACHINE,
                    },
                )
                .await?;
            }
            Request::Clipboard { device, push } => {
                let (ty, body) = match &push {
                    Some(text) => ("push", text.as_bytes().to_vec()),
                    None => ("get", Vec::new()),
                };
                let expect: &[&str] = if push.is_some() { &[] } else { &["set", "err"] };
                plugin_request(
                    &mut wr,
                    &h,
                    &device,
                    Ask {
                        cap: acrylius_core::plugins::clipboard::CAP,
                        ty,
                        body,
                        expect,
                        patience: MACHINE,
                    },
                )
                .await?;
            }
            Request::Send { device, path } => {
                let Some(bulk) = &h.bulk else {
                    write(
                        &mut wr,
                        &Response::Error {
                            message: "this machine has no download directory, so it sends nothing"
                                .into(),
                        },
                    )
                    .await?;
                    continue;
                };
                let path = std::path::PathBuf::from(&path);
                // Refused, not resolved. This process's working directory is
                // `/` under systemd and has nothing to do with where the person
                // asking was standing, so quietly resolving against it reports
                // that a file they are looking at does not exist. Clients make
                // paths absolute themselves; `acryliusctl` does.
                if !path.is_absolute() {
                    write(
                        &mut wr,
                        &Response::Error {
                            message: format!(
                                "{} is a relative path, and this daemon's directory is not yours",
                                path.display()
                            ),
                        },
                    )
                    .await?;
                    continue;
                }
                let meta = match tokio::fs::metadata(&path).await {
                    Ok(m) if m.is_file() => m,
                    Ok(_) => {
                        write(
                            &mut wr,
                            &Response::Error {
                                message: format!("{} is not a file", path.display()),
                            },
                        )
                        .await?;
                        continue;
                    }
                    Err(e) => {
                        write(
                            &mut wr,
                            &Response::Error {
                                message: format!("{}: {e}", path.display()),
                            },
                        )
                        .await?;
                        continue;
                    }
                };
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "file".to_string());
                // The path stops here. What crosses the session is a name, a
                // size and an id, so a peer never learns where anything lives.
                let offer = bulk.offer(path, meta.len(), name, String::new());
                let body = minicbor::to_vec(&offer).unwrap_or_default();
                plugin_request(
                    &mut wr,
                    &h,
                    &device,
                    Ask {
                        cap: acrylius_core::plugins::share::CAP,
                        ty: "offer",
                        body,
                        expect: &["finished", "reject", "err"],
                        patience: PATIENT,
                    },
                )
                .await?;
            }

            Request::Offers => {
                let lines: Vec<String> = match &h.bulk {
                    Some(bulk) => bulk
                        .pending()
                        .into_iter()
                        .map(|o| format!("{:<6} {:<28} {}", o.transfer, o.name, human(o.size)))
                        .collect(),
                    None => Vec::new(),
                };
                let text = if lines.is_empty() {
                    "nothing offered".to_string()
                } else {
                    lines.join("\n")
                };
                write(&mut wr, &Response::Event { text }).await?;
            }

            Request::Answer { transfer, accept } => {
                let Some(bulk) = &h.bulk else {
                    write(
                        &mut wr,
                        &Response::Error {
                            message: "this machine receives no files".into(),
                        },
                    )
                    .await?;
                    continue;
                };
                let Some(peer) = bulk.peer_for(transfer) else {
                    write(
                        &mut wr,
                        &Response::Error {
                            message: format!("no offer numbered {transfer}"),
                        },
                    )
                    .await?;
                    continue;
                };
                let body = minicbor::to_vec(acrylius_core::plugins::share::Finished {
                    transfer,
                    ok: accept,
                    detail: String::new(),
                })
                .unwrap_or_default();
                let expect: &[&str] = if accept { &["finished", "err"] } else { &[] };
                if !accept {
                    // An accepted offer is dropped when the bytes stop moving.
                    // A refused one has no later moment, so it goes here.
                    bulk.forget(acrylius_core::vocab::TransferId(transfer));
                }
                plugin_request(
                    &mut wr,
                    &h,
                    &peer,
                    Ask {
                        cap: acrylius_core::plugins::share::CAP,
                        ty: if accept { "accept" } else { "reject" },
                        body,
                        expect,
                        patience: PATIENT,
                    },
                )
                .await?;
            }

            Request::Commands { device } => {
                // The catalog arrives unprompted when a peer connects, so this
                // reads what was already cached rather than asking again.
                plugin_request(
                    &mut wr,
                    &h,
                    &device,
                    Ask {
                        cap: acrylius_core::plugins::command::CAP,
                        ty: "list",
                        body: Vec::new(),
                        expect: &["list"],
                        patience: MACHINE,
                    },
                )
                .await?;
            }
            Request::Run { device, id } => {
                let body = minicbor::to_vec(acrylius_core::plugins::command::RunRequest { id })
                    .unwrap_or_default();
                plugin_request(
                    &mut wr,
                    &h,
                    &device,
                    Ask {
                        cap: acrylius_core::plugins::command::CAP,
                        ty: "run",
                        body,
                        expect: &["exited", "err"],
                        patience: MACHINE,
                    },
                )
                .await?;
            }
            Request::Wake { device, mac } => {
                let body = minicbor::to_vec(acrylius_core::plugins::wol::RelayRequest { mac })
                    .unwrap_or_default();
                plugin_request(
                    &mut wr,
                    &h,
                    &device,
                    Ask {
                        cap: acrylius_core::plugins::wol::CAP,
                        ty: "relay",
                        body,
                        expect: &["ok", "err"],
                        patience: MACHINE,
                    },
                )
                .await?;
            }
            Request::Ping { device } => match DeviceId::parse(&device) {
                Ok(id) => {
                    let mut rx = h.ui.subscribe();
                    h.events.send(Event::Local(LocalCommand::Plugin {
                        peer: id,
                        cap: acrylius_core::plugins::ping::CAP.to_string(),
                        ty: "ping".to_string(),
                        body: b"acryliusctl".to_vec(),
                    }))?;
                    let deadline = std::time::Duration::from_secs(5);
                    let got = tokio::time::timeout(deadline, async {
                        loop {
                            match rx.recv().await {
                                Ok(UiEvent::Plugin { ty, .. }) if ty == "pong" => return true,
                                Ok(UiEvent::PeerUnreachable { .. }) | Err(_) => return false,
                                Ok(_) => {}
                            }
                        }
                    })
                    .await;
                    match got {
                        Ok(true) => {
                            write(
                                &mut wr,
                                &Response::Event {
                                    text: "pong".into(),
                                },
                            )
                            .await?
                        }
                        _ => {
                            write(
                                &mut wr,
                                &Response::Error {
                                    message: "no pong".into(),
                                },
                            )
                            .await?;
                        }
                    }
                }
                Err(e) => {
                    write(
                        &mut wr,
                        &Response::Error {
                            message: e.to_string(),
                        },
                    )
                    .await?
                }
            },
        }
    }
    Ok(())
}

/// Send a plugin verb to a peer and report what comes back.
///
/// `expect` names the reply verbs worth waiting for. An empty list means the
/// verb is fire-and-forget, which is the case for pushing a clipboard value: it
/// is a broadcast to every peer, and there is nothing to answer it.
///
/// How long a machine gets to answer a machine.
const MACHINE: std::time::Duration = std::time::Duration::from_secs(15);
/// How long a file transfer gets. It waits on two things a clock cannot bound:
/// a person noticing the offer, and however long the bytes take. Giving up here
/// does not stop the transfer, it only stops reporting on it, so the generous
/// figure costs nothing and the short one lies.
const PATIENT: std::time::Duration = std::time::Duration::from_secs(3600);

/// What came back: the peer's reply, or a refusal from our own core.
enum Answer {
    Reply(String, Vec<u8>),
    Refused(String),
}

/// One question for a peer.
struct Ask<'a> {
    cap: &'a str,
    ty: &'a str,
    body: Vec<u8>,
    /// The reply verbs worth waiting for.
    expect: &'a [&'a str],
    /// How long the reply is worth waiting for. A lock either happens or does
    /// not, in seconds. A file offer waits on a person noticing it.
    patience: std::time::Duration,
}

async fn plugin_request(
    wr: &mut tokio::net::unix::OwnedWriteHalf,
    h: &Handles,
    device: &str,
    ask: Ask<'_>,
) -> anyhow::Result<()> {
    let Ask {
        cap,
        ty,
        body,
        expect,
        patience,
    } = ask;
    let id = match DeviceId::parse(device) {
        Ok(id) => id,
        Err(e) => {
            return write(
                wr,
                &Response::Error {
                    message: e.to_string(),
                },
            )
            .await;
        }
    };
    let mut rx = h.ui.subscribe();
    h.events.send(Event::Local(LocalCommand::Plugin {
        peer: id,
        cap: cap.to_string(),
        ty: ty.to_string(),
        body,
    }))?;
    if expect.is_empty() {
        return write(wr, &Response::Ok).await;
    }

    let waited = tokio::time::timeout(patience, async {
        loop {
            match rx.recv().await {
                Ok(UiEvent::Plugin {
                    cap: c,
                    ty: t,
                    body,
                    ..
                }) if c == cap && expect.contains(&t.as_str()) => {
                    return Some(Answer::Reply(t, body));
                }
                // A request the core refused before it left this machine — a
                // value out of range, a capability not negotiated. Nothing will
                // ever come back from the peer, so waiting for one turns an
                // immediate, well-explained refusal into a fifteen-second
                // timeout reported as if the peer were at fault.
                Ok(UiEvent::Error { code, detail }) => {
                    return Some(Answer::Refused(format!("{detail} ({})", code.as_str())));
                }
                Ok(UiEvent::PeerUnreachable { .. }) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    })
    .await;

    match waited {
        Ok(Some(Answer::Reply(ty, body))) => {
            write(
                wr,
                &Response::Event {
                    text: describe(cap, &ty, &body),
                },
            )
            .await
        }
        Ok(Some(Answer::Refused(message))) => write(wr, &Response::Error { message }).await,
        Ok(None) => {
            write(
                wr,
                &Response::Error {
                    message: "peer unreachable".into(),
                },
            )
            .await
        }
        Err(_) => {
            write(
                wr,
                &Response::Error {
                    message: "timed out".into(),
                },
            )
            .await
        }
    }
}

/// Render a reply body for a human.
///
/// The core keeps bodies opaque, which is the right call for routing and the
/// wrong one for a terminal, so decoding happens here at the edge.
fn describe(cap: &str, ty: &str, body: &[u8]) -> String {
    use acrylius_core::plugins::{clipboard, command, media, session};
    use acrylius_core::proto::envelope::ErrorBody;

    if ty == "err" {
        return match minicbor::decode::<ErrorBody>(body) {
            Ok(e) => format!("refused: {} ({})", e.message, e.code),
            Err(_) => "refused".to_string(),
        };
    }
    if cap == session::CAP {
        if let Ok(s) = minicbor::decode::<session::SessionState>(body) {
            return format!(
                "session {} ({}) is {}",
                s.session_id,
                s.kind,
                if s.locked { "locked" } else { "unlocked" }
            );
        }
        if let Ok(o) = minicbor::decode::<session::SessionOutcome>(body) {
            return format!(
                "session {} was {} and is now {}",
                o.session_id,
                if o.was_locked { "locked" } else { "unlocked" },
                if o.locked { "locked" } else { "unlocked" }
            );
        }
    }
    if cap == clipboard::CAP
        && let Ok(c) = minicbor::decode::<clipboard::ClipboardSet>(body)
    {
        return String::from_utf8_lossy(&c.data).into_owned();
    }
    if cap == acrylius_core::plugins::share::CAP {
        use acrylius_core::plugins::share::{Finished, Offer};
        if let Ok(o) = minicbor::decode::<Offer>(body) {
            return format!("{} offers {} ({})", ty, o.name, human(o.size));
        }
        if let Ok(f) = minicbor::decode::<Finished>(body) {
            return if f.ok {
                format!("transfer {} finished", f.transfer)
            } else if f.detail.is_empty() {
                format!("transfer {} was refused", f.transfer)
            } else {
                format!("transfer {} failed: {}", f.transfer, f.detail)
            };
        }
    }
    if cap == media::CAP
        && let Ok(s) = minicbor::decode::<media::MediaState>(body)
    {
        if s.players.is_empty() {
            // The volume is still worth saying. It is a property of the
            // machine and it can be turned down whether or not anything is
            // playing through it.
            return match s.system_volume {
                Some(v) => format!("nothing is playing (output volume {v}%)"),
                None => "nothing is playing".to_string(),
            };
        }
        return s
            .players
            .iter()
            .map(|p| {
                // The active one is marked, because a command with no player
                // named goes there and a person should be able to see which.
                let mark = if p.id == s.active { "*" } else { " " };
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
                // Last, and separate: this is the machine's, not a player's,
                // and it is what a `volume` with no --player moves.
                s.system_volume
                    .map(|v| format!("  {:<12} {:<8} output volume {v}%", "system", "")),
            )
            .collect::<Vec<_>>()
            .join("\n");
    }
    if cap == command::CAP {
        if let Ok(l) = minicbor::decode::<command::CommandList>(body) {
            if l.commands.is_empty() {
                return "no commands offered".to_string();
            }
            return l
                .commands
                .iter()
                .map(|c| format!("{}  {}", c.id, c.name))
                .collect::<Vec<_>>()
                .join("\n");
        }
        if let Ok(e) = minicbor::decode::<command::Exited>(body) {
            return format!(
                "exit {}{}",
                e.code,
                if e.truncated {
                    " (output truncated)"
                } else {
                    ""
                }
            );
        }
    }
    format!("{ty} ({} bytes)", body.len())
}

/// Relay pairing events until the window resolves one way or the other.
///
/// The short authentication string goes out as `Confirm` rather than as prose,
/// because it is the one event that needs an answer.
async fn stream_pairing(
    wr: &mut tokio::net::unix::OwnedWriteHalf,
    rx: &mut broadcast::Receiver<UiEvent>,
) -> anyhow::Result<()> {
    while let Ok(e) = rx.recv().await {
        let done = matches!(
            e,
            UiEvent::PairingComplete { .. } | UiEvent::PairingFailed { .. }
        );
        match &e {
            UiEvent::PairingSas {
                name,
                fingerprint,
                sas,
            } => {
                write(
                    wr,
                    &Response::Confirm {
                        name: name.clone(),
                        fingerprint: fingerprint.to_string(),
                        sas: sas.clone(),
                    },
                )
                .await?;
            }
            _ => write(wr, &Response::Event { text: render(&e) }).await?,
        }
        if done {
            break;
        }
    }
    Ok(())
}

async fn write(wr: &mut tokio::net::unix::OwnedWriteHalf, r: &Response) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(r)?;
    line.push('\n');
    wr.write_all(line.as_bytes()).await?;
    Ok(())
}

/// A fresh pairing code from the Crockford-minus-`ILOU` alphabet.
#[must_use]
/// A size a person reads rather than counts.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Milliseconds as m:ss, for a line a person reads.
fn clock(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{}:{:02}", secs / 60, secs % 60)
}

#[must_use]
pub fn random_code() -> String {
    use rand::Rng;
    let bits: u64 = rand::rng().random::<u64>() & 0xFF_FFFF_FFFF;
    acrylius_core::proto::pairing::encode(bits)
}

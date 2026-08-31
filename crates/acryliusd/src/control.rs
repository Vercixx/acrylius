//! The control socket: `0600` in `$XDG_RUNTIME_DIR`, peer credentials checked
//! against our own uid before a byte is read. Approving a pairing has no
//! network route — only `Approve` writes to the trust store, and it arrives
//! only through this socket.

use std::path::{Path, PathBuf};

use acrylius_core::proto::ids::DeviceId;
use acrylius_core::vocab::{Event, LocalCommand, UiEvent};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};

pub use acryliusd::ipc::{
    Command, Confirmation, Device, Nearby, Player, Report, Request, Response, Status,
};

/// A live control socket. Unlinked on drop, but only if we are the instance
/// that bound it.
pub struct ControlSocket {
    path: PathBuf,
    bound: bool,
}

impl Drop for ControlSocket {
    fn drop(&mut self) {
        // A second instance that failed to start must not delete the live one's socket.
        if self.bound {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// An explicitly named state directory keeps its socket beside it, so two
/// daemons can run on one machine.
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

/// What one event means to a request that is waiting.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// The answer, with the verb it came back under.
    Reply(String, Vec<u8>),
    /// It will never be answered, and here is why.
    Refused(String),
    /// The device this was aimed at has gone.
    Gone,
    /// Somebody else's business.
    Ignore,
}

/// Decide what an event means for a request aimed at `id`. A function rather
/// than match guards inside the wait loop, so the filter is testable.
fn verdict(e: UiEvent, id: &DeviceId, cap: &str, expect: &[&str]) -> Verdict {
    match e {
        UiEvent::Plugin {
            peer,
            cap: c,
            ty,
            body,
        } => {
            if &peer == id && c == cap && expect.contains(&ty.as_str()) {
                Verdict::Reply(ty, body)
            } else {
                Verdict::Ignore
            }
        }
        // A refusal from our own core: nothing will come back from the peer, so
        // don't wait. An error with no peer is about the machine, not this request.
        UiEvent::Error { peer, code, detail } => {
            if peer.as_ref() == Some(id) {
                Verdict::Refused(format!("{detail} ({})", code.as_str()))
            } else {
                Verdict::Ignore
            }
        }
        UiEvent::PeerUnreachable { peer } => {
            if &peer == id {
                Verdict::Gone
            } else {
                Verdict::Ignore
            }
        }
        _ => Verdict::Ignore,
    }
}

/// Whether an event is about a particular peer. Events with no peer are about
/// the machine and answer nobody's request.
fn about(e: &UiEvent, who: &DeviceId) -> bool {
    match e {
        UiEvent::PeerReachable { peer, .. }
        | UiEvent::PeerUnreachable { peer }
        | UiEvent::Plugin { peer, .. }
        | UiEvent::PairingComplete { peer, .. } => peer == who,
        UiEvent::Error { peer, .. } => peer.as_ref() == Some(who),
        // News about strangers; the waits here are all about paired devices.
        UiEvent::Discovered { .. }
        | UiEvent::Undiscovered { .. }
        | UiEvent::Revoked { .. }
        | UiEvent::PairingSas { .. }
        | UiEvent::PairingFailed { .. } => false,
    }
}

/// Render a UI event as a line a human can read.
fn render(e: &UiEvent) -> String {
    match e {
        UiEvent::PairingSas {
            name,
            fingerprint,
            sas,
        } => format!(
            "{name} wants to pair\n  fingerprint {fingerprint}\n  code on both screens: {sas}\n  run `acryliusctl pair approve` if they match"
        ),
        UiEvent::Discovered {
            name,
            addr,
            pairing,
            ..
        } => format!(
            "found {name} at {addr}{}",
            if *pairing { ", waiting to pair" } else { "" }
        ),
        UiEvent::Undiscovered { fingerprint } => format!("{fingerprint} has left the network"),
        UiEvent::PairingComplete { peer, name } => format!("paired with {name} ({peer})"),
        UiEvent::Revoked { peer } => format!("forgot {peer}"),
        UiEvent::PairingFailed { reason } => format!("pairing failed: {reason}"),
        UiEvent::PeerReachable { peer, name } => format!("{name} ({peer}) is reachable"),
        UiEvent::PeerUnreachable { peer } => format!("{peer} is unreachable"),
        UiEvent::Plugin { cap, ty, body, .. } => {
            format!("{cap} {ty} ({} bytes)", body.len())
        }
        UiEvent::Error { code, detail, .. } => format!("error [{}]: {detail}", code.as_str()),
    }
}

pub struct Handles {
    pub transport: acrylius_core::link::TransportId,
    pub bulk: Option<std::sync::Arc<crate::files::FileBulk>>,
    pub events: mpsc::UnboundedSender<Event>,
    pub ui: broadcast::Sender<UiEvent>,
    pub status: std::sync::Arc<tokio::sync::Mutex<Option<Status>>>,
    pub devices: std::sync::Arc<tokio::sync::Mutex<Vec<Device>>>,
    pub nearby: std::sync::Arc<tokio::sync::Mutex<Vec<Nearby>>>,
    pub pending_pair: std::sync::Arc<tokio::sync::Mutex<Option<Confirmation>>>,
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
                nearby: handles.nearby.clone(),
                pending_pair: handles.pending_pair.clone(),
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
            Request::Nearby => {
                let n = h.nearby.lock().await.clone();
                write(&mut wr, &Response::Nearby { nearby: n }).await?;
            }
            // Nothing to arm: any device may start a pairing handshake, so this
            // only subscribes and waits for one.
            Request::Pair => {
                let mut rx = h.ui.subscribe();
                // Replay a pairing already waiting; subscribing alone only
                // shows what happens next.
                if let Some(p) = h.pending_pair.lock().await.clone() {
                    write(
                        &mut wr,
                        &Response::Confirm {
                            name: p.name,
                            fingerprint: p.fingerprint,
                            sas: p.sas,
                        },
                    )
                    .await?;
                }
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
            Request::PairWith { addr } => {
                let mut rx = h.ui.subscribe();
                h.events.send(Event::Local(LocalCommand::RequestPairing {
                    transport: h.transport,
                    addr,
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
                        .send(Event::Local(LocalCommand::Connect { peer: id.clone() }))?;
                    // The first event about this peer, not the first event at all.
                    let waited = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                        loop {
                            match rx.recv().await {
                                Ok(e) if about(&e, &id) => return Some(e),
                                Err(_) => return None,
                                Ok(_) => {}
                            }
                        }
                    })
                    .await;
                    if let Ok(Some(e)) = waited {
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
                // Refused, not resolved: the daemon's working directory is `/`
                // under systemd. Clients make paths absolute themselves.
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
                // The path stops here: a peer sees a name, a size and an id,
                // never where anything lives.
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
                        .map(|o| {
                            format!(
                                "{:<6} {:<28} {}",
                                acrylius_core::vocab::TransferId(o.transfer).short(),
                                o.name,
                                human(o.size)
                            )
                        })
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
                // Resolve the short id a person was shown back to the real one, once.
                let Some(transfer) = bulk
                    .resolve(transfer)
                    .filter(|t| bulk.peer_for(*t).is_some())
                else {
                    write(
                        &mut wr,
                        &Response::Error {
                            message: format!("no offer numbered {transfer}"),
                        },
                    )
                    .await?;
                    continue;
                };
                let peer = bulk.peer_for(transfer).expect("just checked");
                let body = minicbor::to_vec(acrylius_core::plugins::share::Finished {
                    transfer: transfer.0,
                    ok: accept,
                    detail: String::new(),
                })
                .unwrap_or_default();
                let expect: &[&str] = if accept { &["finished", "err"] } else { &[] };
                if !accept {
                    // A refused offer has no later moment to be dropped.
                    bulk.forget(transfer);
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
                // The catalog arrives unprompted when a peer connects; this reads the cache.
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
                        peer: id.clone(),
                        cap: acrylius_core::plugins::ping::CAP.to_string(),
                        ty: "ping".to_string(),
                        body: b"acryliusctl".to_vec(),
                    }))?;
                    let deadline = std::time::Duration::from_secs(5);
                    // Filtered by peer: every pong is identical, so anybody's would convince.
                    let got = tokio::time::timeout(deadline, async {
                        loop {
                            match rx.recv().await {
                                Ok(UiEvent::Plugin { peer, ty, .. })
                                    if peer == id && ty == "pong" =>
                                {
                                    return true;
                                }
                                Ok(UiEvent::PeerUnreachable { peer }) if peer == id => {
                                    return false;
                                }
                                Err(_) => return false,
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

/// How long a machine gets to answer a machine.
const MACHINE: std::time::Duration = std::time::Duration::from_secs(15);
/// How long a file transfer gets: a person noticing the offer plus however
/// long the bytes take. Giving up only stops the reporting, not the transfer.
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
    /// The reply verbs worth waiting for; empty means fire-and-forget.
    expect: &'a [&'a str],
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
        peer: id.clone(),
        cap: cap.to_string(),
        ty: ty.to_string(),
        body,
    }))?;
    if expect.is_empty() {
        return write(wr, &Response::Ok).await;
    }

    // Filtered by peer: the event broadcast is global, so another device's
    // unsolicited push must not answer this request.
    let waited = tokio::time::timeout(patience, async {
        loop {
            let Ok(e) = rx.recv().await else { return None };
            match verdict(e, &id, cap, expect) {
                Verdict::Reply(ty, body) => return Some(Answer::Reply(ty, body)),
                Verdict::Refused(m) => return Some(Answer::Refused(m)),
                Verdict::Gone => return None,
                Verdict::Ignore => {}
            }
        }
    })
    .await;

    match waited {
        Ok(Some(Answer::Reply(ty, body))) => {
            write(
                wr,
                &Response::Report {
                    report: report(cap, &ty, &body),
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

/// Decode a reply body into data at the edge; the core keeps bodies opaque,
/// and wording is the CLI's job.
fn report(cap: &str, ty: &str, body: &[u8]) -> Report {
    use acrylius_core::plugins::{clipboard, command, media, session};
    use acrylius_core::proto::envelope::ErrorBody;

    if ty == "err" {
        return match minicbor::decode::<ErrorBody>(body) {
            Ok(e) => Report::Refused {
                code: e.code.to_string(),
                message: e.message.to_string(),
            },
            Err(_) => Report::Refused {
                code: "unknown".to_string(),
                message: "refused".to_string(),
            },
        };
    }
    if cap == session::CAP {
        if let Ok(s) = minicbor::decode::<session::SessionState>(body) {
            return Report::Session {
                session_id: s.session_id.to_string(),
                kind: s.kind.to_string(),
                locked: s.locked,
            };
        }
        if let Ok(o) = minicbor::decode::<session::SessionOutcome>(body) {
            return Report::SessionChanged {
                session_id: o.session_id.to_string(),
                was_locked: o.was_locked,
                locked: o.locked,
            };
        }
    }
    if cap == clipboard::CAP
        && let Ok(c) = minicbor::decode::<clipboard::ClipboardSet>(body)
    {
        return Report::Clipboard {
            text: String::from_utf8_lossy(&c.data).into_owned(),
        };
    }
    if cap == acrylius_core::plugins::share::CAP {
        use acrylius_core::plugins::share::{Finished, Offer};
        if let Ok(o) = minicbor::decode::<Offer>(body) {
            return Report::Offer {
                ty: ty.to_string(),
                name: o.name.to_string(),
                size: o.size,
            };
        }
        if let Ok(f) = minicbor::decode::<Finished>(body) {
            // The same number the offer was listed under.
            return Report::Transfer {
                transfer: f.transfer,
                ok: f.ok,
                detail: f.detail.to_string(),
            };
        }
    }
    if cap == media::CAP
        && let Ok(s) = minicbor::decode::<media::MediaState>(body)
    {
        return Report::Media {
            active: s.active.to_string(),
            players: s
                .players
                .iter()
                .map(|p| Player {
                    id: p.id.to_string(),
                    status: p.status.to_string(),
                    title: p.title.to_string(),
                    artist: p.artist.to_string(),
                    position_ms: p.position_ms,
                    length_ms: p.length_ms,
                    volume_percent: p.volume_percent,
                    can_control: p.can_control,
                    active: p.id == s.active,
                })
                .collect(),
            system_volume: s.system_volume,
        };
    }
    if cap == command::CAP {
        if let Ok(l) = minicbor::decode::<command::CommandList>(body) {
            return Report::Commands {
                commands: l
                    .commands
                    .iter()
                    .map(|c| Command {
                        id: c.id.to_string(),
                        name: c.name.to_string(),
                    })
                    .collect(),
            };
        }
        if let Ok(e) = minicbor::decode::<command::Exited>(body) {
            return Report::Exited {
                code: e.code,
                truncated: e.truncated,
            };
        }
    }
    Report::Opaque {
        ty: ty.to_string(),
        bytes: body.len(),
    }
}

/// Relay pairing events until the window resolves. The SAS goes out as
/// `Confirm` rather than prose because it is the one event that needs an answer.
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

#[cfg(test)]
mod tests {
    use super::*;
    use acrylius_core::proto::envelope::ErrorCode;

    /// 22 base64url chars carry 16 bytes, so the last char is constrained to
    /// `A`, `Q`, `g` or `w`; the variation goes at the front.
    fn who(first: char) -> DeviceId {
        let s: String = std::iter::once(first)
            .chain(std::iter::repeat_n('A', DeviceId::CHARS - 1))
            .collect();
        DeviceId::parse(&s).expect("a well-formed device id")
    }

    #[test]
    fn a_reply_from_another_peer_is_not_this_requests_answer() {
        let theirs = UiEvent::Plugin {
            peer: who('B'),
            cap: "org.acrylius.media/1".to_string(),
            ty: "state".to_string(),
            body: Vec::new(),
        };
        assert!(!about(&theirs, &who('A')));
        assert!(about(&theirs, &who('B')));
    }

    #[test]
    fn a_machine_leaving_the_network_reads_as_a_sentence() {
        let fp = acrylius_core::proto::ids::Fingerprint::of(&[7u8; 32]);
        let line = render(&UiEvent::Undiscovered {
            fingerprint: fp.clone(),
        });
        assert!(
            line.contains(fp.as_str()) && line.contains("left"),
            "a withdrawal has to name what left and say that it did: {line}"
        );
    }

    #[test]
    fn another_peer_going_away_does_not_end_this_request() {
        let gone = UiEvent::PeerUnreachable { peer: who('B') };
        assert!(
            !about(&gone, &who('A')),
            "B going away must not report A as unreachable"
        );
        assert!(about(&gone, &who('B')));
    }

    #[test]
    fn an_error_about_the_machine_is_nobodys_answer() {
        let machine = UiEvent::Error {
            peer: None,
            code: ErrorCode::Internal,
            detail: "a disk went away".to_string(),
        };
        assert!(!about(&machine, &who('A')));

        let theirs = UiEvent::Error {
            peer: Some(who('B')),
            code: ErrorCode::NotAllowed,
            detail: "no".to_string(),
        };
        assert!(!about(&theirs, &who('A')));
        assert!(about(&theirs, &who('B')));
    }

    fn reply_from(peer: DeviceId, cap: &str, ty: &str) -> UiEvent {
        UiEvent::Plugin {
            peer,
            cap: cap.to_string(),
            ty: ty.to_string(),
            body: b"body".to_vec(),
        }
    }

    const MEDIA: &str = "org.acrylius.media/1";

    #[test]
    fn a_reply_must_match_the_peer_the_cap_and_the_verb() {
        // Each conjunct gets a case that fails without it.
        let want = ["state"];
        assert_eq!(
            verdict(
                reply_from(who('A'), MEDIA, "state"),
                &who('A'),
                MEDIA,
                &want
            ),
            Verdict::Reply("state".to_string(), b"body".to_vec()),
        );
        assert_eq!(
            verdict(
                reply_from(who('B'), MEDIA, "state"),
                &who('A'),
                MEDIA,
                &want
            ),
            Verdict::Ignore,
            "the right answer from the wrong device is not this request's"
        );
        assert_eq!(
            verdict(
                reply_from(who('A'), "org.acrylius.session/1", "state"),
                &who('A'),
                MEDIA,
                &want
            ),
            Verdict::Ignore,
            "a session state is not a media state"
        );
        assert_eq!(
            verdict(
                reply_from(who('A'), MEDIA, "other"),
                &who('A'),
                MEDIA,
                &want
            ),
            Verdict::Ignore,
            "a verb nobody asked for is not an answer"
        );
    }

    #[test]
    fn only_this_peers_failures_end_this_request() {
        let mine = UiEvent::Error {
            peer: Some(who('A')),
            code: ErrorCode::NotAllowed,
            detail: "no".to_string(),
        };
        assert!(matches!(
            verdict(mine, &who('A'), MEDIA, &["state"]),
            Verdict::Refused(_)
        ));

        for other in [
            UiEvent::Error {
                peer: Some(who('B')),
                code: ErrorCode::NotAllowed,
                detail: "no".to_string(),
            },
            UiEvent::Error {
                peer: None,
                code: ErrorCode::Internal,
                detail: "a disk went away".to_string(),
            },
            UiEvent::PeerUnreachable { peer: who('B') },
        ] {
            assert_eq!(
                verdict(other, &who('A'), MEDIA, &["state"]),
                Verdict::Ignore,
                "somebody else's trouble must not answer this request"
            );
        }

        assert_eq!(
            verdict(
                UiEvent::PeerUnreachable { peer: who('A') },
                &who('A'),
                MEDIA,
                &["state"]
            ),
            Verdict::Gone
        );
    }

    #[test]
    fn pairing_events_answer_no_ones_request() {
        assert!(!about(
            &UiEvent::PairingSas {
                name: "someone".to_string(),
                fingerprint: acrylius_core::proto::ids::Fingerprint::of(&[0u8; 32]),
                sas: "605 480".to_string(),
            },
            &who('A')
        ));
    }
}

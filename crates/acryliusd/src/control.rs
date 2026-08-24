//! The control socket.
//!
//! This is where "you must be at the machine" lives, and it is deliberately a
//! property of the transport rather than a rule a handler could forget. The
//! socket is `0600` inside `$XDG_RUNTIME_DIR`, and every connection's peer
//! credentials are checked against our own uid before a single byte is read.
//!
//! `pair` has **no network route at all**. Not a protected one — none. That is
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
    /// it fails at *serialisation* time — so the wrapper form compiled fine and
    /// silently closed the connection with no reply.
    Devices {
        devices: Vec<Device>,
    },
    /// Anything the core wanted a human to see, forwarded verbatim.
    Event {
        text: String,
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
/// When a state directory is named explicitly, the socket goes beside it —
/// which is what lets two daemons run on one machine without fighting over a
/// single path in `$XDG_RUNTIME_DIR`. That is not just a test affordance: it is
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
                // Stream events until the window resolves, so the operator can
                // compare fingerprints before approving.
                while let Ok(e) = rx.recv().await {
                    let done = matches!(
                        e,
                        UiEvent::PairingComplete { .. } | UiEvent::PairingFailed { .. }
                    );
                    write(&mut wr, &Response::Event { text: render(&e) }).await?;
                    if done {
                        break;
                    }
                }
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
                while let Ok(e) = rx.recv().await {
                    let done = matches!(
                        e,
                        UiEvent::PairingComplete { .. } | UiEvent::PairingFailed { .. }
                    );
                    write(&mut wr, &Response::Event { text: render(&e) }).await?;
                    if done {
                        break;
                    }
                }
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

async fn write(wr: &mut tokio::net::unix::OwnedWriteHalf, r: &Response) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(r)?;
    line.push('\n');
    wr.write_all(line.as_bytes()).await?;
    Ok(())
}

/// A fresh pairing code from the Crockford-minus-`ILOU` alphabet.
#[must_use]
pub fn random_code() -> String {
    use rand::Rng;
    let bits: u64 = rand::rng().random::<u64>() & 0xFF_FFFF_FFFF;
    acrylius_core::proto::pairing::encode(bits)
}

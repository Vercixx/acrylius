//! USB transport: `iproxy` forwards a local port into one the phone listens
//! on, so unlike TCP this desktop dials out. Frames match `acrylius_rt::tcp`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use acrylius_core::link::{LinkAttrs, LinkDownReason, LinkId, TransportId, TransportKind};
use acrylius_core::vocab::Event;
use acrylius_rt::transport::{EventSink, Transport, TransportCmd};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// The port `iproxy` forwards, both sides of the tunnel.
pub const PORT: u16 = 1972;

/// Matches `acrylius_rt::tcp::MAX_FRAME`.
const MAX_FRAME: u32 = 1 << 20;

/// How long a dead peer may go unacknowledged before the socket is declared
/// broken. Same number TCP uses; USB's tunnel is just as capable of wedging.
const DEAD_PEER: std::time::Duration =
    std::time::Duration::from_millis(acrylius_core::link::DEAD_PEER_MS);

/// Backoff between `iproxy` restarts and reconnect attempts, so a phone that
/// is unplugged does not spin a tight loop.
const RETRY: std::time::Duration = std::time::Duration::from_millis(500);

pub struct UsbTransport {
    id: TransportId,
    next_link: AtomicU64,
}

impl UsbTransport {
    #[must_use]
    pub fn new(id: TransportId) -> Self {
        Self {
            id,
            next_link: AtomicU64::new(1),
        }
    }

    fn next_link(&self) -> LinkId {
        LinkId::new(self.id, self.next_link.fetch_add(1, Ordering::Relaxed))
    }

    fn attrs(&self) -> LinkAttrs {
        LinkAttrs {
            transport: self.id,
            kind: TransportKind::Custom("usb"),
            max_message: MAX_FRAME,
            reliable: true,
            ordered: true,
            latency: acrylius_core::link::LatencyClass::Loopback,
            bulk: acrylius_core::link::BulkSupport::None,
        }
    }
}

async fn read_frame(stream: &mut tokio::net::tcp::OwnedReadHalf) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await?;
    let n = u32::from_be_bytes(len);
    if n > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame of {n} bytes exceeds the {MAX_FRAME} cap"),
        ));
    }
    let mut buf = vec![0u8; n as usize];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

/// One held stream, driven until it closes or errors.
async fn serve(link: LinkId, stream: TcpStream, attrs: LinkAttrs, sink: EventSink) {
    let _ = stream.set_nodelay(true);
    let sock = socket2::SockRef::from(&stream);
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(DEAD_PEER / 2)
        .with_interval(DEAD_PEER / 4);
    let _ = sock.set_tcp_keepalive(&keepalive);
    #[cfg(target_os = "linux")]
    let _ = sock.set_tcp_user_timeout(Some(DEAD_PEER));

    let (mut rd, mut wr) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
    tracing::info!(?link, "USB link up");
    let _ = sink.send(Event::LinkUp {
        link,
        attrs,
        dial: None,
    });

    let mut writer = tokio::spawn(async move {
        while let Some(Some(msg)) = rx.recv().await {
            let Ok(n) = u32::try_from(msg.len()) else {
                break;
            };
            if wr.write_all(&n.to_be_bytes()).await.is_err() || wr.write_all(&msg).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    // Owned by this task, not shared: only one USB link exists at a time.
    LIVE_WRITER.lock().await.replace((link, tx));

    let reason = loop {
        tokio::select! {
            _ = &mut writer => break LinkDownReason::Closed,
            frame = read_frame(&mut rd) => match frame {
                Ok(msg) => {
                    if sink.send(Event::LinkRecv { link, msg }).is_err() {
                        break LinkDownReason::Closed;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    break LinkDownReason::Closed;
                }
                Err(e) => break LinkDownReason::Transport(e.to_string()),
            },
        }
    };

    LIVE_WRITER.lock().await.take();
    writer.abort();
    tracing::info!(?link, ?reason, "USB link down");
    let _ = sink.send(Event::LinkDown { link, reason });
}

type Writer = mpsc::UnboundedSender<Option<Vec<u8>>>;

/// The one outstanding USB link's sender, so `TransportCmd::Send`/`Close` can
/// reach it without a broadcast map — USB never holds more than one at a time.
static LIVE_WRITER: tokio::sync::Mutex<Option<(LinkId, Writer)>> =
    tokio::sync::Mutex::const_new(None);

/// Set for as long as a dial is in flight or a link is live. A repeat dial
/// while up would open a second tunnel and make the phone drop the first.
static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The UDID `iproxy` runs for, and its supervisor: only replaced when the
/// UDID changes, since a link dying is not evidence iproxy itself is bad.
static IPROXY: tokio::sync::Mutex<Option<(String, tokio::task::JoinHandle<()>)>> =
    tokio::sync::Mutex::const_new(None);

/// Start (or keep) the `iproxy` supervisor for this UDID.
async fn ensure_iproxy(udid: &str) {
    let mut guard = IPROXY.lock().await;
    if guard.as_ref().is_some_and(|(u, _)| u == udid) {
        return;
    }
    if let Some((_, old)) = guard.take() {
        old.abort();
    }
    *guard = Some((udid.to_string(), tokio::spawn(run_iproxy(udid.to_string()))));
}

/// Keeps `iproxy 1972:1972 -u <udid>` running, restarting it if it exits —
/// a cable reseat or a usbmuxd hiccup should heal without a daemon restart.
async fn run_iproxy(udid: String) {
    tracing::info!(%udid, port = PORT, "starting iproxy");
    loop {
        match tokio::process::Command::new("iproxy")
            .arg(format!("{PORT}:{PORT}"))
            .arg("-u")
            .arg(&udid)
            .kill_on_drop(true)
            .status()
            .await
        {
            Ok(status) => tracing::info!(%status, "iproxy exited; restarting"),
            Err(e) => {
                tracing::warn!(error = %e, "could not spawn iproxy; is libimobiledevice installed?");
                tokio::time::sleep(RETRY * 4).await;
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

#[async_trait::async_trait]
impl Transport for UsbTransport {
    fn id(&self) -> TransportId {
        self.id
    }

    async fn run(
        self: Arc<Self>,
        sink: EventSink,
        mut cmds: mpsc::UnboundedReceiver<TransportCmd>,
    ) -> anyhow::Result<()> {
        while let Some(cmd) = cmds.recv().await {
            match cmd {
                TransportCmd::Dial { dial, addr } => {
                    if BUSY.swap(true, Ordering::AcqRel) {
                        tracing::debug!(udid = %addr, "already up or dialling; ignoring a repeat dial");
                        continue;
                    }
                    let udid = addr;
                    tracing::info!(%udid, "USB dial requested");
                    let sink = sink.clone();
                    let me = self.clone();
                    tokio::spawn(async move {
                        ensure_iproxy(&udid).await;
                        let mut last_err = String::new();
                        for attempt in 0..20 {
                            match TcpStream::connect(("127.0.0.1", PORT)).await {
                                Ok(s) => {
                                    tracing::info!(attempt, "connected to iproxy's local port");
                                    let link = me.next_link();
                                    serve(link, s, me.attrs(), sink.clone()).await;
                                    BUSY.store(false, Ordering::Release);
                                    return;
                                }
                                Err(e) => {
                                    tracing::debug!(attempt, error = %e, "not up yet; retrying");
                                    last_err = e.to_string();
                                    tokio::time::sleep(RETRY).await;
                                }
                            }
                        }
                        tracing::warn!(error = %last_err, "giving up on the USB dial");
                        BUSY.store(false, Ordering::Release);
                        let _ = sink.send(Event::DialFailed {
                            dial,
                            reason: format!("could not reach iproxy: {last_err}"),
                        });
                    });
                }
                TransportCmd::Send { link, msg } => {
                    let guard = LIVE_WRITER.lock().await;
                    if let Some((held, tx)) = guard.as_ref()
                        && *held == link
                    {
                        let _ = tx.send(Some(msg));
                    }
                }
                TransportCmd::Close { link } => {
                    let mut guard = LIVE_WRITER.lock().await;
                    if guard.as_ref().is_some_and(|(held, _)| *held == link) {
                        let (_, tx) = guard.take().unwrap();
                        let _ = tx.send(None);
                    }
                }
                // A cable's presence is reported by the daemon's own attach
                // watcher, not by this transport; nothing to do here.
                TransportCmd::Advertise { .. } | TransportCmd::Discover { .. } => {}
            }
        }
        Ok(())
    }
}

//! TCP over a local network, with `mdns-sd` for discovery.
//!
//! Framing is `u32` big-endian length followed by that many bytes, capped at
//! 1 MiB. That cap is not decoration: without it a peer can name a length and
//! make us allocate it before a single byte of payload has arrived.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use acrylius_core::link::{LinkAttrs, LinkDownReason, LinkId, TransportId};
use acrylius_core::vocab::{DialToken, DiscoveredPeer, Event};
use acrylius_proto::ids::Fingerprint;
use acrylius_proto::{DEFAULT_PORT, SERVICE_TYPE};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::transport::{EventSink, Transport, TransportCmd};

/// The largest frame we will read. A peer that announces more is hung up on
/// rather than believed.
pub const MAX_FRAME: u32 = 1 << 20;

pub struct TcpTransport {
    id: TransportId,
    port: u16,
    /// Advertised so a peer can match this instance to a record it already has.
    fingerprint: Fingerprint,
    name: String,
    next_link: AtomicU64,
}

impl TcpTransport {
    #[must_use]
    pub fn new(id: TransportId, port: u16, fingerprint: Fingerprint, name: String) -> Self {
        Self {
            id,
            port,
            fingerprint,
            name,
            next_link: AtomicU64::new(1),
        }
    }

    fn next_link(&self) -> LinkId {
        LinkId(self.next_link.fetch_add(1, Ordering::Relaxed))
    }
}

/// Per-connection outbound channel, owned by the writer task.
type Writers = Arc<tokio::sync::Mutex<HashMap<LinkId, mpsc::UnboundedSender<Option<Vec<u8>>>>>>;

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

/// Pump one accepted or dialled connection until it ends.
async fn serve(
    link: LinkId,
    stream: TcpStream,
    attrs: LinkAttrs,
    dial: Option<DialToken>,
    sink: EventSink,
    writers: Writers,
) {
    let _ = stream.set_nodelay(true);
    let (mut rd, mut wr) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
    writers.lock().await.insert(link, tx);

    let _ = sink.send(Event::LinkUp { link, attrs, dial });

    let writer = tokio::spawn(async move {
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

    let reason = loop {
        match read_frame(&mut rd).await {
            Ok(msg) => {
                if sink.send(Event::LinkRecv { link, msg }).is_err() {
                    break LinkDownReason::Closed;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break LinkDownReason::Closed;
            }
            Err(e) => break LinkDownReason::Transport(e.to_string()),
        }
    };

    writers.lock().await.remove(&link);
    writer.abort();
    let _ = sink.send(Event::LinkDown { link, reason });
}

#[async_trait::async_trait]
impl Transport for TcpTransport {
    fn id(&self) -> TransportId {
        self.id
    }

    async fn run(
        self: Arc<Self>,
        sink: EventSink,
        mut cmds: mpsc::UnboundedReceiver<TransportCmd>,
    ) -> anyhow::Result<()> {
        let listener = TcpListener::bind(("0.0.0.0", self.port)).await?;
        let bound = listener.local_addr()?;
        tracing::info!(%bound, "listening");

        let writers: Writers = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let mdns = mdns_sd::ServiceDaemon::new()?;
        let mut advertised: Option<String> = None;
        let mut browsing = false;

        let accept_sink = sink.clone();
        let accept_writers = writers.clone();
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        tracing::debug!(%peer, "inbound connection");
                        let link = me.next_link();
                        tokio::spawn(serve(
                            link,
                            stream,
                            LinkAttrs::tcp_lan(me.id),
                            None,
                            accept_sink.clone(),
                            accept_writers.clone(),
                        ));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                        return;
                    }
                }
            }
        });

        while let Some(cmd) = cmds.recv().await {
            match cmd {
                TransportCmd::Dial { dial, addr } => {
                    let sink = sink.clone();
                    let writers = writers.clone();
                    let me = self.clone();
                    tokio::spawn(async move {
                        match tokio::net::lookup_host(&addr).await.map(|mut a| a.next()) {
                            Ok(Some(sa)) => match TcpStream::connect(sa).await {
                                Ok(s) => {
                                    let link = me.next_link();
                                    serve(
                                        link,
                                        s,
                                        LinkAttrs::tcp_lan(me.id),
                                        Some(dial),
                                        sink,
                                        writers,
                                    )
                                    .await;
                                }
                                Err(e) => {
                                    let _ = sink.send(Event::DialFailed {
                                        dial,
                                        reason: e.to_string(),
                                    });
                                }
                            },
                            _ => {
                                let _ = sink.send(Event::DialFailed {
                                    dial,
                                    reason: format!("could not resolve {addr}"),
                                });
                            }
                        }
                    });
                }
                TransportCmd::Send { link, msg } => {
                    if let Some(tx) = writers.lock().await.get(&link) {
                        let _ = tx.send(Some(msg));
                    }
                }
                TransportCmd::Close { link } => {
                    if let Some(tx) = writers.lock().await.remove(&link) {
                        let _ = tx.send(None);
                    }
                }
                TransportCmd::Advertise { enable, txt } => {
                    if let Some(name) = advertised.take() {
                        let _ = mdns.unregister(&name);
                    }
                    if enable {
                        let host = format!("{}.local.", hostname());
                        let mut props: HashMap<String, String> = txt.into_iter().collect();
                        // The display name belongs to the transport's
                        // advertisement, not to the core's TXT list: it is a
                        // discovery hint, and nothing may decide anything from
                        // it. Identity comes from the handshake.
                        props
                            .entry("n".to_string())
                            .or_insert_with(|| self.name.clone());
                        let instance = self.fingerprint.as_str()[..8].to_string();
                        match mdns_sd::ServiceInfo::new(
                            &format!("{SERVICE_TYPE}.local."),
                            &instance,
                            &host,
                            (),
                            bound.port(),
                            props,
                        ) {
                            Ok(info) => {
                                let info = info.enable_addr_auto();
                                let full = info.get_fullname().to_string();
                                advertised = Some(full.clone());
                                match mdns.register(info) {
                                    Ok(()) => tracing::info!(service = %full, "advertising"),
                                    Err(e) => tracing::warn!(error = %e, "could not advertise"),
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, "could not build service info"),
                        }
                    }
                }
                TransportCmd::Discover { enable } => {
                    if enable && !browsing {
                        browsing = true;
                        let rx = mdns.browse(&format!("{SERVICE_TYPE}.local."))?;
                        tracing::info!(service = %SERVICE_TYPE, "browsing");
                        let sink = sink.clone();
                        let id = self.id;
                        let mine = self.fingerprint.clone();
                        tokio::spawn(async move {
                            while let Ok(ev) = rx.recv_async().await {
                                if let mdns_sd::ServiceEvent::ServiceResolved(info) = ev {
                                    let fp = info
                                        .get_property_val_str("fp")
                                        .and_then(|s| Fingerprint::parse(s).ok());
                                    // Do not report ourselves back to the core.
                                    if fp.as_ref() == Some(&mine) {
                                        continue;
                                    }
                                    // Prefer IPv4. IPv6 link-local addresses
                                    // carry a scope id that has to travel with
                                    // them to be dialable, and nothing in M1
                                    // needs v6 on a LAN.
                                    let addrs = info.get_addresses();
                                    let Some(addr) = addrs
                                        .iter()
                                        .find(|a| a.is_ipv4())
                                        .or_else(|| addrs.iter().next())
                                        .map(|a| a.to_ip_addr())
                                    else {
                                        continue;
                                    };
                                    let sa = SocketAddr::new(addr, info.get_port());
                                    let _ = sink.send(Event::Discovered {
                                        transport: id,
                                        peer: DiscoveredPeer {
                                            fingerprint: fp,
                                            name: info
                                                .get_property_val_str("n")
                                                .unwrap_or_default()
                                                .to_string(),
                                            addr: sa.to_string(),
                                            pairing: info.get_property_val_str("pair") == Some("1"),
                                        },
                                    });
                                }
                            }
                        });
                    }
                }
            }
        }
        if let Some(name) = advertised {
            let _ = mdns.unregister(&name);
        }
        Ok(())
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "acrylius".to_string())
}

/// The default listen port, exposed so a daemon can override it for a second
/// instance on the same machine, which is exactly what the M0 two-daemon test
/// needs.
#[must_use]
pub fn default_port() -> u16 {
    DEFAULT_PORT
}

//! TCP over a local network, with `mdns-sd` for discovery. Frames are `u32`
//! big-endian length then payload, capped so a peer cannot force allocation.

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

/// The largest frame we will read; a peer announcing more is hung up on.
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
        LinkId::new(self.id, self.next_link.fetch_add(1, Ordering::Relaxed))
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

/// How long a peer may be silent, or leave data unacknowledged, before the
/// socket is declared dead. A routing deadline: a dead-but-believed TCP link
/// outranks a working Bluetooth one. The number lives in the core.
const DEAD_PEER: std::time::Duration =
    std::time::Duration::from_millis(acrylius_core::link::DEAD_PEER_MS);

/// Backoff after a failed `accept`, so descriptor exhaustion cannot spin the loop.
const ACCEPT_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

/// Make a vanished peer show up as a broken socket in bounded time: keepalive
/// covers the idle socket, `TCP_USER_TIMEOUT` the one with bytes stuck in the
/// send queue, which keepalive alone does not. Failures to set either are
/// ignored deliberately.
fn bound_the_wait_for_a_dead_peer(stream: &TcpStream) {
    let sock = socket2::SockRef::from(stream);
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(DEAD_PEER / 2)
        .with_interval(DEAD_PEER / 4);
    let _ = sock.set_tcp_keepalive(&keepalive);
    #[cfg(target_os = "linux")]
    let _ = sock.set_tcp_user_timeout(Some(DEAD_PEER));
}

/// What a browse event means to the core, if anything. A free function so a
/// test can reach it; `reported` is the memory that makes a withdrawal possible.
fn discovery_event(
    id: TransportId,
    mine: &Fingerprint,
    reported: &mut HashMap<String, String>,
    ev: mdns_sd::ServiceEvent,
) -> Option<Event> {
    match ev {
        // Only if we ever reported it; otherwise the core has nothing to remove.
        mdns_sd::ServiceEvent::ServiceRemoved(_, fullname) => Some(Event::Undiscovered {
            transport: id,
            addr: reported.remove(&fullname)?,
        }),
        mdns_sd::ServiceEvent::ServiceResolved(info) => {
            let fp = info
                .get_property_val_str("fp")
                .and_then(|s| Fingerprint::parse(s).ok());
            // Do not report ourselves back to the core.
            if fp.as_ref() == Some(mine) {
                return None;
            }
            // Prefer IPv4: IPv6 link-local needs a scope id to be dialable.
            let addrs = info.get_addresses();
            let addr = addrs
                .iter()
                .find(|a| a.is_ipv4())
                .or_else(|| addrs.iter().next())
                .map(|a| a.to_ip_addr())?;
            let sa = SocketAddr::new(addr, info.get_port());
            reported.insert(info.get_fullname().to_string(), sa.to_string());
            Some(Event::Discovered {
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
            })
        }
        _ => None,
    }
}

async fn serve(
    link: LinkId,
    stream: TcpStream,
    attrs: LinkAttrs,
    dial: Option<DialToken>,
    sink: EventSink,
    writers: Writers,
) {
    let _ = stream.set_nodelay(true);
    bound_the_wait_for_a_dead_peer(&stream);
    let (mut rd, mut wr) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
    writers.lock().await.insert(link, tx);

    let _ = sink.send(Event::LinkUp { link, attrs, dial });

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

    let reason = loop {
        tokio::select! {
            // The writer ending always means the link is over. Without this arm
            // a closed link's read half sits parked in `read_frame` until the
            // keepalive fires.
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
                    // Errors here are mostly about the accepted connection
                    // (ECONNABORTED, EMFILE), not the listener; returning would
                    // stop answering TCP for good. The pause keeps descriptor
                    // exhaustion from spinning the loop.
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed; still listening");
                        tokio::time::sleep(ACCEPT_RETRY).await;
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
                        // The name is a discovery hint only; identity comes
                        // from the handshake.
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
                            // mDNS withdraws a *name*, but the core was told an
                            // address; only this map can bridge the two.
                            let mut reported: HashMap<String, String> = HashMap::new();
                            while let Ok(ev) = rx.recv_async().await {
                                if let Some(event) = discovery_event(id, &mine, &mut reported, ev) {
                                    let _ = sink.send(event);
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

/// Exposed so a daemon can override it for a second instance on one machine.
#[must_use]
pub fn default_port() -> u16 {
    DEFAULT_PORT
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: TransportId = TransportId(1);

    fn fp(byte: u8) -> Fingerprint {
        let mut key = [0u8; 32];
        key[31] = byte;
        Fingerprint::of(&key)
    }

    fn resolved(name: &str, ip: &str, props: &[(&str, &str)]) -> mdns_sd::ServiceEvent {
        let props: Vec<(String, String)> = props
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        let info = mdns_sd::ServiceInfo::new(
            "_acrylius._tcp.local.",
            name,
            "host.local.",
            ip,
            1971,
            &props[..],
        )
        .expect("a service record");
        mdns_sd::ServiceEvent::ServiceResolved(Box::new(info.as_resolved_service()))
    }

    fn removed(name: &str) -> mdns_sd::ServiceEvent {
        mdns_sd::ServiceEvent::ServiceRemoved(
            "_acrylius._tcp.local.".to_string(),
            format!("{name}._acrylius._tcp.local."),
        )
    }

    #[test]
    fn a_resolved_service_becomes_an_address_to_try() {
        let mut reported = HashMap::new();
        let ev = discovery_event(
            ID,
            &fp(1),
            &mut reported,
            resolved(
                "bravo",
                "10.0.0.9",
                &[("fp", fp(2).as_str()), ("n", "bravo")],
            ),
        );
        let Some(Event::Discovered { peer, transport }) = ev else {
            panic!("a stranger on the network is a sighting, got {ev:?}");
        };
        assert_eq!(transport, ID);
        assert_eq!(peer.addr, "10.0.0.9:1971");
        assert_eq!(peer.name, "bravo");
        assert_eq!(peer.fingerprint, Some(fp(2)));
        assert!(!peer.pairing, "nothing said it was waiting to pair");
    }

    #[test]
    fn a_pairing_window_is_carried_only_when_it_says_so() {
        let mut reported = HashMap::new();
        let waiting = discovery_event(
            ID,
            &fp(1),
            &mut reported,
            resolved(
                "bravo",
                "10.0.0.9",
                &[("fp", fp(2).as_str()), ("pair", "1")],
            ),
        );
        assert!(
            matches!(waiting, Some(Event::Discovered { peer, .. }) if peer.pairing),
            "a machine advertising an open window was not reported as one"
        );
        // Only `pair=1` is an invitation, not a value that merely exists.
        let not = discovery_event(
            ID,
            &fp(1),
            &mut HashMap::new(),
            resolved(
                "bravo",
                "10.0.0.9",
                &[("fp", fp(2).as_str()), ("pair", "0")],
            ),
        );
        assert!(
            matches!(not, Some(Event::Discovered { peer, .. }) if !peer.pairing),
            "pair=0 was read as an open pairing window"
        );
    }

    #[test]
    fn this_machine_is_not_reported_back_to_itself() {
        let mine = fp(1);
        let mut reported = HashMap::new();
        let ev = discovery_event(
            ID,
            &mine,
            &mut reported,
            resolved("alpha", "10.0.0.9", &[("fp", mine.as_str())]),
        );
        assert!(ev.is_none(), "the daemon discovered itself, got {ev:?}");
        assert!(
            reported.is_empty(),
            "and it must not be remembered either, or its own record lapsing \
             would be announced as a machine leaving"
        );
    }

    #[test]
    fn a_withdrawal_names_the_address_the_sighting_did() {
        let mut reported = HashMap::new();
        discovery_event(
            ID,
            &fp(1),
            &mut reported,
            resolved("bravo", "10.0.0.9", &[("fp", fp(2).as_str())]),
        );
        let ev = discovery_event(ID, &fp(1), &mut reported, removed("bravo"));
        assert!(
            matches!(&ev, Some(Event::Undiscovered { addr, transport })
                     if addr == "10.0.0.9:1971" && *transport == ID),
            "a service going away did not take its address with it, got {ev:?}"
        );
        assert!(reported.is_empty(), "and it is not withdrawn twice");
    }

    #[test]
    fn a_withdrawal_for_something_never_reported_is_not_news() {
        // Our own advertisement, or a record that never resolved, may lapse.
        let mut reported = HashMap::new();
        let ev = discovery_event(ID, &fp(1), &mut reported, removed("someone-else"));
        assert!(ev.is_none(), "invented a departure, got {ev:?}");
    }
}

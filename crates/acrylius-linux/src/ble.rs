//! Bluetooth LE, as a second transport.
//!
//! The desktop is the peripheral and the phone is the central. That is not a
//! preference; it is forced from three directions that happen to agree.
//! `PROTOCOL.md` §4 already says a device that cannot advertise "dials and is
//! never dialled", which is what iOS is. An iPhone advertising in the background
//! drops its local name and pushes service UUIDs into an Apple-private overflow
//! area no Linux scanner can read. And the iOS app is foreground-only anyway, so
//! it is the side that can afford to initiate.
//!
//! So BlueZ advertises and serves GATT, and `TransportCmd::Discover` is a no-op
//! here — the exact mirror of `advertise` being a no-op in the iOS TCP
//! transport.
//!
//! ## Why this lives in `acrylius-linux` and not beside `tcp.rs`
//!
//! `acrylius-rt` is the runtime for Rust hosts, and its `tcp.rs` would work on
//! any of them. This is BlueZ over D-Bus, which is Linux and nothing else, and
//! it belongs with the other zbus integrations whose patterns it copies. It
//! implements `acrylius_rt::Transport`, so the runtime cannot tell the
//! difference.
//!
//! ## No link-layer pairing
//!
//! Every characteristic uses the plain `read` / `write-without-response` /
//! `notify` flags, never the `encrypt-*` ones. Requiring encryption is what
//! makes iOS raise a pairing dialog, which would mean an `Agent1`
//! implementation, a second trust store to keep in step with the peer records,
//! and a "Forget This Device" that silently breaks the link. Noise already
//! authenticates and encrypts over an untrusted carrier; a BLE link is exactly
//! the untrusted carrier `Noise_IKpsk2` was designed for.
//!
//! ## One central at a time
//!
//! `StartNotify()` takes no arguments — verified against
//! `man 5 org.bluez.GattCharacteristic` and against a real subscribe, which
//! arrived with an empty options dict. A GATT server therefore cannot tell which
//! device subscribed, and notifying means setting the `Value` property, which
//! bluetoothd sends to *every* subscriber. With two phones connected each would
//! receive the other's fragments, fail to decrypt them, and corrupt reassembly.
//!
//! So a second central is refused while one holds the link, and that is stated
//! rather than discovered. `AcquireNotify` returns a per-device descriptor and
//! would lift the restriction; it is deliberately not the first thing built,
//! because the property path is the one proven to work against the phone and the
//! server side of `AcquireNotify` is not.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use acrylius_core::link::{LinkAttrs, LinkDownReason, LinkId, TransportId};
use acrylius_core::proto::{
    BLE_IDENTITY_UUID, BLE_RX_UUID, BLE_SERVICE_UUID, BLE_TX_UUID, ble as frag,
};
use acrylius_core::vocab::Event;
use acrylius_rt::transport::{EventSink, Transport, TransportCmd};
use futures_lite::StreamExt;
use tokio::sync::Mutex;
use zbus::zvariant::{ObjectPath, OwnedValue};

/// Where our GATT tree and advertisement are exported.
const APP_PATH: &str = "/org/acrylius/gatt";
/// Deliberately *outside* `APP_PATH`. `man 5 org.bluez.GattManager` requires the
/// application's ObjectManager to manage "solely the objects of that service",
/// and an advertisement sitting under it would be handed to bluetoothd while it
/// reads the GATT tree.
const ADV_PATH: &str = "/org/acrylius/adv0";
const SERVICE_PATH: &str = "/org/acrylius/gatt/service0";
const IDENTITY_PATH: &str = "/org/acrylius/gatt/service0/char0";
const RX_PATH: &str = "/org/acrylius/gatt/service0/char1";
const TX_PATH: &str = "/org/acrylius/gatt/service0/char2";

const ADAPTER: &str = "/org/bluez/hci0";

/// What we assume the link carries when bluetoothd does not say.
///
/// It always says on `WriteValue`, so this is only ever used before the phone
/// has written anything. 23 is the ATT default and always safe.
const FALLBACK_MTU: u16 = 23;

// --------------------------------------------------------------- BlueZ proxies

#[zbus::proxy(
    interface = "org.bluez.LEAdvertisingManager1",
    default_service = "org.bluez"
)]
trait LeAdvertisingManager {
    fn register_advertisement(
        &self,
        advertisement: &ObjectPath<'_>,
        options: HashMap<&str, &zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<()>;

    fn unregister_advertisement(&self, advertisement: &ObjectPath<'_>) -> zbus::Result<()>;
}

#[zbus::proxy(interface = "org.bluez.GattManager1", default_service = "org.bluez")]
trait GattManager {
    fn register_application(
        &self,
        application: &ObjectPath<'_>,
        options: HashMap<&str, &zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<()>;

    fn unregister_application(&self, application: &ObjectPath<'_>) -> zbus::Result<()>;
}

#[zbus::proxy(interface = "org.bluez.Adapter1", default_service = "org.bluez")]
trait Adapter {
    #[zbus(property)]
    fn powered(&self) -> zbus::Result<bool>;

    /// `["central", "peripheral"]` on an adapter that can do both. One that
    /// cannot be a peripheral cannot serve GATT, and the honest answer is to
    /// offer no BLE transport rather than to fail later.
    #[zbus(property)]
    fn roles(&self) -> zbus::Result<Vec<String>>;
}

// ------------------------------------------------------------------- the state

/// One connected central.
struct Link {
    link: LinkId,
    /// The device object path, which is how bluetoothd names the phone. It is a
    /// resolvable private address unless the phone is bonded, so it rotates and
    /// is never an identity — only a routing handle for as long as it lives.
    device: String,
    mtu: u16,
    reassembler: frag::Reassembler,
}

/// Shared between the exported D-Bus objects and the command loop.
struct Shared {
    id: TransportId,
    sink: EventSink,
    next_link: AtomicU64,
    /// At most one, for the reason in the module docs.
    link: Mutex<Option<Link>>,
    /// What the `identity` characteristic answers: the same facts the mDNS TXT
    /// record carries, since a 43-character fingerprint cannot fit in a 31-byte
    /// advertisement.
    identity: Mutex<Vec<u8>>,
}

impl Shared {
    fn next_link(&self) -> LinkId {
        LinkId::new(self.id, self.next_link.fetch_add(1, Ordering::Relaxed))
    }
}

/// `k=v` per line, which is what the TXT record already is.
fn encode_identity(txt: &[(String, String)]) -> Vec<u8> {
    txt.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes()
}

/// Pull `device` and `mtu` out of the options bluetoothd passes.
///
/// `WriteValue` and `ReadValue` carry both; `StartNotify` carries neither, which
/// is the whole reason a link is established by a write rather than by a
/// subscription.
fn device_and_mtu(options: &HashMap<String, OwnedValue>) -> (Option<String>, Option<u16>) {
    let device = options
        .get("device")
        .and_then(|v| ObjectPath::try_from(v.clone()).ok())
        .map(|p| p.as_str().to_string());
    let mtu = options.get("mtu").and_then(|v| u16::try_from(v).ok());
    (device, mtu)
}

// ------------------------------------------------------------- the advertisement

struct Advertisement {
    name: String,
}

#[zbus::interface(name = "org.bluez.LEAdvertisement1")]
impl Advertisement {
    /// Called when bluetoothd drops the advertisement. There is no need to
    /// unregister in response: by the time this arrives it already has.
    fn release(&self) {
        tracing::debug!("bluetoothd released the advertisement");
    }

    /// `"peripheral"`, which is what makes it connectable. `src/advertising.c`
    /// sets `MGMT_ADV_FLAG_CONNECTABLE` for this type unconditionally, so the
    /// adapter's own `Connectable` property — false on a typical desktop — does
    /// not gate it.
    #[zbus(property, name = "Type")]
    fn kind(&self) -> String {
        "peripheral".to_string()
    }

    /// What the phone's scan filters on.
    ///
    /// `scanForPeripherals(withServices:)` matches the **advertisement**, not
    /// the GATT database. A service registered with `GattManager1` but missing
    /// here is invisible to a filtered scan, with no error on either side — the
    /// single most likely cause of "no services were ever discovered".
    #[zbus(property, name = "ServiceUUIDs")]
    fn service_uuids(&self) -> Vec<String> {
        vec![BLE_SERVICE_UUID.to_string()]
    }

    /// Not decoration, and not the adapter's `Discoverable`.
    ///
    /// `src/advertising.c` only emits a Flags AD element when this property is
    /// present; absent, a non-broadcast advertisement gets `flags = 0x00`, and
    /// otherwise inherits an adapter discoverability that is normally off.
    #[zbus(property)]
    fn discoverable(&self) -> bool {
        true
    }

    /// Truncated by bluetoothd if it does not fit, which it might: flags take 3
    /// of the 31 bytes and a 128-bit UUID takes 18, leaving about ten. The name
    /// is a convenience for a human staring at a scanner app; identity comes
    /// from the `identity` characteristic and, ultimately, from Noise.
    #[zbus(property, name = "LocalName")]
    fn local_name(&self) -> String {
        self.name.clone()
    }

    // `SecondaryChannel` is deliberately absent. Setting it moves us to extended
    // advertising PDUs, which iOS scans poorly or not at all; leaving it unset
    // keeps the kernel on legacy `ADV_IND`, which is what the phone reliably
    // sees. `ScanResponse*` are absent because they are experimental in 5.87 and
    // bluetoothd here runs without `--experimental`.
}

// ------------------------------------------------------------------ GATT tree

struct Service;

#[zbus::interface(name = "org.bluez.GattService1")]
impl Service {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        BLE_SERVICE_UUID.to_string()
    }

    #[zbus(property)]
    fn primary(&self) -> bool {
        true
    }
}

struct IdentityChr {
    shared: Arc<Shared>,
}

#[zbus::interface(name = "org.bluez.GattCharacteristic1")]
impl IdentityChr {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        BLE_IDENTITY_UUID.to_string()
    }

    #[zbus(property)]
    fn service(&self) -> ObjectPath<'_> {
        ObjectPath::from_static_str_unchecked(SERVICE_PATH)
    }

    #[zbus(property)]
    fn flags(&self) -> Vec<String> {
        vec!["read".to_string()]
    }

    async fn read_value(&self, options: HashMap<String, OwnedValue>) -> Vec<u8> {
        let identity = self.shared.identity.lock().await.clone();
        // Logged because this is the last step of the phone's discovery chain,
        // and without it a connection that got everything right and a connection
        // that stopped one call short look identical from here.
        let (device, mtu) = device_and_mtu(&options);
        tracing::debug!(
            device = device.as_deref().unwrap_or("unknown"),
            mtu,
            bytes = identity.len(),
            "identity read"
        );
        identity
    }
}

struct RxChr {
    shared: Arc<Shared>,
}

#[zbus::interface(name = "org.bluez.GattCharacteristic1")]
impl RxChr {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        BLE_RX_UUID.to_string()
    }

    #[zbus(property)]
    fn service(&self) -> ObjectPath<'_> {
        ObjectPath::from_static_str_unchecked(SERVICE_PATH)
    }

    /// Without response: an acknowledged write costs a round trip per fragment,
    /// and the link layer already retransmits.
    #[zbus(property)]
    fn flags(&self) -> Vec<String> {
        vec!["write-without-response".to_string()]
    }

    /// A fragment from the phone.
    ///
    /// This is also where a link is born. The phone always speaks first — the
    /// first thing it sends is a handshake message — so the first write from a
    /// device we have no link for is exactly the moment the link exists, and it
    /// is the only callback that tells us which device we are talking to.
    async fn write_value(&self, value: Vec<u8>, options: HashMap<String, OwnedValue>) {
        let (device, mtu) = device_and_mtu(&options);
        let Some(device) = device else {
            tracing::warn!("a write with no device; ignoring");
            return;
        };

        let mut guard = self.shared.link.lock().await;
        match guard.as_ref() {
            Some(l) if l.device == device => {}
            Some(l) => {
                // See the module docs: notifications cannot be aimed, so a
                // second central would be answered on the first one's link.
                tracing::warn!(
                    holder = %l.device, refused = %device,
                    "another device already holds the BLE link; refusing"
                );
                return;
            }
            None => {
                let link = self.shared.next_link();
                let attrs = LinkAttrs::ble(self.shared.id);
                *guard = Some(Link {
                    link,
                    device: device.clone(),
                    mtu: mtu.unwrap_or(FALLBACK_MTU),
                    reassembler: frag::Reassembler::new(attrs.max_message as usize),
                });
                tracing::info!(%device, ?link, "BLE link up");
                let _ = self.shared.sink.send(Event::LinkUp {
                    link,
                    attrs,
                    dial: None,
                });
            }
        }

        let Some(l) = guard.as_mut() else { return };
        if let Some(m) = mtu {
            l.mtu = m;
        }
        let link = l.link;
        match l.reassembler.push(&value) {
            Ok(Some(msg)) => {
                let _ = self.shared.sink.send(Event::LinkRecv { link, msg });
            }
            Ok(None) => {}
            Err(e) => {
                // A stream we cannot trust. Dropping the link is the honest
                // answer; carrying on would feed the core torn messages.
                tracing::warn!(error = %e, "malformed BLE fragment; dropping the link");
                *guard = None;
                let _ = self.shared.sink.send(Event::LinkDown {
                    link,
                    reason: LinkDownReason::Transport(e.to_string()),
                });
            }
        }
    }
}

struct TxChr {
    value: Vec<u8>,
    notifying: bool,
}

#[zbus::interface(name = "org.bluez.GattCharacteristic1")]
impl TxChr {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        BLE_TX_UUID.to_string()
    }

    #[zbus(property)]
    fn service(&self) -> ObjectPath<'_> {
        ObjectPath::from_static_str_unchecked(SERVICE_PATH)
    }

    #[zbus(property)]
    fn flags(&self) -> Vec<String> {
        vec!["notify".to_string()]
    }

    /// Setting this and emitting `PropertiesChanged` is how a D-Bus GATT server
    /// sends a notification.
    #[zbus(property)]
    fn value(&self) -> Vec<u8> {
        self.value.clone()
    }

    /// Carries no arguments at all — not even which device subscribed. See the
    /// module docs; this is why a link is established by a write.
    fn start_notify(&mut self) {
        self.notifying = true;
        tracing::debug!("a central subscribed to notifications");
    }

    fn stop_notify(&mut self) {
        self.notifying = false;
        tracing::debug!("a central unsubscribed");
    }
}

// ------------------------------------------------------------------- transport

pub struct BleTransport {
    id: TransportId,
    name: String,
}

impl BleTransport {
    #[must_use]
    pub fn new(id: TransportId, name: String) -> Self {
        Self { id, name }
    }

    /// Whether this machine can serve GATT at all.
    ///
    /// Checked rather than assumed, and answered with `Ok(false)` rather than an
    /// error, because "this machine has no Bluetooth" is a normal thing for a
    /// machine to be. The same rule the effectors follow: the machine reports
    /// what it has.
    async fn usable(conn: &zbus::Connection) -> anyhow::Result<bool> {
        let adapter = AdapterProxy::builder(conn)
            .path(ADAPTER)?
            .build()
            .await
            .map_err(|e| anyhow::anyhow!("no adapter at {ADAPTER}: {e}"))?;
        if !adapter.powered().await.unwrap_or(false) {
            tracing::debug!("the Bluetooth adapter is off; no BLE transport");
            return Ok(false);
        }
        let roles = adapter.roles().await.unwrap_or_default();
        if !roles.iter().any(|r| r == "peripheral") {
            tracing::debug!(
                ?roles,
                "the adapter cannot be a peripheral; no BLE transport"
            );
            return Ok(false);
        }
        Ok(true)
    }

    /// Send one whole message as however many notifications it takes.
    async fn notify(conn: &zbus::Connection, mtu: u16, msg: &[u8]) -> anyhow::Result<()> {
        // The ATT payload is the negotiated MTU less the three-byte
        // notification header. Learned from bluetoothd, never assumed.
        let payload = usize::from(mtu.saturating_sub(3)).max(2);
        let server = conn.object_server();
        let iface = server.interface::<_, TxChr>(TX_PATH).await?;
        for f in frag::fragment(msg, payload) {
            let mut tx = iface.get_mut().await;
            tx.value = f;
            tx.value_changed(iface.signal_emitter()).await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Transport for BleTransport {
    fn id(&self) -> TransportId {
        self.id
    }

    async fn run(
        self: Arc<Self>,
        sink: EventSink,
        mut cmds: tokio::sync::mpsc::UnboundedReceiver<TransportCmd>,
    ) -> anyhow::Result<()> {
        let conn = zbus::Connection::system().await?;
        if !Self::usable(&conn).await? {
            // Drain commands so the runtime's sender never blocks on a
            // transport that is present but has nothing to offer.
            while cmds.recv().await.is_some() {}
            return Ok(());
        }

        let shared = Arc::new(Shared {
            id: self.id,
            sink: sink.clone(),
            next_link: AtomicU64::new(1),
            link: Mutex::new(None),
            identity: Mutex::new(Vec::new()),
        });

        // The whole tree first, then the ObjectManager over it, then register.
        //
        // Both halves insist on this order. `man 5 org.bluez.GattManager`:
        // "InterfacesAdded signals will be ignored" — bluetoothd reads the tree
        // exactly once, at RegisterApplication. And zbus emits InterfacesAdded
        // for everything already under the path when the ObjectManager is added,
        // so populating first is also what keeps that quiet.
        let server = conn.object_server();
        server.at(SERVICE_PATH, Service).await?;
        server
            .at(
                IDENTITY_PATH,
                IdentityChr {
                    shared: shared.clone(),
                },
            )
            .await?;
        server
            .at(
                RX_PATH,
                RxChr {
                    shared: shared.clone(),
                },
            )
            .await?;
        server
            .at(
                TX_PATH,
                TxChr {
                    value: Vec::new(),
                    notifying: false,
                },
            )
            .await?;
        server.at(APP_PATH, zbus::fdo::ObjectManager).await?;

        let gatt = GattManagerProxy::builder(&conn)
            .path(ADAPTER)?
            .build()
            .await?;
        let app = ObjectPath::try_from(APP_PATH)?;
        gatt.register_application(&app, HashMap::new()).await?;
        tracing::info!("GATT application registered");

        server
            .at(
                ADV_PATH,
                Advertisement {
                    name: self.name.clone(),
                },
            )
            .await?;

        let ads = LeAdvertisingManagerProxy::builder(&conn)
            .path(ADAPTER)?
            .build()
            .await?;
        let adv = ObjectPath::try_from(ADV_PATH)?;
        let mut advertising = false;

        // A device going away is the only reliable end-of-link signal: the phone
        // is under no obligation to say goodbye. Handled the way every other
        // signal in this crate is — a task that only ever sends into a channel,
        // never into the core.
        {
            let shared = shared.clone();
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender("org.bluez")?
                .interface("org.freedesktop.DBus.Properties")?
                .member("PropertiesChanged")?
                .build();
            let mut stream = zbus::MessageStream::for_match_rule(rule, &conn, None).await?;
            tokio::spawn(async move {
                while let Some(Ok(msg)) = stream.next().await {
                    let Some(path) = msg.header().path().map(|p| p.as_str().to_string()) else {
                        continue;
                    };
                    let Ok((iface, changed, _invalidated)) =
                        msg.body()
                            .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                    else {
                        continue;
                    };
                    if iface != "org.bluez.Device1" {
                        continue;
                    }
                    let still_connected = changed
                        .get("Connected")
                        .and_then(|v| bool::try_from(v).ok());
                    if still_connected != Some(false) {
                        continue;
                    }
                    let mut guard = shared.link.lock().await;
                    if guard.as_ref().is_some_and(|l| l.device == path) {
                        let link = guard.as_ref().map(|l| l.link);
                        *guard = None;
                        if let Some(link) = link {
                            tracing::info!(device = %path, ?link, "BLE link down");
                            let _ = shared.sink.send(Event::LinkDown {
                                link,
                                reason: LinkDownReason::Closed,
                            });
                        }
                    }
                }
            });
        }

        while let Some(cmd) = cmds.recv().await {
            match cmd {
                TransportCmd::Advertise { enable, txt } => {
                    *shared.identity.lock().await = encode_identity(&txt);
                    if enable && !advertising {
                        match ads.register_advertisement(&adv, HashMap::new()).await {
                            Ok(()) => {
                                advertising = true;
                                tracing::info!(name = %self.name, "advertising over BLE");
                            }
                            Err(e) => tracing::warn!(error = %e, "could not advertise over BLE"),
                        }
                    } else if !enable && advertising {
                        let _ = ads.unregister_advertisement(&adv).await;
                        advertising = false;
                    }
                }

                // Nothing to do. The phone scans and dials; this end is found,
                // never finding — the mirror of `advertise` on the iOS TCP
                // transport.
                TransportCmd::Discover { .. } => {}

                // Nothing dials out over BLE. A peripheral is reached, and a
                // core that asks for this has a route it should not have.
                TransportCmd::Dial { dial, addr } => {
                    tracing::debug!(%addr, "BLE cannot dial; a peripheral is dialled");
                    let _ = sink.send(Event::DialFailed {
                        dial,
                        reason: "this device is a BLE peripheral and cannot dial out".to_string(),
                    });
                }

                TransportCmd::Send { link, msg } => {
                    let mtu = {
                        let guard = shared.link.lock().await;
                        match guard.as_ref() {
                            // Not ours: `Action::LinkSend` is offered to every
                            // transport, and the one that recognises the id acts.
                            Some(l) if l.link == link => l.mtu,
                            _ => continue,
                        }
                    };
                    if let Err(e) = Self::notify(&conn, mtu, &msg).await {
                        tracing::warn!(error = %e, "could not notify; dropping the link");
                        let mut guard = shared.link.lock().await;
                        if guard.as_ref().is_some_and(|l| l.link == link) {
                            *guard = None;
                            let _ = sink.send(Event::LinkDown {
                                link,
                                reason: LinkDownReason::Transport(e.to_string()),
                            });
                        }
                    }
                }

                TransportCmd::Close { link } => {
                    let mut guard = shared.link.lock().await;
                    if guard.as_ref().is_some_and(|l| l.link == link) {
                        *guard = None;
                        let _ = sink.send(Event::LinkDown {
                            link,
                            reason: LinkDownReason::Closed,
                        });
                    }
                }
            }
        }

        if advertising {
            let _ = ads.unregister_advertisement(&adv).await;
        }
        let _ = gatt.unregister_application(&app).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tree_nests_the_way_bluetoothd_requires() {
        // A flat tree registers and then publishes nothing: characteristics must
        // be children of their service, and the service a child of the root.
        for c in [IDENTITY_PATH, RX_PATH, TX_PATH] {
            assert!(c.starts_with(&format!("{SERVICE_PATH}/")), "{c}");
        }
        assert!(SERVICE_PATH.starts_with(&format!("{APP_PATH}/")));
        // And the advertisement must sit outside it, or the GATT application's
        // ObjectManager would offer bluetoothd an object that is not part of the
        // service.
        assert!(!ADV_PATH.starts_with(&format!("{APP_PATH}/")));
    }

    #[test]
    fn every_characteristic_has_its_own_path_and_uuid() {
        let paths = [IDENTITY_PATH, RX_PATH, TX_PATH];
        let uuids = [BLE_IDENTITY_UUID, BLE_RX_UUID, BLE_TX_UUID];
        for i in 0..3 {
            for j in (i + 1)..3 {
                assert_ne!(paths[i], paths[j]);
                assert_ne!(uuids[i], uuids[j]);
            }
        }
    }

    #[test]
    fn no_characteristic_asks_for_encryption() {
        // An `encrypt-*` or `secure-*` flag is what raises an iOS pairing
        // dialog, and every recurring iOS/BlueZ GATT failure in the wild is a
        // pairing failure. Noise is the security boundary here.
        let shared = || {
            Arc::new(Shared {
                id: TransportId(2),
                sink: tokio::sync::mpsc::unbounded_channel().0,
                next_link: AtomicU64::new(1),
                link: Mutex::new(None),
                identity: Mutex::new(Vec::new()),
            })
        };
        let flags = [
            IdentityChr { shared: shared() }.flags(),
            RxChr { shared: shared() }.flags(),
            TxChr {
                value: Vec::new(),
                notifying: false,
            }
            .flags(),
        ];
        for f in flags.iter().flatten() {
            assert!(
                !f.contains("encrypt") && !f.contains("secure"),
                "{f} would force bonding"
            );
        }
    }

    #[test]
    fn identity_carries_the_same_facts_as_a_txt_record() {
        let txt = vec![
            ("v".to_string(), "1".to_string()),
            ("fp".to_string(), "abc".to_string()),
        ];
        assert_eq!(encode_identity(&txt), b"v=1\nfp=abc");
    }

    #[test]
    fn link_ids_are_namespaced_so_tcp_and_ble_cannot_collide() {
        let s = Shared {
            id: TransportId(2),
            sink: tokio::sync::mpsc::unbounded_channel().0,
            next_link: AtomicU64::new(1),
            link: Mutex::new(None),
            identity: Mutex::new(Vec::new()),
        };
        let first = s.next_link();
        assert_eq!(first.transport(), TransportId(2));
        assert_ne!(first, LinkId::new(TransportId(1), 1));
    }
}

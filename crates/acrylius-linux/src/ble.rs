//! Bluetooth LE transport: BlueZ advertises and serves GATT, the phone connects.
//!
//! The desktop must be the peripheral: iOS hides an app's local name and
//! service UUIDs while backgrounded, so `TransportCmd::Discover` is a no-op.
//! Characteristics use plain flags, never `encrypt-*` (Noise secures the
//! link); notifying sets `Value`, which bluetoothd broadcasts to every
//! subscriber, so only one central may hold the link at a time.

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

const APP_PATH: &str = "/org/acrylius/gatt";
/// Outside `APP_PATH`: the app's ObjectManager must manage only the service's
/// objects (`man 5 org.bluez.GattManager`).
const ADV_PATH: &str = "/org/acrylius/adv0";
const SERVICE_PATH: &str = "/org/acrylius/gatt/service0";
const IDENTITY_PATH: &str = "/org/acrylius/gatt/service0/char0";
const RX_PATH: &str = "/org/acrylius/gatt/service0/char1";
const TX_PATH: &str = "/org/acrylius/gatt/service0/char2";

const ADAPTER: &str = "/org/bluez/hci0";

/// ATT default; only used before the phone's first write reports the real MTU.
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

    /// Advertising instances bluetoothd holds. Zero while we believe we're on
    /// air means the advertisement was lost without `Release` reaching us.
    #[zbus(property)]
    fn active_instances(&self) -> zbus::Result<u8>;
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

    /// An adapter without the `peripheral` role cannot serve GATT.
    #[zbus(property)]
    fn roles(&self) -> zbus::Result<Vec<String>>;
}

#[zbus::proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
trait Device {
    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;
}

// ------------------------------------------------------------------- the state

/// One connected central.
struct Link {
    link: LinkId,
    /// bluetoothd's object path for the phone: a rotating private address, a
    /// routing handle rather than an identity.
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
    /// What the `identity` characteristic answers: the mDNS TXT facts, which do
    /// not fit in a 31-byte advertisement.
    identity: Mutex<Vec<u8>>,
    /// Whether we want to be on the air, as opposed to whether we are. See
    /// [`readvertise`].
    advertising: std::sync::atomic::AtomicBool,
    /// The last central bluetoothd named, even one that never held a link:
    /// its connection alone took the advertisement off air. See [`supervise`].
    last_central: Mutex<Option<String>>,
    /// When the advertisement was last put back; bursts of departure signals
    /// collapse into one attempt. See [`ADVERT_COOLDOWN`].
    last_advert: Mutex<Option<std::time::Instant>>,
}

impl Shared {
    fn next_link(&self) -> LinkId {
        LinkId::new(self.id, self.next_link.fetch_add(1, Ordering::Relaxed))
    }
}

/// `k=v` per line, matching the TXT record.
fn encode_identity(txt: &[(String, String)]) -> Vec<u8> {
    txt.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes()
}

/// `WriteValue` and `ReadValue` carry both; `StartNotify` carries neither,
/// which is why a link is established by a write, not a subscription.
fn device_and_mtu(options: &HashMap<String, OwnedValue>) -> (Option<String>, Option<u16>) {
    let device = options
        .get("device")
        .and_then(|v| ObjectPath::try_from(v.clone()).ok())
        .map(|p| p.as_str().to_string());
    let mtu = options.get("mtu").and_then(|v| u16::try_from(v).ok());
    (device, mtu)
}

/// A missing object counts as gone: bluetoothd removes unbonded devices
/// outright on disconnect. Asked without the property cache so the answer is
/// current.
async fn device_is_connected(conn: &zbus::Connection, path: &str) -> bool {
    let Ok(path) = ObjectPath::try_from(path.to_string()) else {
        return false;
    };
    let Ok(builder) = DeviceProxy::builder(conn).path(path) else {
        return false;
    };
    match builder
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
    {
        Ok(dev) => dev.connected().await.unwrap_or(false),
        Err(_) => false,
    }
}

/// Put the advertisement back on the air after a connection has taken it off.
///
/// `ActiveInstances` reports what was requested, not what the radio does, so
/// there's no way to check first; this re-registers blind.
async fn readvertise(conn: &zbus::Connection, shared: &Shared) -> bool {
    if !shared
        .advertising
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return false;
    }

    // Unregistering over a live central can take the connection down with it;
    // off the air is the correct state while one is connected.
    if shared.link.lock().await.is_some() {
        return false;
    }

    // One attempt per cooldown: departures arrive in bursts from several
    // watchers, and bluetoothd refuses continuous re-registration.
    {
        let mut last = shared.last_advert.lock().await;
        if let Some(at) = *last
            && at.elapsed() < ADVERT_COOLDOWN
        {
            return false;
        }
        *last = Some(std::time::Instant::now());
    }

    // A bluetoothd restart silently drops the GATT application with no way to
    // check; register again and treat "already registered" as normal.
    if let Ok(builder) = GattManagerProxy::builder(conn).path(ADAPTER)
        && let Ok(gatt) = builder.build().await
        && let Ok(app) = ObjectPath::try_from(APP_PATH)
    {
        match gatt.register_application(&app, HashMap::new()).await {
            Ok(()) => tracing::info!("GATT application registered again"),
            Err(e) => tracing::debug!(error = %e, "GATT application was still registered"),
        }
    }

    let Ok(builder) = LeAdvertisingManagerProxy::builder(conn).path(ADAPTER) else {
        return false;
    };
    let Ok(ads) = builder.build().await else {
        return false;
    };
    let Ok(adv) = ObjectPath::try_from(ADV_PATH) else {
        return false;
    };
    // Let bluetoothd finish tearing the connection down before registering.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let _ = ads.unregister_advertisement(&adv).await;
    match ads.register_advertisement(&adv, HashMap::new()).await {
        Ok(()) => {
            tracing::info!("back on the air");
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not start advertising again");
            false
        }
    }
}

/// The signal watchers see *every* device on the adapter — a headset pausing,
/// a mouse sleeping — so act only on devices this transport was talking to.
async fn is_ours(shared: &Shared, path: &str) -> bool {
    if shared
        .link
        .lock()
        .await
        .as_ref()
        .is_some_and(|l| l.device == path)
    {
        return true;
    }
    shared
        .last_central
        .lock()
        .await
        .as_deref()
        .is_some_and(|d| d == path)
}

/// Retire the link if this device holds it. Idempotent: the core is told once.
async fn drop_link_for(shared: &Shared, path: &str, why: &'static str) {
    let mut guard = shared.link.lock().await;
    if !guard.as_ref().is_some_and(|l| l.device == path) {
        return;
    }
    let Some(l) = guard.take() else { return };
    tracing::info!(device = %path, link = ?l.link, why, "BLE link down");
    let _ = shared.sink.send(Event::LinkDown {
        link: l.link,
        reason: LinkDownReason::Closed,
    });
}

/// Also the time a phone spends unable to see the desktop after a force-quit,
/// so it must read as "a moment" rather than "broken".
const SUPERVISE_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// Slightly under [`SUPERVISE_EVERY`] so a genuine retry on the next tick is
/// never swallowed, while a burst of departure signals collapses to one.
const ADVERT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(4);

/// Reconcile what this transport believes with what bluetoothd reports.
///
/// The two signal watchers are edge-triggered and lose edges in practice, so
/// this poll backs them up.
async fn supervise(conn: &zbus::Connection, shared: &Shared) {
    let held = shared.link.lock().await.as_ref().map(|l| l.device.clone());
    // The link's device if any; else the last central heard from at all, which
    // catches a phone force-quit before it ever wrote.
    let watching = match &held {
        Some(device) => Some(device.clone()),
        None => shared.last_central.lock().await.clone(),
    };
    let central = match &watching {
        Some(device) => Some(device_is_connected(conn, device).await),
        None => None,
    };
    let want = shared
        .advertising
        .load(std::sync::atomic::Ordering::Relaxed);
    let instances = active_instances(conn).await;

    let decision = reconcile(central, want, instances);
    // A wrong answer here is silent by construction, so log every input.
    tracing::debug!(
        ?central,
        want,
        instances,
        ?decision,
        "supervising the radio"
    );

    match decision {
        Reconcile::Nothing => {}
        Reconcile::CentralGone => {
            if let Some(device) = held {
                drop_link_for(shared, &device, "the central went away unannounced").await;
            }
            // Forgotten only once it worked, so a failed registration is
            // retried on the next tick.
            if readvertise(conn, shared).await {
                *shared.last_central.lock().await = None;
            }
        }
        Reconcile::Readvertise => {
            tracing::warn!("not on the air though it should be; registering again");
            readvertise(conn, shared).await;
        }
    }
}

/// Asked without the property cache. An adapter that cannot be asked answers 1,
/// so an unreachable bus is never mistaken for a lost advertisement.
async fn active_instances(conn: &zbus::Connection) -> u8 {
    let Ok(builder) = LeAdvertisingManagerProxy::builder(conn).path(ADAPTER) else {
        return 1;
    };
    let Ok(ads) = builder
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
    else {
        return 1;
    };
    ads.active_instances().await.unwrap_or(1)
}

/// What [`supervise`] does about what bluetoothd answered. Pure, so the
/// expensive mistake — recovering over a working link — is testable.
#[derive(Debug, PartialEq, Eq)]
enum Reconcile {
    /// A central is connected (off the air is then correct), or nothing to fix.
    Nothing,
    /// Retire any link the departed central held, and readvertise.
    CentralGone,
    /// Nothing on the air though we asked to be.
    Readvertise,
}

/// `central`: whether the last central heard from is still connected; `None`
/// when none has been heard from since this was last settled.
fn reconcile(central: Option<bool>, want_advertising: bool, instances: u8) -> Reconcile {
    match central {
        Some(true) => Reconcile::Nothing,
        Some(false) => Reconcile::CentralGone,
        None if want_advertising && instances == 0 => Reconcile::Readvertise,
        None => Reconcile::Nothing,
    }
}

// ------------------------------------------------------------- the advertisement

struct Advertisement {
    name: String,
    shared: Arc<Shared>,
}

#[zbus::interface(name = "org.bluez.LEAdvertisement1")]
impl Advertisement {
    /// bluetoothd has already dropped the advertisement here; re-register on
    /// a task since bluetoothd is still unwinding this call's client.
    async fn release(&self, #[zbus(connection)] conn: &zbus::Connection) {
        tracing::warn!("bluetoothd dropped the advertisement; going back on the air");
        let conn = conn.clone();
        let shared = self.shared.clone();
        tokio::spawn(async move { readvertise(&conn, &shared).await });
    }

    /// `"peripheral"` is what makes it connectable; the adapter's own
    /// `Connectable` property does not gate it.
    #[zbus(property, name = "Type")]
    fn kind(&self) -> String {
        "peripheral".to_string()
    }

    /// iOS `scanForPeripherals(withServices:)` matches the advertisement, not
    /// the GATT database; a service missing here is invisible to the scan.
    #[zbus(property, name = "ServiceUUIDs")]
    fn service_uuids(&self) -> Vec<String> {
        vec![BLE_SERVICE_UUID.to_string()]
    }

    /// Without this property bluetoothd emits no Flags AD element and the
    /// advertisement inherits adapter discoverability, which is normally off.
    #[zbus(property)]
    fn discoverable(&self) -> bool {
        true
    }

    /// Zero disables the timeout; left unset it inherits the adapter's
    /// default (180s), after which iOS stops seeing the advertisement.
    #[zbus(property, name = "DiscoverableTimeout")]
    fn discoverable_timeout(&self) -> u16 {
        0
    }

    /// Truncated by bluetoothd if it does not fit; identity comes from the
    /// `identity` characteristic, not the name.
    #[zbus(property, name = "LocalName")]
    fn local_name(&self) -> String {
        self.name.clone()
    }

    // `SecondaryChannel` is left unset: it selects extended advertising PDUs
    // that iOS scans poorly. `ScanResponse*` here is experimental too.
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
        // The last step of the phone's discovery chain; worth a log line.
        let (device, mtu) = device_and_mtu(&options);
        tracing::info!(
            device = device.as_deref().unwrap_or("unknown"),
            mtu,
            bytes = identity.len(),
            "identity read"
        );
        if let Some(d) = device {
            *self.shared.last_central.lock().await = Some(d);
        }
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

    /// A fragment from the phone. The first write from an unknown device is
    /// where a link is born — the only callback that names the device.
    async fn write_value(
        &self,
        value: Vec<u8>,
        options: HashMap<String, OwnedValue>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) {
        let (device, mtu) = device_and_mtu(&options);
        let Some(device) = device else {
            tracing::warn!("a write with no device; ignoring");
            return;
        };
        *self.shared.last_central.lock().await = Some(device.clone());

        let mut guard = self.shared.link.lock().await;

        // Retire a holder that's gone before refusing this device: the phone
        // returns under a fresh address, so a missed departure would wedge BLE.
        let holder = match guard.as_ref() {
            Some(l) if l.device != device => Some(l.device.clone()),
            _ => None,
        };
        if let Some(holder) = holder
            && !device_is_connected(conn, &holder).await
        {
            drop(guard);
            drop_link_for(&self.shared, &holder, "the central is gone").await;
            guard = self.shared.link.lock().await;
        }

        match guard.as_ref() {
            Some(l) if l.device == device => {}
            Some(l) => {
                // Notifications cannot be aimed; see the module docs.
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
                // Carrying on would feed the core torn messages.
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
    shared: Arc<Shared>,
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

    /// Carries no arguments, not even which device subscribed. iOS subscribes
    /// once per connection, so anything still held belongs to a finished session.
    async fn start_notify(&mut self) {
        self.notifying = true;
        // The only record that a phone got this far; keep it at `info`.
        tracing::info!("a central subscribed to notifications");
        // Read and released before retiring, so the lock is never held twice.
        let held = self
            .shared
            .link
            .lock()
            .await
            .as_ref()
            .map(|l| l.device.clone());
        if let Some(device) = held {
            drop_link_for(&self.shared, &device, "a central subscribed afresh").await;
        }
    }

    fn stop_notify(&mut self) {
        self.notifying = false;
        tracing::info!("a central unsubscribed");
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

    /// `Ok(false)` rather than an error: having no Bluetooth is normal.
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
        // ATT payload: the negotiated MTU less the 3-byte notification header.
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
            // Drain commands so the runtime's sender never blocks.
            while cmds.recv().await.is_some() {}
            return Ok(());
        }

        let shared = Arc::new(Shared {
            id: self.id,
            sink: sink.clone(),
            next_link: AtomicU64::new(1),
            link: Mutex::new(None),
            identity: Mutex::new(Vec::new()),
            advertising: std::sync::atomic::AtomicBool::new(false),
            last_central: Mutex::new(None),
            last_advert: Mutex::new(None),
        });

        // Tree first, then ObjectManager, then register: bluetoothd reads the
        // tree once at RegisterApplication and ignores InterfacesAdded after.
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
                    shared: shared.clone(),
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
                    shared: shared.clone(),
                },
            )
            .await?;

        let ads = LeAdvertisingManagerProxy::builder(&conn)
            .path(ADAPTER)?
            .build()
            .await?;
        let adv = ObjectPath::try_from(ADV_PATH)?;
        let mut advertising = false;

        // A departure arrives as `Connected = false`, an `InterfacesRemoved`
        // (unbonded devices are removed outright), or both; both are watched.
        {
            let shared = shared.clone();
            let conn = conn.clone();
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
                    if !is_ours(&shared, &path).await {
                        continue;
                    }
                    drop_link_for(&shared, &path, "the central disconnected").await;
                    readvertise(&conn, &shared).await;
                }
            });
        }

        // The other shape: bluetoothd forgetting an unbonded central.
        {
            let shared = shared.clone();
            let conn = conn.clone();
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender("org.bluez")?
                .interface("org.freedesktop.DBus.ObjectManager")?
                .member("InterfacesRemoved")?
                .build();
            let mut stream = zbus::MessageStream::for_match_rule(rule, &conn, None).await?;
            tokio::spawn(async move {
                while let Some(Ok(msg)) = stream.next().await {
                    // Owned: the body it is deserialised from is a temporary.
                    let Ok((path, ifaces)) = msg
                        .body()
                        .deserialize::<(zbus::zvariant::OwnedObjectPath, Vec<String>)>()
                    else {
                        continue;
                    };
                    if !ifaces.iter().any(|i| i == "org.bluez.Device1") {
                        continue;
                    }
                    if !is_ours(&shared, path.as_str()).await {
                        continue;
                    }
                    drop_link_for(&shared, path.as_str(), "bluetoothd forgot the device").await;
                    readvertise(&conn, &shared).await;
                }
            });
        }

        // The backstop for departures neither watcher saw. See [`supervise`].
        {
            let shared = shared.clone();
            let conn = conn.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(SUPERVISE_EVERY);
                // A tokio interval's first tick completes immediately.
                tick.tick().await;
                loop {
                    tick.tick().await;
                    supervise(&conn, &shared).await;
                }
            });
        }

        while let Some(cmd) = cmds.recv().await {
            match cmd {
                TransportCmd::Advertise { enable, txt } => {
                    *shared.identity.lock().await = encode_identity(&txt);
                    if enable && !advertising {
                        // Intent is recorded before the attempt and kept on
                        // failure, so a lost race with prior teardown gets retried by `supervise`.
                        advertising = true;
                        shared
                            .advertising
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        match ads.register_advertisement(&adv, HashMap::new()).await {
                            Ok(()) => tracing::info!(name = %self.name, "advertising over BLE"),
                            Err(e) => {
                                tracing::warn!(error = %e, "could not advertise over BLE yet");
                            }
                        }
                    } else if !enable && advertising {
                        // Cleared first: a disconnect arriving mid-teardown
                        // must not put back what is being taken down.
                        shared
                            .advertising
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                        let _ = ads.unregister_advertisement(&adv).await;
                        advertising = false;
                    }
                }

                // The phone scans and dials; this end is found, never finding.
                TransportCmd::Discover { .. } => {}

                // A peripheral cannot dial out.
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
                            // `Action::LinkSend` is offered to every transport;
                            // the one that recognises the id acts.
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
        // A flat tree registers and then publishes nothing.
        for c in [IDENTITY_PATH, RX_PATH, TX_PATH] {
            assert!(c.starts_with(&format!("{SERVICE_PATH}/")), "{c}");
        }
        assert!(SERVICE_PATH.starts_with(&format!("{APP_PATH}/")));
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
    fn the_advertisement_never_stops_being_discoverable() {
        // Anything but zero makes the desktop silently vanish minutes after it
        // appears; see `discoverable_timeout`.
        let adv = Advertisement {
            name: "test".to_string(),
            shared: Arc::new(Shared {
                id: TransportId(2),
                sink: tokio::sync::mpsc::unbounded_channel().0,
                next_link: AtomicU64::new(1),
                link: Mutex::new(None),
                identity: Mutex::new(Vec::new()),
                advertising: std::sync::atomic::AtomicBool::new(false),
                last_central: Mutex::new(None),
                last_advert: Mutex::new(None),
            }),
        };
        assert!(adv.discoverable(), "or no Flags element is emitted at all");
        assert_eq!(
            adv.discoverable_timeout(),
            0,
            "a desktop is not discoverable for three minutes; it is discoverable"
        );
    }

    #[test]
    fn a_working_link_is_never_disturbed_to_fix_a_problem_it_does_not_have() {
        // Re-registering would drop a connected phone mid-session.
        assert_eq!(reconcile(Some(true), true, 0), Reconcile::Nothing);
        assert_eq!(reconcile(Some(true), true, 1), Reconcile::Nothing);
    }

    #[test]
    fn a_central_that_is_gone_is_noticed_whether_or_not_anyone_said_so() {
        assert_eq!(reconcile(Some(false), true, 0), Reconcile::CentralGone);
        // Even when bluetoothd still counts an instance it is not broadcasting.
        assert_eq!(reconcile(Some(false), true, 1), Reconcile::CentralGone);
    }

    #[test]
    fn an_advertisement_that_is_not_on_the_air_is_put_back() {
        // Covers both "dropped" and "never registered"; indistinguishable here.
        assert_eq!(reconcile(None, true, 0), Reconcile::Readvertise);
    }

    #[test]
    fn nothing_is_registered_behind_the_owners_back() {
        // Recovering an advertisement nobody asked for would override
        // `ble.enabled = false`.
        assert_eq!(reconcile(None, false, 0), Reconcile::Nothing);
        assert_eq!(reconcile(None, true, 1), Reconcile::Nothing);
    }

    #[test]
    fn no_characteristic_asks_for_encryption() {
        // An `encrypt-*`/`secure-*` flag raises an iOS pairing dialog; Noise is
        // the security boundary here.
        let shared = || {
            Arc::new(Shared {
                id: TransportId(2),
                sink: tokio::sync::mpsc::unbounded_channel().0,
                next_link: AtomicU64::new(1),
                link: Mutex::new(None),
                identity: Mutex::new(Vec::new()),
                advertising: std::sync::atomic::AtomicBool::new(false),
                last_central: Mutex::new(None),
                last_advert: Mutex::new(None),
            })
        };
        let flags = [
            IdentityChr { shared: shared() }.flags(),
            RxChr { shared: shared() }.flags(),
            TxChr {
                value: Vec::new(),
                notifying: false,
                shared: shared(),
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

    /// A `Shared` holding a link to `device`, plus the receiver to watch.
    fn holding(device: &str) -> (Arc<Shared>, tokio::sync::mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let id = TransportId(2);
        let shared = Arc::new(Shared {
            id,
            sink: tx,
            next_link: AtomicU64::new(1),
            link: Mutex::new(None),
            identity: Mutex::new(Vec::new()),
            advertising: std::sync::atomic::AtomicBool::new(false),
            last_central: Mutex::new(None),
            last_advert: Mutex::new(None),
        });
        let link = shared.next_link();
        *shared.link.try_lock().unwrap() = Some(Link {
            link,
            device: device.to_string(),
            mtu: 517,
            reassembler: frag::Reassembler::new(LinkAttrs::ble(id).max_message as usize),
        });
        (shared, rx)
    }

    #[tokio::test]
    async fn a_departing_central_frees_the_link_for_the_next_one() {
        let (shared, mut rx) = holding("/org/bluez/hci0/dev_75_C3_D4_C8_ED_AB");
        drop_link_for(&shared, "/org/bluez/hci0/dev_75_C3_D4_C8_ED_AB", "gone").await;

        assert!(shared.link.lock().await.is_none());
        assert!(matches!(
            rx.try_recv(),
            Ok(Event::LinkDown {
                reason: LinkDownReason::Closed,
                ..
            })
        ));
        // Two signals can describe one departure; the core is told once.
        drop_link_for(&shared, "/org/bluez/hci0/dev_75_C3_D4_C8_ED_AB", "gone").await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_unrelated_bluetooth_device_is_not_mistaken_for_the_phone() {
        // Acting on every device meant headphones disconnecting could drop the
        // phone's session.
        let (shared, _rx) = holding("/org/bluez/hci0/dev_4B_FD_BC_CC_54_FD");
        assert!(is_ours(&shared, "/org/bluez/hci0/dev_4B_FD_BC_CC_54_FD").await);
        assert!(!is_ours(&shared, "/org/bluez/hci0/dev_AD_03_00_00_36_33").await);
    }

    #[tokio::test]
    async fn a_central_that_never_wrote_still_counts_as_ours() {
        // Its connection still took the advertisement off the air.
        let (shared, _rx) = holding("/org/bluez/hci0/dev_4B_FD_BC_CC_54_FD");
        *shared.link.lock().await = None;
        *shared.last_central.lock().await =
            Some("/org/bluez/hci0/dev_7E_46_81_5F_49_2D".to_string());

        assert!(is_ours(&shared, "/org/bluez/hci0/dev_7E_46_81_5F_49_2D").await);
        assert!(!is_ours(&shared, "/org/bluez/hci0/dev_AD_03_00_00_36_33").await);
    }

    #[tokio::test]
    async fn a_central_subscribing_again_retires_the_link_it_had() {
        // A force-quit phone can reconnect before bluetoothd retires the
        // device on the same address; the stale link record must not be reused.
        let device = "/org/bluez/hci0/dev_75_C3_D4_C8_ED_AB";
        let (shared, mut rx) = holding(device);
        let mut tx = TxChr {
            value: Vec::new(),
            notifying: false,
            shared: shared.clone(),
        };

        tx.start_notify().await;

        assert!(
            shared.link.lock().await.is_none(),
            "a subscription is the start of a session, so the old one is over"
        );
        assert!(
            matches!(rx.try_recv(), Ok(Event::LinkDown { .. })),
            "and the core is told, rather than left holding a link nobody is on"
        );
    }

    #[tokio::test]
    async fn a_first_subscription_has_nothing_to_retire() {
        let (tx_sink, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            id: TransportId(2),
            sink: tx_sink,
            next_link: AtomicU64::new(1),
            link: Mutex::new(None),
            identity: Mutex::new(Vec::new()),
            advertising: std::sync::atomic::AtomicBool::new(false),
            last_central: Mutex::new(None),
            last_advert: Mutex::new(None),
        });
        let mut tx = TxChr {
            value: Vec::new(),
            notifying: false,
            shared: shared.clone(),
        };

        tx.start_notify().await;

        assert!(tx.notifying);
        assert!(rx.try_recv().is_err(), "nothing to report on a fresh link");
    }

    #[tokio::test]
    async fn some_other_device_leaving_does_not_touch_our_link() {
        let (shared, mut rx) = holding("/org/bluez/hci0/dev_75_C3_D4_C8_ED_AB");
        drop_link_for(&shared, "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF", "gone").await;

        assert!(shared.link.lock().await.is_some());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn link_ids_are_namespaced_so_tcp_and_ble_cannot_collide() {
        let s = Shared {
            id: TransportId(2),
            sink: tokio::sync::mpsc::unbounded_channel().0,
            next_link: AtomicU64::new(1),
            link: Mutex::new(None),
            identity: Mutex::new(Vec::new()),
            advertising: std::sync::atomic::AtomicBool::new(false),
            last_central: Mutex::new(None),
            last_advert: Mutex::new(None),
        };
        let first = s.next_link();
        assert_eq!(first.transport(), TransportId(2));
        assert_ne!(first, LinkId::new(TransportId(1), 1));
    }
}

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

    /// How many advertising instances bluetoothd holds, across every client on
    /// the bus. Zero, while this transport believes it is on the air, means the
    /// advertisement went away without `Release` ever reaching us — which is
    /// the one thing that presents to a user as "the desktop vanished".
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

    /// `["central", "peripheral"]` on an adapter that can do both. One that
    /// cannot be a peripheral cannot serve GATT, and the honest answer is to
    /// offer no BLE transport rather than to fail later.
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
    /// Whether we want to be on the air, as opposed to whether we are.
    ///
    /// Those two come apart, which is the whole reason this exists — see
    /// [`readvertise`].
    advertising: std::sync::atomic::AtomicBool,
    /// The last central bluetoothd named to us, whether or not it ever got as
    /// far as holding a link.
    ///
    /// A connection takes the advertisement off the air the moment it is made,
    /// well before the phone has written anything. So a central that connects,
    /// reads `identity`, and is then force-quit leaves nothing behind for the
    /// link record to catch — and the desktop stays silently off the air. This
    /// is how [`supervise`] finds that case.
    last_central: Mutex<Option<String>>,
    /// When the advertisement was last put back, so that bursts of departure
    /// signals collapse into one attempt. See [`ADVERT_COOLDOWN`].
    last_advert: Mutex<Option<std::time::Instant>>,
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

/// Whether bluetoothd still has this device, and still calls it connected.
///
/// A missing object counts as gone, and that is the ordinary case rather than
/// an error: an unbonded central is a temporary device, and bluetoothd removes
/// temporary devices outright when they disconnect instead of leaving one
/// behind marked `Connected = false`.
///
/// Asked without the property cache, because the answer is only worth having if
/// it is current.
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
/// A peripheral stops advertising the moment a central connects — the
/// controller does that itself, and it is correct. What is supposed to happen
/// afterwards is that it starts again, and it is not certain it always does.
///
/// Belt and braces, and stated as such. The measured cause of the desktop
/// disappearing was the discoverable timeout — see `discoverable_timeout` — and
/// that is fixed where it is caused. This covers the other half of the same
/// symptom, because registering again is what a daemon restart does and a
/// restart is what was observed to bring the desktop back.
///
/// There is no local instrument for "is the radio actually advertising":
/// `ActiveInstances` reports what bluetoothd was asked for rather than what the
/// controller is doing, and `btmgmt info`'s `advertising` flag tracks the
/// legacy `Set Advertising` setting, not the instances the D-Bus API adds — it
/// reads the same whether this works or not. So this is cheap insurance, not a
/// diagnosis: registering again when nothing was lost costs two D-Bus calls.
async fn readvertise(conn: &zbus::Connection, shared: &Shared) -> bool {
    if !shared
        .advertising
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return false;
    }

    // Never over a live central. Unregistering an advertisement while a phone
    // is connected can take the connection down with it, and a Noise handshake
    // interrupted half way through fails on the next message with a decrypt
    // error — which is a long way from anything that would make a person
    // suspect the radio. Being off the air *is* the correct state while a
    // central is connected.
    if shared.link.lock().await.is_some() {
        return false;
    }

    // At most one attempt per cooldown, from however many callers.
    //
    // Departures arrive in bursts and from several watchers at once, and each
    // attempt sleeps 500ms before registering. Without this the bursts queued
    // and drained into a re-registration every half second — the advertisement
    // taken down and put back continuously, which is worse than the fault it
    // was added to repair, and which bluetoothd eventually refused outright
    // with "Failed to complete registration".
    {
        let mut last = shared.last_advert.lock().await;
        if let Some(at) = *last
            && at.elapsed() < ADVERT_COOLDOWN
        {
            return false;
        }
        *last = Some(std::time::Instant::now());
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
    // Let bluetoothd finish tearing the connection down first; registering
    // into the middle of that is how the "Failed to register advertisement"
    // this used to log on startup happened.
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

/// Retire the link, if this device is the one holding it.
///
/// Shared by every path that can learn a central went away, because the core
/// must be told exactly once and the transport must not keep a link nobody is
/// on the other end of.
/// Whether this device path is one this transport was talking to.
///
/// The signal watchers see *every* device on the adapter — a headset pausing,
/// a mouse sleeping, anything at all. Acting on those was a mistake with real
/// consequences: each one triggered a re-registration, so an unrelated pair of
/// headphones could take the desktop's advertisement off the air.
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

/// How often to check on what the departure signals were supposed to tell us.
///
/// This is a recovery time as much as a poll interval: it is how long a phone
/// spends unable to see the desktop after the app is force-quit and reopened.
/// The person doing that is watching the screen and waiting, so the number has
/// to be small enough to read as "a moment" rather than as "broken".
///
/// What it costs is two D-Bus round trips to a daemon on the same machine,
/// which is nothing. What it risks is retiring a link on a transient wrong
/// answer — and there is no such answer here: `device_is_connected` is asked of
/// bluetoothd without the cache, and bluetoothd does not drop a device object
/// in the middle of a live connection.
const SUPERVISE_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// The shortest gap between two attempts to put the advertisement back.
///
/// Slightly under [`SUPERVISE_EVERY`], so a genuine retry on the next tick is
/// never swallowed, while a burst of departure signals collapses to one.
const ADVERT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(4);

/// Reconcile what this transport believes with what bluetoothd reports.
///
/// The two signal watchers below are edge-triggered, and the edges get lost. On
/// this hardware, across a day of testing, the transport logged **nine links up
/// and one link down**: eight departures that bluetoothd never announced on the
/// bus in any shape either watcher recognised. Everything downstream follows
/// from that one gap, and each piece of it was reported separately as its own
/// bug:
///
/// * the core keeps routing to a link whose central is gone, so reopening the
///   app appears to do nothing until a second force-quit — the link is only
///   displaced once something else happens to retire it;
/// * [`readvertise`] is never reached, because it hangs off the same departure
///   path, so the advertisement stays off the air and the phone reports that
///   nothing is advertising the service — until the daemon is restarted, which
///   is the only other thing that registers an advertisement.
///
/// Neither is visible from inside this process. Both are one question away from
/// bluetoothd, which is the only party here that actually knows. So the signals
/// stay as the fast path, and this is the truth that backs them.
async fn supervise(conn: &zbus::Connection, shared: &Shared) {
    let held = shared.link.lock().await.as_ref().map(|l| l.device.clone());
    // The link's device if there is one, because that is the one whose
    // departure the core needs told about; otherwise the last central we heard
    // from at all, which catches a phone that connected, read `identity` and
    // was force-quit before it ever wrote.
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
    // Every input and the answer, at debug. A wrong answer here is silent by
    // construction — `active_instances` reports 1 for an adapter it cannot ask,
    // so a proxy that reads the wrong property would simply never recover
    // anything and never say why.
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
            // Forgotten only once it has actually worked. Otherwise a failed
            // registration would look, on the next tick, exactly like a desktop
            // that never had a central at all — and nothing would try again.
            if readvertise(conn, shared).await {
                *shared.last_central.lock().await = None;
            }
        }
        Reconcile::Readvertise => {
            // Covers both "it was registered and went away" and "it never
            // registered in the first place", which are the same thing from
            // here and want the same answer.
            tracing::warn!("not on the air though it should be; registering again");
            readvertise(conn, shared).await;
        }
    }
}

/// How many advertising instances bluetoothd holds right now.
///
/// Asked without the property cache, because a stale "yes, still advertising"
/// is the one answer that would make [`supervise`] pointless. An adapter that
/// cannot be asked answers 1, so an unreachable bus is never mistaken for an
/// advertisement that needs replacing.
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

/// What [`supervise`] does about what bluetoothd answered.
///
/// Split out and made pure because the expensive mistake here is not failing to
/// recover — it is recovering over a link that was working, and dropping a
/// phone mid-session to fix a problem it did not have. That case is a test
/// rather than a careful reading.
#[derive(Debug, PartialEq, Eq)]
enum Reconcile {
    /// Either a central is connected — in which case being off the air is
    /// correct and not a fault — or there is nothing to put right.
    Nothing,
    /// A central we were talking to has gone. Retire any link it held, and put
    /// the advertisement its connection took down back on the air.
    CentralGone,
    /// No central in the picture, and nothing on the air that we asked to be
    /// there.
    Readvertise,
}

/// `central` is whether the central we last heard from is still connected, or
/// `None` when we have not heard from one since the last time this was settled.
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
    /// Called when bluetoothd drops the advertisement. There is no need to
    /// unregister in response: by the time this arrives it already has.
    ///
    /// There *is* a need to register again, and not doing so was a bug. This is
    /// the single moment the desktop is told it has gone off the air, and it
    /// answered by writing one line at `debug` — below the `info` the daemon
    /// actually runs at, so the report was invisible as well as inert. That
    /// left restarting the daemon as the only thing in the system that put the
    /// advertisement back, which is exactly the shape the symptom had.
    ///
    /// Registering happens on a task rather than here: bluetoothd is still
    /// unwinding the client that owns this call, and [`readvertise`] waits for
    /// it to finish before asking again.
    async fn release(&self, #[zbus(connection)] conn: &zbus::Connection) {
        tracing::warn!("bluetoothd dropped the advertisement; going back on the air");
        let conn = conn.clone();
        let shared = self.shared.clone();
        tokio::spawn(async move { readvertise(&conn, &shared).await });
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

    /// Zero, which `man 5 org.bluez.LEAdvertisement` defines as "the timeout is
    /// disabled and it will stay in discoverable/limited mode forever".
    ///
    /// Not a detail. Left unset, the discoverable window inherits the adapter's
    /// `DiscoverableTimeout`, which is **180 seconds** on a default install —
    /// so three minutes after registering, the Flags element stops saying
    /// discoverable and the advertisement goes to `flags = 0x00`, which is
    /// exactly the shape iOS will not surface.
    ///
    /// Nothing announces this. `ActiveInstances` still reports 1, `Release` is
    /// never called, no call fails: from inside the daemon everything looks
    /// correct while the phone sees nothing. The symptom is a desktop that
    /// appears when the daemon is restarted and vanishes a few minutes later,
    /// which reads like a crash and is a timer.
    #[zbus(property, name = "DiscoverableTimeout")]
    fn discoverable_timeout(&self) -> u16 {
        0
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

    /// A fragment from the phone.
    ///
    /// This is also where a link is born. The phone always speaks first — the
    /// first thing it sends is a handshake message — so the first write from a
    /// device we have no link for is exactly the moment the link exists, and it
    /// is the only callback that tells us which device we are talking to.
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

        // Retire a holder that is not there any more, before deciding whether
        // to refuse this one.
        //
        // A phone does not have to say goodbye, and bluetoothd does not have to
        // announce the departure in a form we caught: an unbonded device is
        // removed outright rather than marked disconnected. Refusing on the
        // strength of a link record alone means one missed signal wedges BLE
        // until the daemon restarts — and since the phone comes back under a
        // fresh random address, it is a *different* device path that gets
        // turned away, so the wedge never clears itself. Asking is cheap and
        // happens only when a second device appears.
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

    /// Carries no arguments at all — not even which device subscribed. See the
    /// module docs; this is why a link is established by a write.
    ///
    /// It does say one thing exactly, though, and it is the thing that was
    /// missing: *a central is starting from the beginning*. iOS subscribes once
    /// per connection, during characteristic discovery, so anything still held
    /// when this arrives belongs to a session that is over.
    ///
    /// Without this, a phone whose app was force-quit and reopened reconnected
    /// on the same address before bluetoothd retired the old device, matched
    /// the link record still sitting here, and carried on writing into a link
    /// the core had already torn down — so no `LinkUp` was ever raised and
    /// nothing worked until a second force-quit outlasted the device timeout.
    async fn start_notify(&mut self) {
        self.notifying = true;
        // At `info`, not `debug`. This transport is reached over a radio, by an
        // app installed through CI and a sideload, and every question about it
        // has had to be answered from this journal. A once-per-connection line
        // is not chatter; it is the only record that a phone got this far.
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
            advertising: std::sync::atomic::AtomicBool::new(false),
            last_central: Mutex::new(None),
            last_advert: Mutex::new(None),
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

        // A device going away is the only reliable end-of-link signal: the phone
        // is under no obligation to say goodbye. Handled the way every other
        // signal in this crate is — a task that only ever sends into a channel,
        // never into the core.
        //
        // It arrives in two shapes, and one departure can raise both. An
        // unbonded central is only ever a *temporary* device, and bluetoothd
        // marks it `Connected = false` and then, seconds later on its own
        // timeout, removes the object entirely — observed on this adapter as an
        // `InterfacesRemoved` at `/` naming the path and `org.bluez.Device1`.
        //
        // Watching only the property change means a link outlives a phone that
        // left without one; watching only the removal means waiting out the
        // timeout. So both are watched and the retirement is idempotent, which
        // is the part the tests pin.
        {
            let shared = shared.clone();
            // Cheap: a zbus connection is a handle, not a socket of its own.
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
                    // Only for a device this transport was talking to. A
                    // connection that failed before it ever held a link still
                    // counts — `last_central` is set from the first callback
                    // that names a device, well before any link exists — but a
                    // headset disconnecting does not.
                    if !is_ours(&shared, &path).await {
                        continue;
                    }
                    drop_link_for(&shared, &path, "the central disconnected").await;
                    readvertise(&conn, &shared).await;
                }
            });
        }

        // The other shape. `InterfacesRemoved` names the object path and the
        // interfaces it lost, so an unbonded central that bluetoothd forgets
        // still ends its link promptly rather than at whatever moment something
        // else happens to notice.
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
                    // Owned, because the body it is deserialised from is a
                    // temporary that does not outlive the call below.
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

        // And the backstop under both of them, for the eight departures in nine
        // that neither one saw. See [`supervise`].
        {
            let shared = shared.clone();
            let conn = conn.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(SUPERVISE_EVERY);
                // The first tick of a tokio interval completes immediately, and
                // there is nothing to reconcile before anything has happened.
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
                        // Intent, recorded *before* the attempt and kept
                        // whether or not it succeeds. What the watchers and
                        // `supervise` read to decide whether being on the air
                        // is wanted.
                        //
                        // It used to be set only on success, which made a
                        // failure permanent: this is the one place that ever
                        // registers, the runtime asks exactly once at startup,
                        // and the commonest reason to fail is a race that
                        // passes in a second — bluetoothd still tearing down
                        // the advertisement of the daemon this one replaced. So
                        // a restart that lost the race left the desktop off the
                        // air until the next restart, which is precisely the
                        // "restarting makes it appear, then it disappears
                        // again" report. `supervise` is what retries now.
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
    fn the_advertisement_never_stops_being_discoverable() {
        // Zero means no timeout. Anything else, including inheriting the
        // adapter's 180 seconds by not saying, makes the desktop vanish a few
        // minutes after it appears — with no error, nothing released, and
        // `ActiveInstances` still reporting 1.
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
        // The expensive mistake. A central is connected, so the radio is off
        // the air by design, and re-registering would drop a phone mid-session
        // to cure a symptom that is not present.
        assert_eq!(reconcile(Some(true), true, 0), Reconcile::Nothing);
        assert_eq!(reconcile(Some(true), true, 1), Reconcile::Nothing);
    }

    #[test]
    fn a_central_that_is_gone_is_noticed_whether_or_not_anyone_said_so() {
        // Eight departures in nine reached neither signal watcher. This is the
        // path that catches them, and it does not care which shape was missed.
        assert_eq!(reconcile(Some(false), true, 0), Reconcile::CentralGone);
        // Including when bluetoothd still counts an instance it is not
        // broadcasting — which is the ordinary case, and the reason a count of
        // instances cannot be the only thing this looks at.
        assert_eq!(reconcile(Some(false), true, 1), Reconcile::CentralGone);
    }

    #[test]
    fn an_advertisement_that_is_not_on_the_air_is_put_back() {
        // Two histories, one state. Either bluetoothd dropped it, or the
        // registration at startup lost the race against the previous daemon's
        // advertisement being torn down and there has never been one at all.
        // From here they are indistinguishable and want the same answer, which
        // is why the retry lives here rather than beside the first attempt.
        assert_eq!(reconcile(None, true, 0), Reconcile::Readvertise);
    }

    #[test]
    fn nothing_is_registered_behind_the_owners_back() {
        // `ble.enabled = false`, or the transport was told to stop advertising.
        // Recovering an advertisement nobody asked for would override the one
        // setting whose entire purpose is to keep this radio quiet.
        assert_eq!(reconcile(None, false, 0), Reconcile::Nothing);
        // And an adapter that is already advertising needs no help.
        assert_eq!(reconcile(None, true, 1), Reconcile::Nothing);
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
        // The phone comes back under a fresh random address, so if a departure
        // is ever missed it is a *different* device path that gets refused and
        // the transport never recovers on its own. This is the retirement that
        // stops that, whichever signal carried the news.
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
        // Exactly once: two signals can describe one departure, and the core
        // must not be told a link died twice.
        drop_link_for(&shared, "/org/bluez/hci0/dev_75_C3_D4_C8_ED_AB", "gone").await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_unrelated_bluetooth_device_is_not_mistaken_for_the_phone() {
        // The signal watchers see every device on the adapter, and acting on
        // all of them was expensive: a pair of headphones disconnecting
        // re-registered the advertisement, and re-registering drops whichever
        // central is connected. Measured on hardware as seventeen
        // re-registrations in eight seconds over a live link, which took the
        // phone's session down mid-handshake and surfaced on the phone as a
        // Noise decrypt error — about as far from the cause as a symptom gets.
        let (shared, _rx) = holding("/org/bluez/hci0/dev_4B_FD_BC_CC_54_FD");
        assert!(is_ours(&shared, "/org/bluez/hci0/dev_4B_FD_BC_CC_54_FD").await);
        assert!(!is_ours(&shared, "/org/bluez/hci0/dev_AD_03_00_00_36_33").await);
    }

    #[tokio::test]
    async fn a_central_that_never_wrote_still_counts_as_ours() {
        // A phone that connects, reads `identity` and is force-quit before it
        // writes anything holds no link at all — but its connection still took
        // the advertisement off the air, so its departure is still the one that
        // has to put it back.
        let (shared, _rx) = holding("/org/bluez/hci0/dev_4B_FD_BC_CC_54_FD");
        *shared.link.lock().await = None;
        *shared.last_central.lock().await =
            Some("/org/bluez/hci0/dev_7E_46_81_5F_49_2D".to_string());

        assert!(is_ours(&shared, "/org/bluez/hci0/dev_7E_46_81_5F_49_2D").await);
        assert!(!is_ours(&shared, "/org/bluez/hci0/dev_AD_03_00_00_36_33").await);
    }

    #[tokio::test]
    async fn a_central_subscribing_again_retires_the_link_it_had() {
        // The force-quit case, and the one the address rotation hides. A phone
        // that reopens usually comes back on a *new* random address, and that
        // path was already covered — but when it comes back on the same one,
        // before bluetoothd has retired the device, the old link record matched
        // and was reused. The core had already torn that link down, so nothing
        // raised a new one and the phone stayed dark until a second force-quit
        // outlasted the device timeout.
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

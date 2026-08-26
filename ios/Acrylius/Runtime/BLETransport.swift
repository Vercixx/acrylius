//
//  Bluetooth LE, from the phone's side: the central.
//
//  The desktop advertises and serves GATT; this end scans, connects and writes.
//  That direction is forced rather than chosen — an iPhone advertising in the
//  background drops its local name and pushes service UUIDs into an Apple-private
//  overflow area no Linux scanner can read — and it happens to be the direction
//  `PROTOCOL.md` §4 already describes: a device that cannot advertise "dials and
//  is never dialled".
//
//  This is also the diagnostics probe. There is deliberately not a second
//  CoreBluetooth stack for reporting: everything the Bluetooth screen shows is
//  what this object actually did, because a diagnostic that observes a different
//  code path than the one that breaks is worse than none.
//
//  Guarded, because `scripts/swift-test.sh` compiles this directory on Linux —
//  the same guard `NWTransport.swift` uses for Network.framework.
//
//  ## The traps, named
//
//  Each of these produces "it just does nothing", which is what the previous
//  attempt at this in a predecessor project reported before it was abandoned:
//
//  * Anything asked of a manager before it reports `.poweredOn` is discarded
//    silently, so scanning starts in the state callback and nowhere else.
//  * `scanForPeripherals(withServices:)` matches the **advertisement**, not the
//    GATT database. A service present only in the database is invisible.
//  * A `CBPeripheral` that is not strongly retained is deallocated, and Apple
//    documents that this implicitly cancels the connection. No callback ever
//    fires again.
//  * `delegate` must be set before `discoverServices`, and only from
//    `didConnect`.
//  * Writes past `canSendWriteWithoutResponse` are dropped with no error and
//    surface as corruption much later.
//

#if canImport(CoreBluetooth)

import CoreBluetooth
import Foundation

public final class BLETransport: NSObject, Transport, @unchecked Sendable {
    public let transportId: UInt16

    private let lock = NSLock()
    private var central: CBCentralManager?
    private var emit: (@Sendable (FfiEvent) -> Void)?

    /// Peripherals we have seen, held strongly. See the trap list above.
    private var peers: [UUID: Peer] = [:]
    private var nextLink: UInt64 = 1

    private let serviceUUID: CBUUID
    private let identityUUID: CBUUID
    private let rxUUID: CBUUID
    private let txUUID: CBUUID

    private let report: @Sendable (BLEUpdate) -> Void
    private let queue = DispatchQueue(label: "org.acrylius.ble")

    /// Everything known about one desktop.
    private final class Peer {
        let peripheral: CBPeripheral
        var rx: CBCharacteristic?
        var tx: CBCharacteristic?
        /// Assigned when the core dials, not when we connect: a connection is
        /// how we learn who someone is, and a link is what the core opens.
        var link: UInt64?
        var reassembler: BleReassembler?
        /// Fragments waiting on `canSendWriteWithoutResponse`.
        var pending: [Data] = []
        var fingerprint: String?
        var name: String = "unnamed"

        init(_ p: CBPeripheral) { self.peripheral = p }
    }

    public init(transportId: UInt16, report: @escaping @Sendable (BLEUpdate) -> Void) {
        self.transportId = transportId
        self.serviceUUID = CBUUID(string: bleServiceUuid())
        self.identityUUID = CBUUID(string: bleIdentityUuid())
        self.rxUUID = CBUUID(string: bleRxUuid())
        self.txUUID = CBUUID(string: bleTxUuid())
        self.report = report
        super.init()
    }

    // MARK: - synchronous helpers, so no lock is held across a suspension

    private func setEmit(_ f: @escaping @Sendable (FfiEvent) -> Void) {
        lock.lock(); emit = f; lock.unlock()
    }
    private func fire(_ e: FfiEvent) {
        lock.lock(); let f = emit; lock.unlock()
        f?(e)
    }
    private func push(_ u: BLEUpdate) { report(u) }

    private func peer(_ id: UUID) -> Peer? {
        lock.lock(); defer { lock.unlock() }
        return peers[id]
    }
    private func peer(forLink link: UInt64) -> Peer? {
        lock.lock(); defer { lock.unlock() }
        return peers.values.first { $0.link == link }
    }
    private func remember(_ p: CBPeripheral) -> Peer {
        lock.lock(); defer { lock.unlock() }
        if let existing = peers[p.identifier] { return existing }
        let fresh = Peer(p)
        peers[p.identifier] = fresh
        return fresh
    }
    private func manager() -> CBCentralManager? {
        lock.lock(); defer { lock.unlock() }
        return central
    }
    private func claimLink() -> UInt64 {
        lock.lock(); defer { lock.unlock() }
        // Namespaced by transport, because the core keys every link in one table
        // and NWTransport is counting from 1 as well. The rule lives in Rust so
        // neither host invents its own.
        let id = linkId(transport: transportId, counter: nextLink)
        nextLink += 1
        return id
    }

    /// Whether we may build a manager without ambushing someone.
    ///
    /// `CBManager.authorization` is a class property from iOS 13.1, readable
    /// *without* constructing a manager — which matters, because constructing
    /// one is what raises the prompt. So this doubles as the persisted opt-in:
    /// nothing happens until a person has agreed once, and everything happens
    /// automatically afterwards, with no separate setting to keep in step.
    public static var permitted: Bool { CBManager.authorization == .allowedAlways }

    /// The one spelling of a BLE address. Written by `didUpdateValueFor` when
    /// identity is read, parsed by `dial`; naming it once is what keeps those
    /// two from drifting.
    static let addrPrefix = "ble:"

    // MARK: - Transport

    public func start(events: @escaping @Sendable (FfiEvent) -> Void) async {
        setEmit(events)
    }

    /// iOS never advertises. See the file header.
    public func advertise(enable: Bool, txt: [FfiTxt]) async {}

    public func discover(enable: Bool) async {
        guard enable else {
            manager()?.stopScan()
            push(.scanning(false))
            push(.note("scanning stopped"))
            return
        }
        guard Self.permitted else {
            // Refusing here is the whole reason there is no prompt at launch: a
            // permission dialog with nothing on screen to explain it is one
            // people decline, and declining is undone only in Settings.
            push(.state("waiting for permission", auth: Self.authorizationName()))
            push(.note("not permitted yet; open the Bluetooth screen to allow"))
            return
        }
        startManager()
    }

    /// Build the manager, which is also what raises the permission prompt.
    public func startManager() {
        lock.lock()
        let already = central != nil
        if !already { central = CBCentralManager(delegate: self, queue: queue) }
        lock.unlock()
        guard !already else { return }
        push(.note("central manager created; permission is \(Self.authorizationName())"))
    }

    /// `ble:<peripheral identifier>`, which is what discovery emitted.
    ///
    /// The identifier is CoreBluetooth's own per-app handle for the device, and
    /// it is stable across launches — unlike the desktop's BLE address, which
    /// rotates. By the time the core dials we are normally already connected,
    /// because connecting is how we learned the fingerprint it is dialling.
    public func dial(addr: String, token: UInt64) async {
        guard addr.hasPrefix(Self.addrPrefix),
            let uuid = UUID(uuidString: String(addr.dropFirst(Self.addrPrefix.count)))
        else {
            fire(.dialFailed(dial: token, reason: "not a Bluetooth address: \(addr)"))
            return
        }
        // Onto CoreBluetooth's queue, like everything else that touches a
        // `Peer`. Every delegate callback already runs here, so serialising on
        // this one queue is what makes the shared state safe — the
        // `@unchecked Sendable` on this class is a promise, and this is how it
        // is kept rather than assumed.
        queue.async { [weak self] in
            guard let self else { return }
            guard let p = self.peer(uuid), p.rx != nil, p.tx != nil else {
                self.fire(
                    .dialFailed(
                        dial: token,
                        reason: "that device is not connected over Bluetooth"))
                return
            }
            let attrs = bleAttrs(transport: self.transportId)
            let link = self.claimLink()
            p.link = link
            p.reassembler = BleReassembler(maxMessage: attrs.maxMessage)
            self.push(.link("up to \(p.name)"))
            self.push(.note("link up"))
            self.fire(.linkUp(link: link, attrs: attrs, dial: token))
        }
    }

    public func send(link: UInt64, msg: Data) async {
        queue.async { [weak self] in
            // A link belongs to one transport, and the core does not track
            // which, so every transport is offered every send. Not ours:
            // nothing to do.
            guard let self, let p = self.peer(forLink: link), let rx = p.rx else { return }
            // The negotiated payload, asked for rather than assumed. iOS reports
            // 512 against a modern peripheral and 20 against an old one, and
            // guessing either way is how fragments get silently truncated.
            let size = p.peripheral.maximumWriteValueLength(for: .withoutResponse)
            p.pending.append(contentsOf: bleFragment(msg: msg, fragment: UInt32(size)))
            self.drain(p, rx)
        }
    }

    /// Write what the connection will currently take.
    ///
    /// `canSendWriteWithoutResponse` is not advisory. Fragments written past it
    /// are dropped with no error at all, and the damage shows up much later as a
    /// message that will not reassemble.
    private func drain(_ p: Peer, _ rx: CBCharacteristic) {
        while p.peripheral.canSendWriteWithoutResponse, !p.pending.isEmpty {
            p.peripheral.writeValue(p.pending.removeFirst(), for: rx, type: .withoutResponse)
        }
    }

    public func close(link: UInt64) async {
        queue.async { [weak self] in
            guard let self, let p = self.peer(forLink: link) else { return }
            self.manager()?.cancelPeripheralConnection(p.peripheral)
        }
    }

    static func authorizationName() -> String {
        switch CBManager.authorization {
        case .allowedAlways: return "allowed"
        case .denied: return "denied — only Settings can undo this"
        case .restricted: return "restricted"
        case .notDetermined: return "not asked yet"
        @unknown default: return "unknown"
        }
    }

    static func stateName(_ s: CBManagerState) -> String {
        switch s {
        case .poweredOn: return "powered on"
        case .poweredOff: return "Bluetooth is off"
        case .unauthorized: return "not permitted"
        case .unsupported: return "no Bluetooth on this device"
        case .resetting: return "resetting"
        case .unknown: return "unknown"
        @unknown default: return "unknown"
        }
    }
}

// MARK: - central

extension BLETransport: CBCentralManagerDelegate {
    public func centralManagerDidUpdateState(_ c: CBCentralManager) {
        let name = Self.stateName(c.state)
        let auth = Self.authorizationName()
        push(.state(name, auth: auth))
        guard c.state == .poweredOn else {
            push(.scanning(false))
            return
        }
        c.scanForPeripherals(
            withServices: [serviceUUID],
            options: [CBCentralManagerScanOptionAllowDuplicatesKey: false]
        )
        push(.scanning(true))
        push(.note("scanning for the acrylius service"))
    }

    public func centralManager(
        _ c: CBCentralManager,
        didDiscover peripheral: CBPeripheral,
        advertisementData: [String: Any],
        rssi RSSI: NSNumber
    ) {
        let p = remember(peripheral)
        let advertised = (advertisementData[CBAdvertisementDataServiceUUIDsKey] as? [CBUUID]) ?? []
        let ours = advertised.contains(serviceUUID)
        let name =
            (advertisementData[CBAdvertisementDataLocalNameKey] as? String)
            ?? peripheral.name ?? "unnamed"
        p.name = name
        let sighting = BLESighting(
            id: peripheral.identifier.uuidString,
            name: name, rssi: RSSI.intValue,
            advertisedOurService: ours, lastSeen: Date()
        )
        push(.sighting(sighting))
        guard ours, peripheral.state == .disconnected else { return }
        // Connecting is how identity is learned: a 43-character fingerprint does
        // not fit in a 31-byte advertisement, so it is read over GATT instead.
        c.connect(peripheral, options: nil)
    }

    public func centralManager(_ c: CBCentralManager, didConnect peripheral: CBPeripheral) {
        // Before `discoverServices`, and only from here.
        peripheral.delegate = self
        peripheral.discoverServices([serviceUUID])
        push(.note("connected; discovering services"))
    }

    public func centralManager(
        _ c: CBCentralManager, didFailToConnect peripheral: CBPeripheral, error: (any Error)?
    ) {
        let why = error?.localizedDescription ?? "unknown"
        push(.note("could not connect: \(why)"))
    }

    public func centralManager(
        _ c: CBCentralManager, didDisconnectPeripheral peripheral: CBPeripheral,
        error: (any Error)?
    ) {
        let why = error?.localizedDescription
        lock.lock()
        let p = peers[peripheral.identifier]
        let link = p?.link
        p?.link = nil
        p?.rx = nil
        p?.tx = nil
        p?.reassembler = nil
        p?.pending.removeAll()
        lock.unlock()
        push(.link("none"))
        push(.note("disconnected\(why.map { ": \($0)" } ?? "")"))
        if let link {
            fire(
                .linkDown(
                    link: link,
                    reason: why.map { FfiLinkDown.transport(detail: $0) } ?? .closed))
        }
    }
}

// MARK: - peripheral

extension BLETransport: CBPeripheralDelegate {
    public func peripheral(_ peripheral: CBPeripheral, didDiscoverServices error: (any Error)?) {
        if let error {
            push(.note("service discovery failed: \(error.localizedDescription)"))
            return
        }
        guard let service = peripheral.services?.first(where: { $0.uuid == serviceUUID }) else {
            // The exact symptom the predecessor project died on. Saying so
            // plainly is the reason this screen exists.
            push(.note("connected, but the acrylius service was not there"))
            return
        }
        peripheral.discoverCharacteristics([identityUUID, rxUUID, txUUID], for: service)
    }

    public func peripheral(
        _ peripheral: CBPeripheral, didDiscoverCharacteristicsFor service: CBService,
        error: (any Error)?
    ) {
        if let error {
            push(.note("characteristic discovery failed: \(error.localizedDescription)"))
            return
        }
        let found = service.characteristics ?? []
        let p = remember(peripheral)
        lock.lock()
        p.rx = found.first { $0.uuid == rxUUID }
        p.tx = found.first { $0.uuid == txUUID }
        lock.unlock()
        let identity = found.first { $0.uuid == identityUUID }
        let size = peripheral.maximumWriteValueLength(for: .withoutResponse)
        let n = found.count
        push(.fragment(size))
        push(.note("found \(n) characteristics, fragment \(size) bytes"))
        if let tx = p.tx { peripheral.setNotifyValue(true, for: tx) }
        if let identity { peripheral.readValue(for: identity) }
    }

    public func peripheral(
        _ peripheral: CBPeripheral, didUpdateValueFor characteristic: CBCharacteristic,
        error: (any Error)?
    ) {
        if let error {
            push(.note("read failed: \(error.localizedDescription)"))
            return
        }
        guard let data = characteristic.value else { return }
        let p = remember(peripheral)

        if characteristic.uuid == identityUUID {
            // `k=v` per line: the same facts the mDNS TXT record carries.
            var fields: [String: String] = [:]
            for line in String(decoding: data, as: UTF8.self).split(separator: "\n") {
                if let eq = line.firstIndex(of: "=") {
                    fields[String(line[line.startIndex..<eq])] =
                        String(line[line.index(after: eq)...])
                }
            }
            let fp = fields["fp"]
            let name = fields["n"] ?? p.name
            p.fingerprint = fp
            p.name = name
            push(.note("identity: \(fp.map { String($0.prefix(8)) } ?? "none")"))
            fire(
                .discovered(
                    transport: transportId,
                    peer: FfiDiscoveredPeer(
                        fingerprint: fp,
                        name: name,
                        addr: "\(Self.addrPrefix)\(peripheral.identifier.uuidString)",
                        pairing: fields["pair"] == "1"
                    )))
            return
        }

        guard characteristic.uuid == txUUID, let link = p.link, let r = p.reassembler else {
            return
        }
        do {
            if let msg = try r.push(fragment: data) {
                fire(.linkRecv(link: link, msg: msg))
            }
        } catch {
            // A stream we cannot trust. Dropping the link is the honest answer;
            // carrying on would feed the core torn messages.
            let why = error.localizedDescription
            push(.note("bad fragment: \(why)"))
            lock.lock(); p.link = nil; p.reassembler = nil; lock.unlock()
            fire(.linkDown(link: link, reason: .transport(detail: why)))
        }
    }

    public func peripheralIsReady(toSendWriteWithoutResponse peripheral: CBPeripheral) {
        let p = remember(peripheral)
        guard let rx = p.rx else { return }
        drain(p, rx)
    }
}

#endif

//
//  BLE central: the desktop advertises and serves GATT, this end scans,
//  connects and writes (background iOS advertising is unreadable to Linux
//  scanners; see PROTOCOL.md §4). Also feeds BLEDiagnostics, so the diagnostics
//  observe this code path rather than a second stack.
//  Guarded because scripts/swift-test.sh compiles this directory on Linux.
//
//  CoreBluetooth traps, all of which fail silently:
//  * Requests before `.poweredOn` are discarded; scan only from the state callback.
//  * `scanForPeripherals(withServices:)` matches the advertisement, not the GATT database.
//  * An unretained `CBPeripheral` is deallocated, implicitly cancelling its connection.
//  * `delegate` must be set before `discoverServices`, and only from `didConnect`.
//  * Writes past `canSendWriteWithoutResponse` are dropped with no error.
//

#if canImport(CoreBluetooth)

import CoreBluetooth
import Foundation

public final class BLETransport: NSObject, Transport, @unchecked Sendable {
    public let transportId: UInt16

    private let lock = NSLock()
    private var central: CBCentralManager?
    private var emit: (@Sendable (FfiEvent) -> Void)?

    /// Held strongly; see the trap list.
    private var peers: [UUID: Peer] = [:]
    private var nextLink: UInt64 = 1

    private let serviceUUID: CBUUID
    private let identityUUID: CBUUID
    private let rxUUID: CBUUID
    private let txUUID: CBUUID

    private let report: @Sendable (BLEUpdate) -> Void
    private let queue = DispatchQueue(label: "org.acrylius.ble")

    private final class Peer {
        let peripheral: CBPeripheral
        var rx: CBCharacteristic?
        var tx: CBCharacteristic?
        /// Assigned when the core dials, not when we connect.
        var link: UInt64?
        var reassembler: BleReassembler?
        /// Fragments waiting on `canSendWriteWithoutResponse`.
        var pending: [Data] = []
        var fingerprint: String?
        var name: String = "unnamed"
        /// Whether the core was told this peer is present; `retire` must withdraw it.
        var announced = false

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
        // Namespaced by transport: the core keys every link in one table and
        // NWTransport counts from 1 as well.
        let id = linkId(transport: transportId, counter: nextLink)
        nextLink += 1
        return id
    }

    /// Readable without constructing a manager; constructing one is what raises
    /// the permission prompt, so this doubles as the persisted opt-in.
    public static var permitted: Bool { CBManager.authorization == .allowedAlways }

    /// The one spelling of a BLE address, shared by identity reads and `dial`.
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
            // No prompt at launch: an unexplained dialog gets declined, and a
            // decline is undone only in Settings.
            push(.state(BLEDiagnostics.waitingForPermission, auth: Self.authorizationName()))
            push(.note("not permitted yet; the Devices screen offers the button that asks"))
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

    /// `ble:<peripheral identifier>`: CoreBluetooth's per-app handle, stable
    /// across launches unlike the desktop's rotating BLE address.
    public func dial(addr: String, token: UInt64) async {
        guard addr.hasPrefix(Self.addrPrefix),
            let uuid = UUID(uuidString: String(addr.dropFirst(Self.addrPrefix.count)))
        else {
            fire(.dialFailed(dial: token, reason: "not a Bluetooth address: \(addr)"))
            return
        }
        // Everything touching a Peer serialises on CoreBluetooth's queue; that
        // is what makes `@unchecked Sendable` hold.
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
            // Every transport is offered every send; not ours means nothing to do.
            guard let self, let p = self.peer(forLink: link) else { return }
            // Under didModifyServices the peripheral can be gone with no
            // callback, and a write without response would fail silently.
            guard p.peripheral.state == .connected, let rx = p.rx else {
                self.retire(p.peripheral.identifier, why: "the computer is no longer connected")
                return
            }
            // Ask for the real limit: iOS reports 512 or 20 depending on the peripheral.
            let size = p.peripheral.maximumWriteValueLength(for: .withoutResponse)
            p.pending.append(contentsOf: bleFragment(msg: msg, fragment: UInt32(size)))
            self.drain(p, rx)
        }
    }

    /// Fragments written past `canSendWriteWithoutResponse` are silently dropped.
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
            // The radio going away takes every link with it, and CoreBluetooth
            // sends no disconnect for these.
            for id in linkedPeers() {
                retire(id, why: "Bluetooth was turned \(name)")
            }
            return
        }
        startScan(c, why: "powered on")
        adoptConnected(c)
    }

    /// `allowDuplicates: false` reports each peripheral once per scan, so the
    /// scan must be restarted after every disconnect to see a desktop again.
    private func startScan(_ c: CBCentralManager, why: String) {
        c.scanForPeripherals(
            withServices: [serviceUUID],
            options: [CBCentralManagerScanOptionAllowDuplicatesKey: false]
        )
        push(.scanning(true))
        push(.note("scanning for the acrylius service: \(why)"))
    }

    /// A connected peripheral stops advertising, so after a relaunch the scan
    /// finds nothing while the old ACL link lingers. Each peripheral returned
    /// here needs a local `connect` before it is usable (Apple documents this).
    private func adoptConnected(_ c: CBCentralManager) {
        let already = c.retrieveConnectedPeripherals(withServices: [serviceUUID])
        guard !already.isEmpty else {
            push(.note("nothing already connected to iOS"))
            return
        }
        push(.note("\(already.count) already connected to iOS; reconnecting"))
        for peripheral in already {
            // Retained before the connect; see the trap list.
            let p = remember(peripheral)
            if let name = peripheral.name { p.name = name }
            beginConnect(peripheral, via: c, why: "already connected to iOS")
        }
    }

    /// Generous: also covers service and characteristic discovery on a slow link.
    private static let connectTimeout = 10.0

    /// `connect(_:options:)` pends forever with no failure callback, and a
    /// peripheral wedged in `.connecting` makes `didDiscover` ignore it for the
    /// life of the app. Cancelling returns it to `.disconnected` so the scan retries.
    private func beginConnect(_ peripheral: CBPeripheral, via c: CBCentralManager, why: String) {
        push(.note("connecting: \(why)"))
        c.connect(peripheral, options: nil)
        queue.asyncAfter(deadline: .now() + Self.connectTimeout) { [weak self, weak c] in
            guard let self, let c else { return }
            // Connected with nothing discovered (a stale attribute cache) is a
            // failure too, so require the characteristics as well.
            let p = self.peer(peripheral.identifier)
            if peripheral.state == .connected, p?.rx != nil, p?.tx != nil { return }
            if peripheral.state == .disconnected { return }
            self.push(.note("connect timed out after \(Int(Self.connectTimeout))s; retrying"))
            c.cancelPeripheralConnection(peripheral)
        }
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
        // The fingerprint doesn't fit in a 31-byte advertisement; it is read
        // over GATT after connecting.
        beginConnect(peripheral, via: c, why: "seen advertising")
    }

    public func centralManager(_ c: CBCentralManager, didConnect peripheral: CBPeripheral) {
        // Before `discoverServices`, and only from here.
        peripheral.delegate = self
        peripheral.discoverServices([serviceUUID])
        push(.note("connected; discovering services"))
        push(.trouble(nil))
    }

    public func centralManager(
        _ c: CBCentralManager, didFailToConnect peripheral: CBPeripheral, error: (any Error)?
    ) {
        let why = error?.localizedDescription ?? "unknown"
        push(.note("could not connect: \(why)"))
        if let remedy = Self.remedy(for: error) { push(.trouble(remedy)) }
        // Restart or this desktop is never reported again; see startScan.
        startScan(c, why: "a connection failed")
    }

    /// Bond mismatches between the two stacks stall every connection and only
    /// Settings on the phone clears them; CoreBluetooth's messages never say so.
    /// Nothing in this app's GATT tree asks for encryption, so any bond is leftover.
    static func remedy(for error: (any Error)?) -> String? {
        guard let code = (error as? CBError)?.code else { return nil }
        switch code {
        case .peerRemovedPairingInformation, .encryptionTimedOut:
            return """
                This iPhone still holds Bluetooth pairing for that computer and \
                the computer no longer does, so it refuses the connection. Open \
                Settings › Bluetooth, tap the ⓘ beside it and choose Forget This \
                Device, then come back here.
                """
        case .tooManyLEPairedDevices:
            return """
                This iPhone has as many paired Bluetooth devices as it allows. \
                Forget one you no longer use in Settings › Bluetooth.
                """
        default:
            return nil
        }
    }

    public func centralManager(
        _ c: CBCentralManager, didDisconnectPeripheral peripheral: CBPeripheral,
        error: (any Error)?
    ) {
        let why = error?.localizedDescription
        push(.link("none"))
        push(.note("disconnected\(why.map { ": \($0)" } ?? "")"))
        // A bond can fail on the way out as well as on the way in.
        if let remedy = Self.remedy(for: error) { push(.trouble(remedy)) }
        // Restart or this desktop is never reported again; see startScan.
        if c.state == .poweredOn {
            startScan(c, why: "after a disconnect")
        }
        retire(peripheral.identifier, why: why)
    }

    /// Snapshotted and returned rather than retired in place: `retire` takes
    /// the same lock and fires into the core, which deadlocks if held here.
    private func linkedPeers() -> [UUID] {
        lock.lock(); defer { lock.unlock() }
        return peers.compactMap { $0.value.link == nil ? nil : $0.key }
    }

    /// Tell the core exactly once, however many paths report the same death:
    /// whoever clears the link record is the one who reports it.
    @discardableResult
    private func retire(_ id: UUID, why: String?) -> Bool {
        lock.lock()
        let p = peers[id]
        let link = p?.link
        let wasAnnounced = p?.announced ?? false
        p?.announced = false
        p?.link = nil
        p?.rx = nil
        p?.tx = nil
        p?.reassembler = nil
        p?.pending.removeAll()
        lock.unlock()
        // Before the link check: a machine is announced when its identity is
        // read, whether or not the core ever opened a link to it.
        if wasAnnounced {
            fire(.undiscovered(transport: transportId, addr: "\(Self.addrPrefix)\(id.uuidString)"))
        }
        guard let link else { return false }
        fire(
            .linkDown(
                link: link,
                reason: why.map { FfiLinkDown.transport(detail: $0) } ?? .closed))
        return true
    }
}

// MARK: - peripheral

extension BLETransport: CBPeripheralDelegate {
    /// A stopped daemon looks like this and nothing else: bluetoothd keeps the
    /// ACL, there is no disconnect, and writes without response report no error.
    /// Rediscover rather than hang up — a restarted daemon is back within seconds.
    public func peripheral(
        _ peripheral: CBPeripheral, didModifyServices invalidatedServices: [CBService]
    ) {
        guard invalidatedServices.contains(where: { $0.uuid == serviceUUID }) else { return }
        push(.note("the acrylius service went away on the computer"))
        push(.link("none"))
        retire(peripheral.identifier, why: "the service went away")
        if peripheral.state == .connected {
            peripheral.discoverServices([serviceUUID])
        }
    }

    public func peripheral(_ peripheral: CBPeripheral, didDiscoverServices error: (any Error)?) {
        if let error {
            push(.note("service discovery failed: \(error.localizedDescription)"))
            return
        }
        guard let service = peripheral.services?.first(where: { $0.uuid == serviceUUID }) else {
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
            p.announced = true
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
            // Carrying on would feed the core torn messages; drop the link.
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

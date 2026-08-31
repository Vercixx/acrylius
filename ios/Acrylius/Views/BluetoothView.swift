//  Bluetooth diagnostics. Sideloaded builds have no console, so this screen
//  answers what a `print` would.

import SwiftUI

struct BluetoothView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        List {
            if let trouble = model.ble.trouble {
                Section {
                    Label {
                        Text(trouble)
                    } icon: {
                        Image(systemName: "exclamationmark.triangle.fill")
                            .foregroundStyle(.orange)
                    }
                    .font(.callout)
                }
            }

            Section {
                LabeledContent("State", value: model.ble.managerState)
                LabeledContent("Permission", value: model.ble.authorization)
                LabeledContent("Scanning", value: model.ble.scanning ? "yes" : "no")
                LabeledContent("Link", value: model.ble.link)
                if let f = model.ble.fragmentBytes {
                    LabeledContent("Fragment", value: "\(f) bytes")
                }
            } header: {
                Text("Radio")
            }

            Section {
                if model.ble.sightings.isEmpty {
                    ContentUnavailableView(
                        "No services detected",
                        systemImage: "dot.radiowaves.left.and.right",
                        description: Text(
                            "The computer advertises only while acryliusd is running "
                                + "and \"ble.enabled\" is on."
                        )
                    )
                } else {
                    ForEach(model.ble.sightings) { s in
                        VStack(alignment: .leading, spacing: 2) {
                            HStack {
                                Text(s.name)
                                Spacer()
                                Text("\(s.rssi) dBm").font(.caption.monospaced())
                                    .foregroundStyle(.secondary)
                            }
                            // A service in the GATT database but not the
                            // advertisement is invisible to a filtered scan.
                            Text(
                                s.advertisedOurService
                                    ? "advertises acrylius"
                                    : "doesn't advertise acrylius"
                            )
                            .font(.caption)
                            .foregroundStyle(s.advertisedOurService ? .green : .secondary)
                        }
                    }
                }
            } header: {
                Text("Detected BLE services")
            }

            Section {
                ForEach(model.ble.notes.reversed()) { n in
                    HStack(alignment: .firstTextBaseline) {
                        Text(n.at, format: .dateTime.hour().minute().second())
                            .font(.caption2.monospaced())
                            .foregroundStyle(.secondary)
                        Text(n.text).font(.caption)
                    }
                }
            } header: {
                Text("CoreBluetooth log")
            } footer: {
                Text("You can copy this by pressing the copy button on top.")
            }
        }
        .navigationTitle("Bluetooth")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                Button {
                    UIPasteboard.general.string = model.ble.transcript()
                } label: {
                    Label("Copy", systemImage: "doc.on.doc")
                }
            }
        }
        .task { model.startBluetooth() }
    }
}

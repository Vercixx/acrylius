//
//  What Bluetooth is doing, on the phone that is doing it.
//
//  This screen is not a feature. It exists because the app reaches a device
//  through a CI build and a sideload, with no console attached — so every
//  question that would be answered by a `print` costs a full build cycle
//  instead. What is on this screen is exactly the set of things worth a build
//  cycle: the manager's state, whether permission was ever granted, what the
//  scan can see, and whether the advertisement carried the service we filter on.
//

import SwiftUI

struct BluetoothView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        List {
            // First, and unmissable. Everything else on this screen describes a
            // state; this one names the step that changes it, and it is no use
            // to anybody buried under the notes.
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
            } footer: {
                // Each of these is a different problem with a different fix, and
                // they are indistinguishable from the outside.
                Text(
                    "\"Not permitted\" can only be undone in Settings. "
                        + "\"Bluetooth is off\" is Control Centre. "
                        + "\"No Bluetooth on this device\" means a simulator."
                )
            }

            Section {
                if model.ble.sightings.isEmpty {
                    ContentUnavailableView(
                        "Nothing yet",
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
                            // The distinction that matters most: a service in a
                            // peripheral's GATT database but missing from its
                            // advertisement is invisible to a filtered scan, and
                            // that is the likeliest reason to find nothing.
                            Text(
                                s.advertisedOurService
                                    ? "advertises the acrylius service"
                                    : "does not advertise it"
                            )
                            .font(.caption)
                            .foregroundStyle(s.advertisedOurService ? .green : .secondary)
                        }
                    }
                }
            } header: {
                Text("Seen")
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
                Text("Recent")
            } footer: {
                // A screenshot of a scrolling list is a poor bug report.
                Text("Newest first. Copy the whole thing with the button above.")
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

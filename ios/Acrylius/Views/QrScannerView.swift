#if canImport(SwiftUI) && canImport(VisionKit)

import SwiftUI
import VisionKit

/// A camera that reads one QR and stops.
///
/// `DataScannerViewController` rather than an `AVCaptureSession` built by hand:
/// it is iOS 16 and up, so it clears this app's floor, and it brings the
/// viewfinder, the highlight around what it found and pinch-to-zoom with it.
/// None of that is work worth doing again for a screen somebody sees once per
/// computer they own.
///
/// It is a `UIViewControllerRepresentable` because VisionKit has no SwiftUI
/// form. The wrapper does one thing beyond hosting: it stops scanning the
/// instant it has an answer, so a code held in frame cannot fire twice and
/// start two pairings.
struct QrScannerView: View {
    /// Called once, with the payload.
    let found: (String) -> Void

    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            Group {
                if DataScannerViewController.isSupported,
                   DataScannerViewController.isAvailable {
                    Scanner(found: found)
                } else {
                    // Both of these are real and different: a simulator is not
                    // supported at all, and a device can be unavailable
                    // because the camera is in use or the permission was
                    // refused. Neither is fixable from here, and pretending
                    // otherwise with a spinner is worse than saying so.
                    ContentUnavailableView(
                        "No camera to scan with",
                        systemImage: "camera.slash",
                        description: Text(
                            "Type the code from the computer instead, or allow camera access in Settings.")
                    )
                }
            }
            .ignoresSafeArea(edges: .bottom)
            .navigationTitle("Scan")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
            }
        }
    }
}

private struct Scanner: UIViewControllerRepresentable {
    let found: (String) -> Void

    func makeCoordinator() -> Coordinator { Coordinator(found: found) }

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let c = DataScannerViewController(
            // Only QR. A pairing payload is never a barcode or a line of text,
            // and every other symbology this could recognise is one more thing
            // to hold in frame by accident.
            recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .balanced,
            recognizesMultipleItems: false,
            isHighFrameRateTrackingEnabled: false,
            isGuidanceEnabled: true,
            isHighlightingEnabled: true
        )
        c.delegate = context.coordinator
        try? c.startScanning()
        return c
    }

    func updateUIViewController(_: DataScannerViewController, context _: Context) {}

    static func dismantleUIViewController(_ c: DataScannerViewController, coordinator _: Coordinator) {
        // The camera keeps running otherwise, behind whatever is on screen next.
        c.stopScanning()
    }

    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        private let found: (String) -> Void
        /// One answer per presentation. Without this a code left in frame
        /// fires on every recognition and starts a second pairing under the
        /// first one.
        private var done = false

        init(found: @escaping (String) -> Void) { self.found = found }

        func dataScanner(
            _ scanner: DataScannerViewController,
            didAdd addedItems: [RecognizedItem],
            allItems _: [RecognizedItem]
        ) {
            guard !done else { return }
            for item in addedItems {
                guard case let .barcode(code) = item, let text = code.payloadStringValue else {
                    continue
                }
                done = true
                scanner.stopScanning()
                found(text)
                return
            }
        }
    }
}

#endif

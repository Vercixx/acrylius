import SwiftUI

// Liquid Glass where the system has it, a material where it does not.
//
// The app's floor is iOS 17 and `glassEffect` is iOS 26, so without this every
// glass surface carries its own `if #available` and its own fallback. Written
// once, the branch is one thing to get right, one thing to test, and one thing
// to delete when the floor moves.
//
// Standard controls adopt Liquid Glass by themselves once the app is built
// against the iOS 26 SDK — that is a property of the SDK, not of any code here.
// This is only for the surfaces the app draws itself.
//
// Nothing calls it yet. It lands with the toolchain rather than with the views
// that need it, because `ios/Acrylius/Views` is compiled by no local gate: the
// macOS runner is the first thing that will ever type-check this file, and the
// cheapest run to learn that on is one that is about the toolchain anyway.

private struct AcrylicGlass<S: Shape>: ViewModifier {
    let shape: S
    /// Whether the surface reacts to touch. Glass only — the fallback has no
    /// equivalent and ignores it.
    let interactive: Bool

    func body(content: Content) -> some View {
        if #available(iOS 26, *) {
            content.glassEffect(interactive ? .regular.interactive() : .regular, in: shape)
        } else {
            // The closest iOS 17 has: it blurs and it takes a shape. It does
            // not refract and it does not answer a touch. Faking the rest with
            // a shadow reads as a mistake rather than as an older OS, so this
            // stays deliberately plain.
            content.background(.ultraThinMaterial, in: shape)
        }
    }
}

extension View {
    /// A Liquid Glass surface, falling back to a material below iOS 26.
    ///
    /// The shape defaults to a capsule because that is what `glassEffect` uses
    /// and matching it keeps the two eras looking like the same layout.
    func acrylicGlass(
        in shape: some Shape = .capsule,
        interactive: Bool = false
    ) -> some View {
        modifier(AcrylicGlass(shape: shape, interactive: interactive))
    }
}

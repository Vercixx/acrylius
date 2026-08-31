import SwiftUI

// Liquid Glass where the system has it (iOS 26+), a material fallback below that.
// Only for surfaces the app draws itself; standard controls adopt it via the SDK.

private struct AcrylicGlass<S: Shape>: ViewModifier {
    let shape: S
    /// Glass only; the fallback ignores this.
    let interactive: Bool

    func body(content: Content) -> some View {
        if #available(iOS 26, *) {
            content.glassEffect(interactive ? .regular.interactive() : .regular, in: shape)
        } else {
            // Closest iOS 17 has: blurs and takes a shape, no refraction, no touch response.
            content.background(.ultraThinMaterial, in: shape)
        }
    }
}

extension View {
    /// A Liquid Glass surface, falling back to a material below iOS 26.
    /// Defaults to a capsule shape to match `glassEffect`'s default.
    func acrylicGlass(
        in shape: some Shape = .capsule,
        interactive: Bool = false
    ) -> some View {
        modifier(AcrylicGlass(shape: shape, interactive: interactive))
    }
}

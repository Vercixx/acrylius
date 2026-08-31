//! The bindings generator, pinned to the exact `uniffi` version this crate
//! links; a mismatched bindgen produces bindings that compile and then misbehave.

fn main() {
    uniffi::uniffi_bindgen_main();
}

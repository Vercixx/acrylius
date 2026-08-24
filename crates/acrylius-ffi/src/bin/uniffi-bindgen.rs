//! The bindings generator, pinned to the exact `uniffi` version this crate
//! links. Running a mismatched `uniffi-bindgen` from `cargo install` against a
//! different runtime is a well-known way to get bindings that compile and then
//! misbehave; building it here makes the two impossible to skew.

fn main() {
    uniffi::uniffi_bindgen_main();
}

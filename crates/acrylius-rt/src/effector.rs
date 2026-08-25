//! The platform half of a plugin, for a Rust host.

use acrylius_core::vocab::{Effect, EffectKind, EffectResult};

#[async_trait::async_trait]
pub trait Effector: Send + Sync + 'static {
    /// Which effect kinds this host can actually carry out.
    ///
    /// The core drops plugins whose requirements are unmet and never advertises
    /// their capabilities, so this list is what decides the device's feature
    /// set. Not a `#[cfg]`, and not a config file.
    fn supported(&self) -> Vec<EffectKind>;

    async fn run(&self, effect: Effect) -> EffectResult;
}

/// A host that can do nothing. Useful in tests and for a headless client.
pub struct NullEffector;

#[async_trait::async_trait]
impl Effector for NullEffector {
    fn supported(&self) -> Vec<EffectKind> {
        Vec::new()
    }
    async fn run(&self, _effect: Effect) -> EffectResult {
        EffectResult::Unsupported
    }
}

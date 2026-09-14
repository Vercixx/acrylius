//! What this machine can actually do.
//!
//! [`LinuxEffector::supported`] decides the feature set at runtime, not via
//! `#[cfg]`: a plugin with missing effects is dropped and never advertised.

use acrylius_core::plugins::command::Exited;
use acrylius_core::vocab::{Effect, EffectKind, EffectResult};
use acrylius_rt::effector::Effector;

use crate::command::CommandCatalog;
use crate::{clipboard, command, media, session, wol};

/// Where this machine's wake targets live.
#[derive(Clone, Debug, Default)]
pub struct WolSettings {
    pub allowlist: Vec<String>,
    pub broadcast: String,
    pub port: u16,
}

pub struct LinuxEffector {
    session: Option<session::SessionEffector>,
    media: Option<media::MediaEffector>,
    catalog: CommandCatalog,
    wol: WolSettings,
    has_wayland: bool,
    touchpad: Option<crate::touchpad::TouchpadEffector>,
    run_counter: std::sync::atomic::AtomicU32,
}

impl LinuxEffector {
    pub async fn new(
        catalog: CommandCatalog,
        wol: WolSettings,
        session_commands: session::Commands,
    ) -> Self {
        // No system bus or no graphical session is a normal config, not an error.
        let session = match session::SessionEffector::new(session_commands).await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::debug!(error = %e, "no logind; session control is off");
                None
            }
        };
        // Session bus, not system: players are per-login, and a headless
        // machine having none to control is normal, not a failure.
        let media = match media::MediaEffector::new().await {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::debug!(error = %e, "no session bus; media control is off");
                None
            }
        };
        let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        if !has_wayland {
            tracing::debug!("no WAYLAND_DISPLAY; clipboard is off");
        }
        // Usually a permission problem: /dev/uinput is root:input, and this
        // needs the `input` group and BindPaths= past the unit's private /dev.
        let touchpad = crate::touchpad::available().then(Default::default);
        if touchpad.is_none() {
            tracing::debug!("cannot open /dev/uinput; the touchpad is off");
        }
        Self {
            session,
            media,
            catalog,
            wol,
            has_wayland,
            touchpad,
            run_counter: std::sync::atomic::AtomicU32::new(0),
        }
    }

    #[must_use]
    pub fn catalog(&self) -> &CommandCatalog {
        &self.catalog
    }

    #[must_use]
    pub fn wol_settings(&self) -> &WolSettings {
        &self.wol
    }

    fn encode<T: minicbor::Encode<()>>(value: &T) -> EffectResult {
        match minicbor::to_vec(value) {
            Ok(bytes) => EffectResult::Ok(bytes),
            Err(e) => EffectResult::Failed(format!("could not encode a reply: {e}")),
        }
    }
}

#[async_trait::async_trait]
impl Effector for LinuxEffector {
    fn supported(&self) -> Vec<EffectKind> {
        let mut kinds = vec![EffectKind::Wol];
        if self.session.is_some() {
            kinds.push(EffectKind::Session);
        }
        if self.has_wayland {
            kinds.push(EffectKind::Clipboard);
        }
        // Empty catalog means the capability is absent, not present-but-empty.
        if !self.catalog.is_empty() {
            kinds.push(EffectKind::Command);
        }
        // Offered whenever there's a session bus, not only while something
        // plays, or the phone would renegotiate constantly.
        if self.media.is_some() {
            kinds.push(EffectKind::Media);
        }
        if self.touchpad.is_some() {
            kinds.push(EffectKind::Touchpad);
        }
        kinds
    }

    async fn run(&self, effect: Effect) -> EffectResult {
        match effect {
            Effect::QuerySession | Effect::LockSession | Effect::UnlockSession => {
                let Some(s) = &self.session else {
                    return EffectResult::Unsupported;
                };
                match effect {
                    Effect::QuerySession => match s.query().await {
                        Ok(state) => Self::encode(&state),
                        Err(e) => EffectResult::Failed(e.to_string()),
                    },
                    Effect::LockSession => match s.lock().await {
                        Ok(o) => Self::encode(&o),
                        Err(e) => EffectResult::Failed(e.to_string()),
                    },
                    _ => match s.unlock().await {
                        Ok(o) => Self::encode(&o),
                        Err(e) => EffectResult::Failed(e.to_string()),
                    },
                }
            }

            Effect::ClipboardRead => {
                if !self.has_wayland {
                    return EffectResult::Unsupported;
                }
                match clipboard::read().await {
                    Ok(Some(data)) => EffectResult::Ok(data),
                    // Empty clipboard isn't a failure; nothing to hand over.
                    Ok(None) => EffectResult::Ok(Vec::new()),
                    Err(e) => EffectResult::Failed(e.to_string()),
                }
            }

            Effect::ClipboardWrite { data, .. } => {
                if !self.has_wayland {
                    return EffectResult::Unsupported;
                }
                match clipboard::write(data).await {
                    Ok(()) => EffectResult::Ok(Vec::new()),
                    Err(e) => EffectResult::Failed(e.to_string()),
                }
            }

            Effect::ListCommands => Self::encode(&acrylius_core::plugins::command::CommandList {
                commands: self.catalog.manifest(),
            }),

            Effect::RunCommand { id } => {
                // Re-checked here: this layer starts the process and
                // shouldn't trust the caller.
                let Some(spec) = self.catalog.get(&id) else {
                    return EffectResult::Failed(format!("{id:?} is not a configured command"));
                };
                let run_id = self
                    .run_counter
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    .wrapping_add(1);
                match command::run(spec, run_id).await {
                    Ok(exited) => Self::encode(&exited),
                    Err(e) => EffectResult::Failed(e.to_string()),
                }
            }

            Effect::SendMagicPacket { macs, dests, port } => {
                match wol::send(&macs, &dests, port).await {
                    Ok(n) => Self::encode(&Exited {
                        run_id: 0,
                        code: 0,
                        truncated: n == 0,
                    }),
                    Err(e) => EffectResult::Failed(e.to_string()),
                }
            }

            Effect::MediaQuery => match &self.media {
                Some(m) => Self::encode(&m.state().await),
                None => EffectResult::Unsupported,
            },

            Effect::MediaControl { player, action } => match &self.media {
                // Reply carries the settled state, not a bare ack: a player
                // may ignore, clamp, or stop the command, and only re-reading says which.
                Some(m) => match m.control_and_settle(&player, action).await {
                    Ok(state) => Self::encode(&state),
                    Err(e) => EffectResult::Failed(e.to_string()),
                },
                None => EffectResult::Unsupported,
            },

            Effect::Touchpad { order, op } => match &self.touchpad {
                Some(t) => match t.run(order, op).await {
                    Ok(()) => EffectResult::Ok(Vec::new()),
                    Err(e) => EffectResult::Failed(e),
                },
                None => EffectResult::Unsupported,
            },

            // Unknown namespace: `Unsupported` lets the core drop the plugin quietly.
            Effect::Custom { ns, verb, .. } => {
                tracing::debug!(ns, verb, "unknown custom effect");
                EffectResult::Unsupported
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn a_machine_with_no_commands_does_not_offer_the_capability() {
        let e = LinuxEffector::new(
            CommandCatalog::default(),
            WolSettings::default(),
            session::Commands::default(),
        )
        .await;
        assert!(
            !e.supported().contains(&EffectKind::Command),
            "an empty catalog means the capability is absent, not empty"
        );
        // And asking anyway is refused rather than run.
        let r = e
            .run(Effect::RunCommand {
                id: "anything".to_string(),
            })
            .await;
        assert!(matches!(r, EffectResult::Failed(_)));
    }

    #[tokio::test]
    async fn waking_is_always_offered() {
        // A UDP send needs nothing from the desktop, so headless machines relay too.
        let e = LinuxEffector::new(
            CommandCatalog::default(),
            WolSettings::default(),
            session::Commands::default(),
        )
        .await;
        assert!(e.supported().contains(&EffectKind::Wol));
    }

    #[tokio::test]
    async fn a_configured_catalog_turns_the_capability_on() {
        let mut entries = BTreeMap::new();
        entries.insert(
            "true".to_string(),
            crate::command::CommandSpec {
                name: "No-op".to_string(),
                program: "/bin/true".to_string(),
                args: Vec::new(),
                needs_confirm: false,
                timeout_secs: None,
            },
        );
        let e = LinuxEffector::new(
            CommandCatalog::new(entries),
            WolSettings::default(),
            session::Commands::default(),
        )
        .await;
        assert!(e.supported().contains(&EffectKind::Command));
        assert!(matches!(
            e.run(Effect::RunCommand {
                id: "true".to_string()
            })
            .await,
            EffectResult::Ok(_)
        ));
    }
}

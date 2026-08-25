//! `org.acrylius.command/1`: run a named command on the computer.
//!
//! One rule makes this a command runner and not a remote shell. The wire carries
//! an `id` from the computer's own configuration, never a command string.
//!
//! An id that is not in the allowlist is refused. Everything about what actually
//! runs (the absolute path, the argv vector, the timeout, the output cap) lives
//! on the machine that will run it, where the person who owns that machine put
//! it. A peer cannot influence any of it beyond choosing from the list.

use std::collections::BTreeMap;

use crate::plugin::{Cx, Plugin, PluginError, PluginManifest};
use crate::proto::envelope::Envelope;
use crate::proto::ids::DeviceId;
use crate::vocab::{Effect, EffectKind, EffectResult, EffectToken, UiEvent};

pub const CAP: &str = "org.acrylius.command/1";

#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct CommandEntry {
    #[n(0)]
    pub id: String,
    #[n(1)]
    pub name: String,
    /// A hint for the user interface. It is not enforcement: a peer that
    /// ignores it still only reaches an allowlisted id.
    #[n(2)]
    pub needs_confirm: bool,
}

#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct CommandList {
    #[n(0)]
    pub commands: Vec<CommandEntry>,
}

#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct RunRequest {
    #[n(0)]
    pub id: String,
}

#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct Exited {
    #[n(0)]
    pub run_id: u32,
    #[n(1)]
    pub code: i32,
    /// Set when the output cap was reached, so a reader knows it is not seeing
    /// everything.
    #[n(2)]
    pub truncated: bool,
}

static MANIFEST: PluginManifest = PluginManifest {
    id: "org.acrylius.command",
    outgoing: &[CAP],
    incoming: &[CAP],
    requires: &[EffectKind::Command],
};

#[derive(Default)]
pub struct CommandPlugin {
    /// What this machine is willing to run. Empty means nothing.
    catalog: Vec<CommandEntry>,
    /// What each peer told us it is willing to run.
    ///
    /// A catalogue arrives unprompted when a peer connects, which is the right
    /// time to send it and the wrong time for anyone to be listening. Keeping
    /// it means a user interface that opens later can still show the list
    /// without a round trip.
    remote: BTreeMap<DeviceId, Vec<CommandEntry>>,
    pending: BTreeMap<EffectToken, (DeviceId, u32)>,
}

impl CommandPlugin {
    #[must_use]
    pub fn new(catalog: Vec<CommandEntry>) -> Self {
        Self {
            catalog,
            remote: BTreeMap::new(),
            pending: BTreeMap::new(),
        }
    }
}

impl Plugin for CommandPlugin {
    fn manifest(&self) -> &'static PluginManifest {
        &MANIFEST
    }

    fn on_peer_connected(&mut self, cx: &mut Cx, peer: &DeviceId) {
        if self.catalog.is_empty() {
            return;
        }
        let list = CommandList {
            commands: self.catalog.clone(),
        };
        if let Ok(body) = minicbor::to_vec(&list) {
            cx.send(peer, CAP, "list", body);
        }
    }

    fn on_message(
        &mut self,
        cx: &mut Cx,
        peer: &DeviceId,
        env: &Envelope<'_>,
    ) -> Result<(), PluginError> {
        match env.ty {
            "run" => {
                let req: RunRequest =
                    minicbor::decode(env.body).map_err(|_| PluginError::BadBody)?;
                // The only decision this plugin makes, and the one that matters.
                if !self.catalog.iter().any(|c| c.id == req.id) {
                    return Err(PluginError::NotAllowed);
                }
                let token = cx.effect(Effect::RunCommand { id: req.id });
                self.pending.insert(token, (peer.clone(), env.id));
                Ok(())
            }
            "list" => {
                if let Ok(list) = minicbor::decode::<CommandList>(env.body) {
                    self.remote.insert(peer.clone(), list.commands);
                }
                cx.ui(UiEvent::Plugin {
                    peer: peer.clone(),
                    cap: CAP.to_string(),
                    ty: "list".to_string(),
                    body: env.body.to_vec(),
                });
                Ok(())
            }
            "started" | "output" | "exited" | "ok" => {
                cx.ui(UiEvent::Plugin {
                    peer: peer.clone(),
                    cap: CAP.to_string(),
                    ty: env.ty.to_string(),
                    body: env.body.to_vec(),
                });
                Ok(())
            }
            other => Err(PluginError::UnknownType(other.to_string())),
        }
    }

    fn on_local(
        &mut self,
        cx: &mut Cx,
        peer: &DeviceId,
        ty: &str,
        body: &[u8],
    ) -> Result<(), PluginError> {
        match ty {
            "run" => {
                cx.send(peer, CAP, "run", body.to_vec());
                Ok(())
            }
            // Answer from what the peer already told us, rather than asking
            // again for something that does not change.
            "list" => {
                let commands = self.remote.get(peer).cloned().unwrap_or_default();
                let Ok(encoded) = minicbor::to_vec(CommandList { commands }) else {
                    return Err(PluginError::Internal("encode failed".to_string()));
                };
                cx.ui(UiEvent::Plugin {
                    peer: peer.clone(),
                    cap: CAP.to_string(),
                    ty: "list".to_string(),
                    body: encoded,
                });
                Ok(())
            }
            other => Err(PluginError::UnknownType(other.to_string())),
        }
    }

    fn on_effect_result(&mut self, cx: &mut Cx, token: EffectToken, result: &EffectResult) {
        let Some((peer, request)) = self.pending.remove(&token) else {
            return;
        };
        match result {
            // The host returns an encoded `Exited`. Output streaming is a
            // version 2 concern; version 1 answers once, when it is over.
            EffectResult::Ok(bytes) => cx.send_reply(&peer, CAP, "exited", bytes.clone(), request),
            EffectResult::Failed(detail) => {
                cx.send_error(&peer, CAP, request, "effect_failed", detail);
            }
            EffectResult::Unsupported => {
                cx.send_error(&peer, CAP, request, "not_allowed", "cannot run commands");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::harness::{envelope, run};

    fn peer() -> DeviceId {
        DeviceId::of(&[4u8; 32])
    }

    fn plugin() -> CommandPlugin {
        CommandPlugin::new(vec![CommandEntry {
            id: "screenshot".to_string(),
            name: "Take a screenshot".to_string(),
            needs_confirm: false,
        }])
    }

    #[test]
    fn the_catalog_is_offered_on_connect() {
        let mut p = plugin();
        let r = run(0, |cx| p.on_peer_connected(cx, &peer()));
        let sent = r.sent("list").expect("a catalog");
        let list: CommandList = minicbor::decode(&sent.body).unwrap();
        assert_eq!(list.commands.len(), 1);
        assert_eq!(list.commands[0].id, "screenshot");
    }

    #[test]
    fn a_machine_with_no_commands_offers_nothing() {
        let mut p = CommandPlugin::default();
        let r = run(0, |cx| p.on_peer_connected(cx, &peer()));
        assert!(r.sends.is_empty());
    }

    #[test]
    fn running_a_listed_command_reaches_the_host() {
        let mut p = plugin();
        let body = minicbor::to_vec(RunRequest {
            id: "screenshot".to_string(),
        })
        .unwrap();
        let env = envelope(11, CAP, "run", &body);
        let r = run(0, |cx| p.on_message(cx, &peer(), &env).unwrap());
        assert_eq!(
            r.one_effect(),
            &Effect::RunCommand {
                id: "screenshot".to_string()
            }
        );

        let exited = Exited {
            run_id: 1,
            code: 0,
            truncated: false,
        };
        let r2 = run(r.next_token, |cx| {
            p.on_effect_result(
                cx,
                r.token(),
                &EffectResult::Ok(minicbor::to_vec(&exited).unwrap()),
            );
        });
        let sent = r2.sent("exited").expect("an outcome");
        assert_eq!(sent.re, Some(11));
        assert_eq!(minicbor::decode::<Exited>(&sent.body).unwrap(), exited);
    }

    #[test]
    fn an_id_that_is_not_in_the_catalog_never_reaches_the_host() {
        // This is the whole difference between a command runner and a remote
        // shell, so it is checked before anything else can happen.
        let mut p = plugin();
        for id in [
            "rm",
            "screenshot ; rm -rf /",
            "",
            "SCREENSHOT",
            "../screenshot",
        ] {
            let body = minicbor::to_vec(RunRequest { id: id.to_string() }).unwrap();
            let env = envelope(1, CAP, "run", &body);
            let r = run(0, |cx| {
                assert_eq!(
                    p.on_message(cx, &peer(), &env).unwrap_err(),
                    PluginError::NotAllowed,
                    "{id:?} should be refused"
                );
            });
            assert!(r.effects.is_empty(), "{id:?} must not reach the host");
        }
    }

    #[test]
    fn a_machine_offering_no_commands_refuses_everything() {
        let mut p = CommandPlugin::default();
        let body = minicbor::to_vec(RunRequest {
            id: "anything".to_string(),
        })
        .unwrap();
        let env = envelope(1, CAP, "run", &body);
        run(0, |cx| {
            assert_eq!(
                p.on_message(cx, &peer(), &env).unwrap_err(),
                PluginError::NotAllowed
            );
        });
    }

    #[test]
    fn a_body_that_is_not_a_run_request_is_refused() {
        let mut p = plugin();
        let env = envelope(1, CAP, "run", b"\xff\xff not cbor");
        run(0, |cx| {
            assert_eq!(
                p.on_message(cx, &peer(), &env).unwrap_err(),
                PluginError::BadBody
            );
        });
    }
}

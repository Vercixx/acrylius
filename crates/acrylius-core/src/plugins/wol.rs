//! `org.acrylius.wol/1`: wake a sleeping computer.
//!
//! Almost all of the interesting behaviour involves no messages at all. A
//! sleeping computer is not running this daemon, so nothing on it can receive
//! "please wake up", so the phone sends the magic packet. What travels over the
//! session, while the computer is still awake, is the information the phone will
//! need later.
//!
//! `relay` exists only to ask a computer that is awake to wake a different one.
//! It is allowlisted, because otherwise it would be an open UDP relay.

use std::collections::BTreeMap;

use crate::plugin::{Cx, Plugin, PluginError, PluginManifest};
use crate::proto::envelope::Envelope;
use crate::proto::ids::DeviceId;
use crate::vocab::{Effect, EffectKind, EffectResult, EffectToken, UiEvent};

pub const CAP: &str = "org.acrylius.wol/1";

/// What a phone needs in order to wake this machine later.
#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct WolConfig {
    #[n(0)]
    pub macs: Vec<String>,
    #[n(1)]
    pub broadcast: String,
    #[n(2)]
    pub port: u16,
    /// The address to try first.
    ///
    /// A network interface matches a magic packet by its payload, not its
    /// destination address, so a unicast datagram wakes the machine just as
    /// well as a broadcast. That matters because iOS gates UDP broadcast behind
    /// an entitlement a free developer account cannot get.
    #[n(3)]
    pub last_ipv4: String,
}

#[derive(Clone, PartialEq, Eq, Debug, Default, minicbor::Encode, minicbor::Decode)]
pub struct RelayRequest {
    #[n(0)]
    pub mac: String,
}

static MANIFEST: PluginManifest = PluginManifest {
    id: "org.acrylius.wol",
    outgoing: &[CAP],
    incoming: &[CAP],
    requires: &[EffectKind::Wol],
};

pub struct WolPlugin {
    config: WolConfig,
    /// MACs this machine will relay a wake to. Anything else is refused.
    allowlist: Vec<String>,
    pending: BTreeMap<EffectToken, (DeviceId, u32)>,
}

impl WolPlugin {
    #[must_use]
    pub fn new(config: WolConfig, allowlist: Vec<String>) -> Self {
        Self {
            config,
            allowlist,
            pending: BTreeMap::new(),
        }
    }
}

impl Default for WolPlugin {
    fn default() -> Self {
        Self::new(WolConfig::default(), Vec::new())
    }
}

fn normalize_mac(m: &str) -> String {
    m.chars()
        .filter(char::is_ascii_hexdigit)
        .flat_map(char::to_lowercase)
        .collect()
}

impl Plugin for WolPlugin {
    fn manifest(&self) -> &'static PluginManifest {
        &MANIFEST
    }

    fn on_peer_connected(&mut self, cx: &mut Cx, peer: &DeviceId) {
        // Send it unprompted: by the time the phone wants it, this machine is
        // asleep and cannot answer a question.
        if self.config.macs.is_empty() {
            return;
        }
        if let Ok(body) = minicbor::to_vec(&self.config) {
            cx.send(peer, CAP, "config", body);
        }
    }

    fn on_message(
        &mut self,
        cx: &mut Cx,
        peer: &DeviceId,
        env: &Envelope<'_>,
    ) -> Result<(), PluginError> {
        match env.ty {
            "ok" | "err" => {
                cx.ui(UiEvent::Plugin {
                    peer: peer.clone(),
                    cap: CAP.to_string(),
                    ty: env.ty.to_string(),
                    body: env.body.to_vec(),
                });
                Ok(())
            }
            "config" => {
                cx.ui(UiEvent::Plugin {
                    peer: peer.clone(),
                    cap: CAP.to_string(),
                    ty: "config".to_string(),
                    body: env.body.to_vec(),
                });
                Ok(())
            }
            "relay" => {
                let req: RelayRequest =
                    minicbor::decode(env.body).map_err(|_| PluginError::BadBody)?;
                let wanted = normalize_mac(&req.mac);
                if wanted.len() != 12 {
                    return Err(PluginError::BadBody);
                }
                // Allowlisted only. Without this the endpoint sprays UDP
                // wherever a peer asks it to.
                if !self.allowlist.iter().any(|m| normalize_mac(m) == wanted) {
                    return Err(PluginError::NotAllowed);
                }
                let token = cx.effect(Effect::SendMagicPacket {
                    macs: vec![req.mac],
                    dests: vec![self.config.broadcast.clone()],
                    port: self.config.port,
                });
                self.pending.insert(token, (peer.clone(), env.id));
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
            // Ask a peer that is awake to wake a third machine.
            "relay" => {
                cx.send(peer, CAP, "relay", body.to_vec());
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
            EffectResult::Ok(_) => cx.send_reply(&peer, CAP, "ok", Vec::new(), request),
            EffectResult::Failed(detail) => {
                cx.send_error(&peer, CAP, request, "effect_failed", detail);
            }
            EffectResult::Unsupported => {
                cx.send_error(&peer, CAP, request, "not_allowed", "cannot send packets");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::harness::{envelope, run};

    fn peer() -> DeviceId {
        DeviceId::of(&[3u8; 32])
    }

    fn plugin() -> WolPlugin {
        WolPlugin::new(
            WolConfig {
                macs: vec!["00:11:22:33:44:55".to_string()],
                broadcast: "192.168.1.255".to_string(),
                port: 9,
                last_ipv4: "192.168.1.50".to_string(),
            },
            vec!["aa-bb-cc-dd-ee-ff".to_string()],
        )
    }

    #[test]
    fn config_is_offered_without_being_asked_for() {
        // By the time the phone wants this, the machine is asleep and cannot
        // answer a question. It has to arrive while there is still a session.
        let mut p = plugin();
        let r = run(0, |cx| p.on_peer_connected(cx, &peer()));
        let sent = r
            .sent("config")
            .expect("config should be offered on connect");
        let cfg: WolConfig = minicbor::decode(&sent.body).unwrap();
        assert_eq!(cfg.last_ipv4, "192.168.1.50");
        assert_eq!(cfg.port, 9);
    }

    #[test]
    fn a_machine_with_nothing_to_offer_stays_quiet() {
        let mut p = WolPlugin::default();
        let r = run(0, |cx| p.on_peer_connected(cx, &peer()));
        assert!(r.sends.is_empty());
    }

    #[test]
    fn relaying_to_an_allowlisted_mac_sends_a_packet() {
        let mut p = plugin();
        let body = minicbor::to_vec(RelayRequest {
            mac: "aa-bb-cc-dd-ee-ff".to_string(),
        })
        .unwrap();
        let env = envelope(5, CAP, "relay", &body);
        let r = run(0, |cx| p.on_message(cx, &peer(), &env).unwrap());
        assert!(matches!(
            r.one_effect(),
            Effect::SendMagicPacket { port: 9, .. }
        ));
    }

    #[test]
    fn the_allowlist_ignores_how_a_mac_is_punctuated() {
        // Colons, dashes and bare hex all name the same interface, and a user
        // who typed one form should not be refused for it.
        let mut p = plugin();
        for spelling in ["AA:BB:CC:DD:EE:FF", "aabbccddeeff", "aa-bb-cc-dd-ee-ff"] {
            let body = minicbor::to_vec(RelayRequest {
                mac: spelling.to_string(),
            })
            .unwrap();
            let env = envelope(1, CAP, "relay", &body);
            let r = run(0, |cx| {
                p.on_message(cx, &peer(), &env)
                    .unwrap_or_else(|e| panic!("{spelling} should be allowed, got {e:?}"));
            });
            assert_eq!(r.effects.len(), 1);
        }
    }

    #[test]
    fn relaying_to_anything_else_is_refused() {
        // Without this the endpoint sprays UDP wherever a peer points it.
        let mut p = plugin();
        let body = minicbor::to_vec(RelayRequest {
            mac: "11:22:33:44:55:66".to_string(),
        })
        .unwrap();
        let env = envelope(1, CAP, "relay", &body);
        let r = run(0, |cx| {
            assert_eq!(
                p.on_message(cx, &peer(), &env).unwrap_err(),
                PluginError::NotAllowed
            );
        });
        assert!(r.effects.is_empty(), "no packet should be sent");
    }

    #[test]
    fn a_mac_that_is_not_a_mac_is_refused() {
        let mut p = plugin();
        for junk in ["", "zz:zz:zz:zz:zz:zz", "aabbcc"] {
            let body = minicbor::to_vec(RelayRequest {
                mac: junk.to_string(),
            })
            .unwrap();
            let env = envelope(1, CAP, "relay", &body);
            run(0, |cx| {
                assert!(
                    p.on_message(cx, &peer(), &env).is_err(),
                    "{junk:?} should not be accepted"
                );
            });
        }
    }
}

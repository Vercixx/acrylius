//! `org.acrylius.ping/1`: the smallest possible plugin.
//!
//! It exists to exercise routing in both directions with no effector and no UI,
//! which makes it the thing that proves the plugin seam works before any feature
//! depends on it. KDE Connect has one for the same reason.

use crate::plugin::{Cx, Plugin, PluginError, PluginManifest};
use crate::proto::envelope::Envelope;
use crate::proto::ids::DeviceId;
use crate::vocab::UiEvent;

pub const CAP: &str = "org.acrylius.ping/1";

static MANIFEST: PluginManifest = PluginManifest {
    id: "org.acrylius.ping",
    outgoing: &[CAP],
    incoming: &[CAP],
    // No effects at all, so every host keeps this plugin.
    requires: &[],
};

#[derive(Default)]
pub struct PingPlugin {
    /// Pongs seen since construction. Handy in tests and in `acryliusctl`.
    pub pongs: u32,
}

impl Plugin for PingPlugin {
    fn manifest(&self) -> &'static PluginManifest {
        &MANIFEST
    }

    fn on_message(
        &mut self,
        cx: &mut Cx,
        peer: &DeviceId,
        env: &Envelope<'_>,
    ) -> Result<(), PluginError> {
        match env.ty {
            "ping" => {
                cx.reply(peer, env, "pong", env.body.to_vec());
                Ok(())
            }
            "pong" => {
                self.pongs += 1;
                cx.ui(UiEvent::Plugin {
                    peer: peer.clone(),
                    cap: CAP.to_string(),
                    ty: "pong".to_string(),
                    body: env.body.to_vec(),
                });
                Ok(())
            }
            // Answering an unknown verb with a named error, rather than
            // ignoring it, is what lets the other end say something useful.
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
        if ty == "ping" {
            cx.send(peer, CAP, "ping", body.to_vec());
            Ok(())
        } else {
            Err(PluginError::UnknownType(ty.to_string()))
        }
    }
}

//! What a Rust host's transport looks like.

use acrylius_core::link::{LinkId, TransportId};
use acrylius_core::vocab::{DialToken, Event};
use tokio::sync::mpsc;

/// Commands the runtime hands a transport. These are the transport-shaped
/// subset of `Action`, already resolved to this transport.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TransportCmd {
    Dial {
        dial: DialToken,
        addr: String,
    },
    Send {
        link: LinkId,
        msg: Vec<u8>,
    },
    Close {
        link: LinkId,
    },
    Advertise {
        enable: bool,
        txt: Vec<(String, String)>,
    },
    Discover {
        enable: bool,
    },
}

/// Where a transport pushes events. Cloneable so a transport can hand one to
/// every connection task it spawns.
pub type EventSink = mpsc::UnboundedSender<Event>;

#[async_trait::async_trait]
pub trait Transport: Send + Sync + 'static {
    fn id(&self) -> TransportId;

    /// Run until cancelled. Consumes commands, produces events.
    ///
    /// A transport must **never** call into the core. It only sends events,
    /// which the runtime's single serial loop picks up — that is the rule that
    /// keeps reentrancy impossible rather than merely unlikely.
    async fn run(
        self: std::sync::Arc<Self>,
        sink: EventSink,
        cmds: mpsc::UnboundedReceiver<TransportCmd>,
    ) -> anyhow::Result<()>;
}

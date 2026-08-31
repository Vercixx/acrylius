//! The action pump. One task owns the core; transports and effectors talk to
//! it only via [`Event`]s, so `handle()` can never be reentered.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use acrylius_core::core::Core;
use acrylius_core::link::TransportId;
use acrylius_core::vocab::{Action, Event, Now, TransferId, UiEvent};
use tokio::sync::mpsc;

use crate::effector::Effector;
use crate::store::Store;
use crate::transport::{Transport, TransportCmd};

/// Where the runtime publishes UI events for a local consumer (the control
/// socket, a CLI, a test).
pub type UiSink = mpsc::UnboundedSender<UiEvent>;

/// Where a bulk transfer's bytes come from and go to. Kept out of the core and
/// the transport: the core must not know what a file is, and the transport
/// must not decide where one lands.
#[async_trait::async_trait]
pub trait BulkHost: Send + Sync + 'static {
    /// Somewhere for the far end to connect. Key the listener by `transfer`;
    /// check the greeting against `offered_as`, the number the sender uses.
    async fn listen(
        &self,
        transfer: TransferId,
        offered_as: u64,
        key: Vec<u8>,
        expect_bytes: u64,
    ) -> anyhow::Result<String>;

    /// Wait for the far end to connect, and no further. Split from `receive`
    /// so a sender that never dials can be given up on while a slow file is not.
    async fn accept(&self, transfer: TransferId) -> anyhow::Result<()>;

    /// Take what arrives on the connection `accept` returned for.
    async fn receive(&self, transfer: TransferId) -> anyhow::Result<()>;

    /// Connect to somewhere the far end named, and send.
    async fn send(
        &self,
        transfer: TransferId,
        endpoint: String,
        key: Vec<u8>,
    ) -> anyhow::Result<()>;

    /// Stop a transfer that has not finished.
    fn cancel(&self, transfer: TransferId);
}

/// A read-only look at the core after each step. See [`Runtime::observe`].
pub type Observer = Box<dyn Fn(&Core) + Send>;

pub struct Runtime {
    core: Core,
    events_tx: mpsc::UnboundedSender<Event>,
    events_rx: mpsc::UnboundedReceiver<Event>,
    transports: HashMap<TransportId, mpsc::UnboundedSender<TransportCmd>>,
    effector: Arc<dyn Effector>,
    store: Box<dyn Store>,
    ui: Option<UiSink>,
    bulk: Option<Arc<dyn BulkHost>>,
    /// The task carrying each transfer, so cancelling one can abort a task
    /// already blocked inside the host.
    running: HashMap<TransferId, tokio::task::JoinHandle<()>>,
    /// Called with the core after every step, so a host can keep a live
    /// snapshot without holding the core itself.
    observer: Option<Observer>,
    /// Monotonic zero: a wall-clock change cannot move a deadline.
    started: Instant,
}

impl Runtime {
    pub fn new(core: Core, effector: Arc<dyn Effector>, store: Box<dyn Store>) -> Self {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        Self {
            core,
            events_tx,
            events_rx,
            transports: HashMap::new(),
            effector,
            store,
            ui: None,
            bulk: None,
            running: HashMap::new(),
            observer: None,
            started: Instant::now(),
        }
    }

    /// A handle for feeding the core from outside; the control socket uses it.
    #[must_use]
    pub fn events(&self) -> mpsc::UnboundedSender<Event> {
        self.events_tx.clone()
    }

    /// Give the runtime somewhere to put files. Without one, a bulk action is
    /// reported as a failed transfer rather than ignored.
    pub fn set_bulk(&mut self, bulk: Arc<dyn BulkHost>) {
        self.bulk = Some(bulk);
    }

    pub fn set_ui(&mut self, ui: UiSink) {
        self.ui = Some(ui);
    }

    /// Observe the core after each step. The closure gets `&Core` only, so there
    /// is deliberately no way to reach `handle()` from here.
    pub fn observe(&mut self, f: impl Fn(&Core) + Send + 'static) {
        self.observer = Some(Box::new(f));
    }

    pub fn add_transport(&mut self, t: Arc<dyn Transport>) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.transports.insert(t.id(), tx);
        let sink = self.events_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = t.run(sink, rx).await {
                tracing::error!(error = %e, "transport stopped");
            }
        });
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Wall-clock milliseconds, for the handshake timestamp a peer compares
    /// against its own clock. Never for a deadline: it can jump backwards.
    fn wall_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(0))
            .unwrap_or(0)
    }

    #[must_use]
    pub fn core(&self) -> &Core {
        &self.core
    }

    /// Run until the event channel closes.
    pub async fn run(mut self) {
        let mut pairing = self.core.pairing_open();
        for tx in self.transports.values() {
            let _ = tx.send(TransportCmd::Advertise {
                enable: true,
                txt: self.txt(pairing),
            });
            let _ = tx.send(TransportCmd::Discover { enable: true });
        }

        let mut deadline: Option<u64> = None;
        loop {
            let sleep = match deadline {
                Some(d) => {
                    let now = self.now_ms();
                    let dur = Duration::from_millis(d.saturating_sub(now));
                    tokio::time::sleep(dur)
                }
                // Nothing pending: a long sleep the next event will interrupt.
                None => tokio::time::sleep(Duration::from_secs(3600)),
            };
            tokio::pin!(sleep);

            let ev = tokio::select! {
                ev = self.events_rx.recv() => match ev {
                    Some(e) => e,
                    None => return,
                },
                () = &mut sleep => Event::Tick,
            };

            // A finished transfer's handle would otherwise never leave the map.
            if let Event::BulkFinished { transfer, .. } = &ev {
                self.running.remove(transfer);
            }

            let now = Now {
                monotonic_ms: self.now_ms(),
                wall_ms: Self::wall_ms(),
            };
            let out = self.core.handle(now, ev);
            deadline = out.next_deadline_ms;
            for a in out.actions {
                self.apply(a).await;
            }
            // Re-advertise when the pairing window opens or closes. Compared
            // rather than event-driven: a window can also close silently, by
            // expiry, and `pair=1` must not stay on the air past that.
            let now_pairing = self.core.pairing_open();
            if now_pairing != pairing {
                pairing = now_pairing;
                for tx in self.transports.values() {
                    let _ = tx.send(TransportCmd::Advertise {
                        enable: true,
                        txt: self.txt(pairing),
                    });
                }
            }

            if let Some(f) = &self.observer {
                f(&self.core);
            }
        }
    }

    /// What this device advertises about itself. Never the raw static key,
    /// which would deanonymize the `IKpsk2` opener; the display name is the
    /// transport's business, not the core's.
    fn txt(&self, pairing: bool) -> Vec<(String, String)> {
        vec![
            ("v".to_string(), "1".to_string()),
            ("fp".to_string(), self.core.fingerprint().to_string()),
            ("id".to_string(), self.core.device_id().to_string()),
            (
                "pair".to_string(),
                if pairing { "1" } else { "0" }.to_string(),
            ),
        ]
    }

    /// Report a transfer that never started as one that finished badly, so the
    /// far end is not left waiting for an endpoint that is never coming.
    fn bulk_failed(&self, transfer: TransferId, why: &str) {
        let _ = self.events_tx.send(Event::BulkFinished {
            transfer,
            ok: false,
            detail: why.to_string(),
        });
    }

    async fn apply(&mut self, action: Action) {
        match action {
            Action::Dial {
                transport,
                addr,
                dial,
            } => {
                if let Some(tx) = self.transports.get(&transport) {
                    let _ = tx.send(TransportCmd::Dial { dial, addr });
                }
            }
            Action::LinkSend { link, msg } => {
                // A link belongs to exactly one transport, but the core does not
                // track which, so send to all and let the one that owns it act.
                for tx in self.transports.values() {
                    let _ = tx.send(TransportCmd::Send {
                        link,
                        msg: msg.clone(),
                    });
                }
            }
            Action::Close { link, .. } => {
                for tx in self.transports.values() {
                    let _ = tx.send(TransportCmd::Close { link });
                }
            }
            Action::Effect { token, effect } => {
                // Own task, so a slow effector cannot stall the loop.
                let eff = self.effector.clone();
                let back = self.events_tx.clone();
                tokio::spawn(async move {
                    let result = eff.run(effect).await;
                    let _ = back.send(Event::EffectDone { token, result });
                });
            }
            Action::Persist {
                key,
                value,
                sensitivity,
            } => {
                if let Err(e) = self.store.put(&key, value.as_deref(), sensitivity) {
                    tracing::error!(key, error = %e, "could not persist");
                }
            }
            Action::Advertise {
                transport,
                enable,
                txt,
            } => {
                if let Some(tx) = self.transports.get(&transport) {
                    let _ = tx.send(TransportCmd::Advertise { enable, txt });
                }
            }
            Action::Discover { transport, enable } => {
                if let Some(tx) = self.transports.get(&transport) {
                    let _ = tx.send(TransportCmd::Discover { enable });
                }
            }
            Action::BulkListen {
                transfer,
                offered_as,
                key,
                expect_bytes,
            } => {
                let Some(bulk) = self.bulk.clone() else {
                    self.bulk_failed(transfer, "this host cannot receive files");
                    return;
                };
                let back = self.events_tx.clone();
                let task = tokio::spawn(async move {
                    match bulk.listen(transfer, offered_as, key, expect_bytes).await {
                        Ok(endpoint) => {
                            let _ = back.send(Event::BulkListening { transfer, endpoint });
                            // `accept` blocks until the far end connects; the
                            // core bounds the wait before it, never the
                            // transfer after it.
                            if let Err(e) = bulk.accept(transfer).await {
                                let _ = back.send(Event::BulkFinished {
                                    transfer,
                                    ok: false,
                                    detail: e.to_string(),
                                });
                                return;
                            }
                            let _ = back.send(Event::BulkStarted { transfer });
                            let (ok, detail) = match bulk.receive(transfer).await {
                                Ok(()) => (true, String::new()),
                                Err(e) => (false, e.to_string()),
                            };
                            let _ = back.send(Event::BulkFinished {
                                transfer,
                                ok,
                                detail,
                            });
                        }
                        Err(e) => {
                            let _ = back.send(Event::BulkFinished {
                                transfer,
                                ok: false,
                                detail: e.to_string(),
                            });
                        }
                    }
                });
                self.running.insert(transfer, task);
            }

            Action::BulkSend {
                transfer,
                endpoint,
                key,
            } => {
                let Some(bulk) = self.bulk.clone() else {
                    self.bulk_failed(transfer, "this host cannot send files");
                    return;
                };
                let back = self.events_tx.clone();
                let task = tokio::spawn(async move {
                    let (ok, detail) = match bulk.send(transfer, endpoint, key).await {
                        Ok(()) => (true, String::new()),
                        Err(e) => (false, e.to_string()),
                    };
                    let _ = back.send(Event::BulkFinished {
                        transfer,
                        ok,
                        detail,
                    });
                });
                self.running.insert(transfer, task);
            }

            Action::BulkCancel { transfer } => {
                // Abort the task first: the host forgetting a transfer does
                // nothing to a task already blocked inside it.
                if let Some(task) = self.running.remove(&transfer) {
                    task.abort();
                }
                if let Some(bulk) = &self.bulk {
                    bulk.cancel(transfer);
                }
            }

            Action::Ui(e) => {
                tracing::debug!(?e, "ui");
                if let Some(ui) = &self.ui {
                    let _ = ui.send(e);
                }
            }
        }
    }
}

/// Wall-clock milliseconds for a handshake timestamp.
#[must_use]
pub fn wall_clock_ms() -> u64 {
    Runtime::wall_ms()
}

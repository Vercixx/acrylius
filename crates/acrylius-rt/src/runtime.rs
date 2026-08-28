//! The action pump.
//!
//! One task owns the core. Nothing else ever touches it.
//!
//! Transports and effectors run on their own tasks and communicate only by
//! sending [`Event`]s into a channel this loop drains. Results of actions come
//! back the same way. That is the rule from the top of `vocab`, made structural:
//! there is no handle to the core to misuse, so `handle()` cannot be reentered
//! from inside an action handler.

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

/// Where a bulk transfer's bytes come from and go to.
///
/// Kept out of the core and out of the transport alike: the core must not know
/// what a file is, and the transport must not decide where one lands. A host
/// that has no answer to these does not offer the capability, and an offer it
/// receives is refused rather than half-honoured.
#[async_trait::async_trait]
pub trait BulkHost: Send + Sync + 'static {
    /// Somewhere for the far end to connect, for this transfer.
    ///
    /// `offered_as` is the number the *sender* uses, and the only one it will
    /// put in its greeting. Keep the listener under `transfer`, which is what
    /// everything else here is keyed by, and check the greeting against
    /// `offered_as`.
    async fn listen(
        &self,
        transfer: TransferId,
        offered_as: u64,
        key: Vec<u8>,
        expect_bytes: u64,
    ) -> anyhow::Result<String>;

    /// Wait for the far end to connect, and no further.
    ///
    /// Split from `receive` so that the two silences can be told apart: a
    /// sender that never dials has to be given up on, and a file taking its time
    /// must not be. Only a host knows which of the two it is in, and this
    /// returning is how it says so.
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
    /// The task carrying each transfer, so that cancelling one can reach it.
    ///
    /// A bulk transfer runs detached, because it lasts as long as a file takes
    /// and the loop below has other work. Detached and *forgotten*, though, is
    /// what made `BulkCancel` a suggestion: the host's `cancel` takes the
    /// transfer out of its own table, which the task stopped consulting the
    /// moment it started, so a receive already blocked on `accept` never heard
    /// about it and waited for a connection that was not coming.
    running: HashMap<TransferId, tokio::task::JoinHandle<()>>,
    /// Called with the core after every step, so a host can keep a live
    /// snapshot for its own queries without ever holding the core itself.
    observer: Option<Observer>,
    /// Monotonic zero. The core is handed milliseconds since this instant, so a
    /// wall-clock change cannot move a deadline, which is what makes the pairing
    /// window's expiry honest.
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
    /// reported as a failed transfer rather than ignored — a sender waiting
    /// forever for an endpoint is worse than one told no.
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
        // Start advertising and browsing on every transport we have.
        let txt = vec![
            ("v".to_string(), "1".to_string()),
            ("fp".to_string(), self.core.fingerprint().to_string()),
            ("id".to_string(), self.core.device_id().to_string()),
        ];
        for tx in self.transports.values() {
            let _ = tx.send(TransportCmd::Advertise {
                enable: true,
                txt: txt.clone(),
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

            // A transfer that has ended has nothing left to abort, and a handle
            // kept for it is one this map never gives back.
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
            if let Some(f) = &self.observer {
                f(&self.core);
            }
        }
    }

    /// Report a transfer that never started as one that finished badly.
    ///
    /// Silence would leave the far end waiting for an endpoint that is never
    /// coming, and it has no way to tell that from a slow disk.
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
                // Off to its own task, so a slow effector cannot stall the loop.
                // Its answer arrives as an ordinary event.
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
                            // Accepting blocks until the far end connects, so it
                            // runs after the endpoint has been sent rather than
                            // before it.
                            //
                            // Reported the moment it returns, because that is the
                            // one thing about a transfer only a host can know and
                            // the core needs it: everything before this is a wait
                            // that has to be bounded, and everything after is a
                            // file arriving, which must not be.
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
                // Held for the same reason a receive is: a send that cannot
                // reach the endpoint it was given blocks in `connect` just as
                // patiently.
                self.running.insert(transfer, task);
            }

            Action::BulkCancel { transfer } => {
                // The task first. Telling the host to forget a transfer does
                // nothing to a task already blocked inside it, and that task
                // holds the listener — so without this a cancelled receive kept
                // its port bound and its file reserved for the life of the
                // process.
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

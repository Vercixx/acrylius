//! Asking a person about a file, where they will actually see the question.
//!
//! A file offered to this machine needs an answer from somebody. Until now the
//! only way to give one was a terminal, which meant the answer arrived whenever
//! that person next thought to look and the sender waited in the meantime — and
//! when a transfer did land, nothing said where. Both are the same failure:
//! this is a desktop, and a question for the person at it belongs on their
//! screen.
//!
//! Nothing here decides anything. It puts a question up, reports which button
//! was pressed, and turns that into the same local command `acryliusctl` sends.
//! A machine with no notification daemon loses the notifications and keeps the
//! CLI, which is why none of this is required for a transfer to work.

use std::collections::BTreeMap;
use std::sync::Arc;

use acrylius_core::plugins::share;
use acrylius_core::vocab::{Event, LocalCommand, TransferId};
use acrylius_linux::notify::{Button, Notifier, Pressed};
use tokio::sync::{Mutex, mpsc};

use crate::files::FileBulk;

const ACCEPT: &str = "accept";
const REJECT: &str = "reject";
const SHOW: &str = "show";

pub struct Prompter {
    notifier: Notifier,
    /// Which offer a notification is asking about.
    asked: Mutex<BTreeMap<u32, (String, u64)>>,
    /// Which directory a finished notification would open.
    finished: Mutex<BTreeMap<u32, std::path::PathBuf>>,
    events: mpsc::UnboundedSender<Event>,
}

impl Prompter {
    /// Connect and start listening for pressed buttons.
    ///
    /// `None` where there is no notification daemon — a headless machine, or a
    /// session this did not start inside. Not an error: the CLI is unaffected.
    pub async fn start(
        events: mpsc::UnboundedSender<Event>,
        bulk: Arc<FileBulk>,
    ) -> Option<Arc<Self>> {
        let (notifier, mut pressed) = Notifier::connect().await?;
        let prompter = Arc::new(Self {
            notifier,
            asked: Mutex::new(BTreeMap::new()),
            finished: Mutex::new(BTreeMap::new()),
            events,
        });

        let listening = prompter.clone();
        tokio::spawn(async move {
            while let Some(press) = pressed.recv().await {
                listening.pressed(&press, &bulk).await;
            }
        });
        Some(prompter)
    }

    /// Put an offer on the screen.
    pub async fn ask(&self, peer: &str, from: &str, offer: &share::Offer) {
        let body = if self.notifier.has_buttons() {
            format!("{} · {}", offer.name, human(offer.size))
        } else {
            // No buttons on this desktop, so the notification has to say what
            // to type instead of pretending to be answerable.
            format!(
                "{} · {}\nRun: acryliusctl file accept {}",
                offer.name,
                human(offer.size),
                offer.transfer
            )
        };
        let buttons = [
            Button {
                key: ACCEPT,
                label: "Accept",
            },
            Button {
                key: REJECT,
                label: "Deny",
            },
        ];
        // Zero: it stays until answered. A question that expired off the screen
        // while somebody was in another room is one the sender is still waiting
        // on, and there would be nothing left to say yes to.
        let Some(id) = self
            .notifier
            .show(&format!("{from} wants to send a file"), &body, &buttons, 0)
            .await
        else {
            return;
        };
        self.asked
            .lock()
            .await
            .insert(id, (peer.to_string(), offer.transfer));
    }

    /// Say how a transfer ended, and where the file went.
    pub async fn done(&self, bulk: &FileBulk, transfer: u64, ok: bool, detail: &str) {
        // Take down the question, if it is still up: it has been answered, and
        // an answered question left on screen is one that gets answered twice.
        let mut asked = self.asked.lock().await;
        let stale: Vec<u32> = asked
            .iter()
            .filter(|(_, (_, t))| *t == transfer)
            .map(|(id, _)| *id)
            .collect();
        for id in &stale {
            asked.remove(id);
            self.notifier.close(*id).await;
        }
        drop(asked);

        // Only a transfer this machine was asked about. Both ends announce a
        // result, so without this a machine would report the arrival of a file
        // it had itself sent.
        if stale.is_empty() {
            return;
        }

        let Some(path) = bulk.landed(TransferId(transfer)) else {
            if !ok {
                let reason = if detail.is_empty() { "" } else { detail };
                self.notifier
                    .show("A file did not arrive", reason, &[], 8000)
                    .await;
            }
            return;
        };
        let directory = path.parent().map(std::path::Path::to_path_buf);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let buttons = [Button {
            key: SHOW,
            label: "Show",
        }];
        // The full path in the body, because the point of this notification is
        // that somebody could not find their file. The button is the shortcut;
        // the text is the answer.
        if let Some(id) = self
            .notifier
            .show(
                &format!("Received {name}"),
                &path.to_string_lossy(),
                &buttons,
                12_000,
            )
            .await
            && let Some(directory) = directory
        {
            self.finished.lock().await.insert(id, directory);
        }
    }

    async fn pressed(&self, press: &Pressed, bulk: &FileBulk) {
        if press.action == SHOW {
            if let Some(directory) = self.finished.lock().await.remove(&press.id) {
                open(&directory).await;
            }
            return;
        }

        let Some((peer, transfer)) = self.asked.lock().await.remove(&press.id) else {
            return;
        };
        let accept = press.action == ACCEPT;
        if !accept {
            bulk.forget(TransferId(transfer));
        }
        let body = minicbor::to_vec(share::Finished {
            transfer,
            ok: accept,
            detail: String::new(),
        })
        .unwrap_or_default();
        // The same request `acryliusctl file accept` makes. A button and a command
        // are two ways to say one thing, not two things.
        let _ = self.events.send(Event::Local(LocalCommand::Plugin {
            peer: acrylius_core::proto::ids::DeviceId::parse(&peer)
                .unwrap_or_else(|_| acrylius_core::proto::ids::DeviceId::of(&[0u8; 32])),
            cap: share::CAP.to_string(),
            ty: if accept { "accept" } else { "reject" }.to_string(),
            body,
        }));
        tracing::info!(
            transfer,
            accept,
            "answered a file offer from a notification"
        );
    }
}

/// Open a directory in whatever this desktop uses for one.
///
/// A subprocess, and the right call: "which application handles a directory" is
/// a question with a standard answer and no library that gives it without
/// pulling in a desktop toolkit. It runs only when somebody presses a button.
async fn open(path: &std::path::Path) {
    let _ = tokio::process::Command::new("xdg-open")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Bytes, for a person.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_as_sizes() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(512), "512 B");
        assert_eq!(human(200_000), "195.3 KiB");
        assert_eq!(human(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }
}

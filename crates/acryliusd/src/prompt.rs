//! Asking a person where they'll see the question: a file offer or a pairing
//! request, turned into the same command `acryliusctl` sends.
//! The pairing prompt is a security boundary (plain `XX`; these six digits are
//! all that authenticates it) — the file prompt is not.

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
const MATCH: &str = "match";
const DIFFER: &str = "differ";

pub struct Prompter {
    notifier: Notifier,
    /// Which offer a notification is asking about.
    asked: Mutex<BTreeMap<u32, (String, u64)>>,
    /// Which directory a finished notification would open.
    finished: Mutex<BTreeMap<u32, std::path::PathBuf>>,
    /// The pairing question currently on screen, if any (only one, since the
    /// core handles one pairing at a time). Cleared when it resolves any other
    /// way, so a stale question can't pair whoever presses it next.
    pairing: Mutex<Option<u32>>,
    /// Where a file would go, when this machine accepts files at all.
    ///
    /// `None` when `[share] enabled = false`; unrelated to the pairing half of this type.
    bulk: Option<Arc<FileBulk>>,
    events: mpsc::UnboundedSender<Event>,
}

impl Prompter {
    /// Connect and start listening for pressed buttons.
    ///
    /// Returns `None` with no notification daemon (headless, or wrong
    /// session) — not an error, the CLI is unaffected.
    pub async fn start(
        events: mpsc::UnboundedSender<Event>,
        bulk: Option<Arc<FileBulk>>,
    ) -> Option<Arc<Self>> {
        let (notifier, mut pressed) = Notifier::connect().await?;
        let prompter = Arc::new(Self {
            notifier,
            asked: Mutex::new(BTreeMap::new()),
            finished: Mutex::new(BTreeMap::new()),
            pairing: Mutex::new(None),
            bulk,
            events,
        });

        let listening = prompter.clone();
        tokio::spawn(async move {
            while let Some(press) = pressed.recv().await {
                listening.pressed(&press).await;
            }
        });
        Some(prompter)
    }

    /// Put a pairing question on the screen.
    ///
    /// The digits go in the body, not the summary, so a long device name
    /// can't push them off the end.
    pub async fn ask_pair(&self, name: &str, fingerprint: &str, sas: &str) {
        // Take down any previous one: the core only allows one pending pairing at a time.
        self.close_pair().await;

        let body = if self.notifier.has_buttons() {
            format!("{sas}\n{fingerprint}")
        } else {
            // No buttons on this desktop; say what to type instead.
            format!("{sas}\n{fingerprint}\nRun: acryliusctl pair approve")
        };
        let buttons = [
            Button {
                key: MATCH,
                label: "They match",
            },
            Button {
                key: DIFFER,
                label: "They don't",
            },
        ];
        // Timeout zero: stays until answered, so reading the digits off a phone doesn't race a timeout.
        if let Some(id) = self
            .notifier
            .show(&format!("{name} wants to pair"), &body, &buttons, 0)
            .await
        {
            *self.pairing.lock().await = Some(id);
        }
    }

    /// Take the pairing question down, however it was settled.
    pub async fn close_pair(&self) {
        if let Some(id) = self.pairing.lock().await.take() {
            self.notifier.close(id).await;
        }
    }

    /// Put an offer on the screen.
    pub async fn ask(&self, peer: &str, from: &str, offer: &share::Offer) {
        let body = if self.notifier.has_buttons() {
            format!("{} · {}", offer.name, human(offer.size))
        } else {
            // No buttons here; say what to type instead.
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
        // Timeout zero: stays until answered, since the sender is still waiting either way.
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
        // Take the question down if still up: an answered one left on screen could be answered twice.
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

        // Only for transfers this machine was asked about; both ends announce a result.
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
        // Full path in the body: the point is helping someone find the file; the button is just a shortcut.
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

    async fn pressed(&self, press: &Pressed) {
        if press.action == SHOW {
            if let Some(directory) = self.finished.lock().await.remove(&press.id) {
                open(&directory).await;
            }
            return;
        }

        if is_pair_answer(&press.action) {
            let mut pairing = self.pairing.lock().await;
            let Some(accept) = pair_answer(&press.action, press.id, *pairing) else {
                return;
            };
            *pairing = None;
            drop(pairing);

            // The same command `acryliusctl pair approve` sends.
            let _ = self
                .events
                .send(Event::Local(LocalCommand::ConfirmPairing { accept }));
            tracing::info!(accept, "answered a pairing from a notification");
            return;
        }

        let Some((peer, transfer)) = self.asked.lock().await.remove(&press.id) else {
            return;
        };
        let accept = press.action == ACCEPT;
        if !accept && let Some(bulk) = &self.bulk {
            bulk.forget(TransferId(transfer));
        }
        let body = minicbor::to_vec(share::Finished {
            transfer,
            ok: accept,
            detail: String::new(),
        })
        .unwrap_or_default();
        // The same request `acryliusctl file accept` makes.
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

/// Whether a pressed button is an answer to the pairing question at all.
fn is_pair_answer(action: &str) -> bool {
    action == MATCH || action == DIFFER
}

/// What a pressed button means for the pairing question, or `None` to ignore it.
///
/// Getting either `==` backwards would pair on **They don't** — the digits
/// are all that authenticates a pairing, and mutation testing caught exactly
/// that before this existed. `showing` is the notification currently up; a
/// press for anything else is stale and must be ignored.
fn pair_answer(action: &str, pressed: u32, showing: Option<u32>) -> Option<bool> {
    if showing != Some(pressed) {
        return None;
    }
    match action {
        MATCH => Some(true),
        DIFFER => Some(false),
        _ => None,
    }
}

/// Open a directory in whatever this desktop uses for one.
///
/// A subprocess: no library answers "which app handles this" without pulling
/// in a full desktop toolkit.
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

    #[test]
    fn the_two_pairing_buttons_mean_opposite_things() {
        // Security-critical: confirming on "They don't" would hand a refusal straight to whoever was refused.
        assert_eq!(pair_answer(MATCH, 7, Some(7)), Some(true));
        assert_eq!(pair_answer(DIFFER, 7, Some(7)), Some(false));
    }

    #[test]
    fn a_press_for_a_question_that_is_gone_answers_nothing() {
        // Notification ids get reused; a stale press must not confirm the next pairing.
        assert_eq!(pair_answer(MATCH, 7, Some(9)), None, "a different question");
        assert_eq!(pair_answer(MATCH, 7, None), None, "no question at all");
        assert_eq!(pair_answer(DIFFER, 7, None), None);
    }

    #[test]
    fn only_the_pairing_buttons_are_pairing_answers() {
        // File-offer buttons share this handler; `accept` must not fall into the pairing branch.
        assert!(is_pair_answer(MATCH) && is_pair_answer(DIFFER));
        for other in [ACCEPT, REJECT, SHOW, "", "matchx"] {
            assert!(!is_pair_answer(other), "{other} is not a pairing answer");
            assert_eq!(pair_answer(other, 7, Some(7)), None);
        }
    }
}

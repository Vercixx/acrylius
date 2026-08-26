//! Desktop notifications, with buttons.
//!
//! A file offered to this machine has to be answered by a person, and until now
//! the only way was a terminal — which means the answer arrives when somebody
//! next thinks to look, and the sender waits. A notification with Accept and
//! Deny on it is where an answer of that kind belongs.
//!
//! Two things are deliberate:
//!
//! * Actions are checked for, not assumed. `GetCapabilities` says whether this
//!   desktop's notification daemon draws buttons at all; several do not, and one
//!   that does not would show a notification nobody can answer. Where they are
//!   missing the notification still appears and says to use `acryliusctl`.
//! * Nothing here decides anything. An invoked action is reported to whoever
//!   asked, and it is the caller that turns it into a request — the same shape
//!   as every other effector in this crate.

use std::collections::HashMap;

use futures_lite::stream::StreamExt;
use tokio::sync::mpsc;
use zbus::zvariant::Value;

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, &Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    fn get_capabilities(&self) -> zbus::Result<Vec<String>>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

/// A button on a notification, once somebody has pressed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pressed {
    pub id: u32,
    pub action: String,
}

/// One button: the key reported back, and the label drawn on it.
pub struct Button<'a> {
    pub key: &'a str,
    pub label: &'a str,
}

pub struct Notifier {
    proxy: NotificationsProxy<'static>,
    /// Whether this desktop draws buttons. A notification with actions on a
    /// daemon that ignores them is a question with no way to answer it.
    buttons: bool,
}

impl Notifier {
    /// Connect, and start reporting pressed buttons.
    ///
    /// Returns `None` where there is no notification daemon at all, which is a
    /// normal state on a headless machine and not an error.
    pub async fn connect() -> Option<(Self, mpsc::UnboundedReceiver<Pressed>)> {
        let connection = zbus::Connection::session().await.ok()?;
        let proxy = NotificationsProxy::new(&connection).await.ok()?;
        let buttons = proxy
            .get_capabilities()
            .await
            .map(|caps| caps.iter().any(|c| c == "actions"))
            .unwrap_or(false);
        if !buttons {
            tracing::info!(
                "this desktop's notifications have no buttons; \
                 a file offer will say to use acryliusctl"
            );
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let signals = proxy.receive_action_invoked().await.ok()?;
        tokio::spawn(async move {
            let mut signals = signals;
            while let Some(signal) = signals.next().await {
                let Ok(args) = signal.args() else { continue };
                if tx
                    .send(Pressed {
                        id: args.id,
                        action: args.action_key.to_string(),
                    })
                    .is_err()
                {
                    return;
                }
            }
        });

        Some((Self { proxy, buttons }, rx))
    }

    #[must_use]
    pub fn has_buttons(&self) -> bool {
        self.buttons
    }

    /// Put a notification up, and say which one it is.
    ///
    /// `timeout_ms` of 0 means it stays until answered, which is what a question
    /// wants: an offer that expired off the screen while somebody was in another
    /// room is one the sender is still waiting on.
    pub async fn show(
        &self,
        summary: &str,
        body: &str,
        buttons: &[Button<'_>],
        timeout_ms: i32,
    ) -> Option<u32> {
        // "key", "Label", "key", "Label", … which is how this interface takes
        // them.
        let mut actions: Vec<&str> = Vec::with_capacity(buttons.len() * 2);
        if self.buttons {
            for b in buttons {
                actions.push(b.key);
                actions.push(b.label);
            }
        }
        let urgency = Value::from(1u8);
        let mut hints: HashMap<&str, &Value<'_>> = HashMap::new();
        hints.insert("urgency", &urgency);

        self.proxy
            .notify(
                "acrylius",
                0,
                "document-send",
                summary,
                body,
                &actions,
                hints,
                timeout_ms,
            )
            .await
            .ok()
    }

    /// Take one down, because it has been answered somewhere else.
    ///
    /// A question left on screen after `acryliusctl accept` answered it is a
    /// question that will be answered twice.
    pub async fn close(&self, id: u32) {
        let _ = self.proxy.close_notification(id).await;
    }
}

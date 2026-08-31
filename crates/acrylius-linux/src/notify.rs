//! Desktop notifications, with buttons.
//!
//! Buttons are only added when `GetCapabilities` reports `actions` support;
//! otherwise the notification falls back to naming `acryliusctl`. An invoked
//! action is only reported here, never acted on.

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
    /// Whether this desktop draws buttons on notifications.
    buttons: bool,
    /// Whether this desktop renders markup in a body; escaping unconditionally
    /// would turn `Q&A.pdf` into `Q&amp;A.pdf` on a server that shows it literally.
    markup: bool,
}

/// Escape the markup subset a notification body may be parsed for.
///
/// GNOME/KDE render `<b>`/`<i>`/`<u>`/`<a href>` in bodies, so a peer-chosen
/// name left unescaped could draw a link or hide the question.
#[must_use]
pub fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

impl Notifier {
    /// Connect and start reporting pressed buttons. `None` if there's no
    /// daemon (normal on a headless machine).
    pub async fn connect() -> Option<(Self, mpsc::UnboundedReceiver<Pressed>)> {
        let connection = zbus::Connection::session().await.ok()?;
        let proxy = NotificationsProxy::new(&connection).await.ok()?;
        let caps = proxy.get_capabilities().await.unwrap_or_default();
        let buttons = caps.iter().any(|c| c == "actions");
        let markup = caps.iter().any(|c| c == "body-markup");
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

        Some((
            Self {
                proxy,
                buttons,
                markup,
            },
            rx,
        ))
    }

    #[must_use]
    pub fn has_buttons(&self) -> bool {
        self.buttons
    }

    /// Put a notification up. `timeout_ms` of 0 keeps it until answered.
    pub async fn show(
        &self,
        summary: &str,
        body: &str,
        buttons: &[Button<'_>],
        timeout_ms: i32,
    ) -> Option<u32> {
        // "key", "Label", "key", "Label", ...: the shape this interface expects.
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

        // The body carries peer-chosen text and may be parsed; see `escape_markup`.
        let body = if self.markup {
            escape_markup(body)
        } else {
            body.to_string()
        };
        self.proxy
            .notify(
                "acrylius",
                0,
                "document-send",
                summary,
                &body,
                &actions,
                hints,
                timeout_ms,
            )
            .await
            .ok()
    }

    /// Take a notification down once it's answered elsewhere (e.g. via `acryliusctl`).
    pub async fn close(&self, id: u32) {
        let _ = self.proxy.close_notification(id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::escape_markup;

    #[test]
    fn a_file_name_cannot_bring_its_own_markup() {
        // Peer-chosen name shown to someone deciding whether to accept a file.
        assert_eq!(
            escape_markup("<b>invoice.pdf</b>"),
            "&lt;b&gt;invoice.pdf&lt;/b&gt;"
        );
        assert_eq!(
            escape_markup(r#"a<a href="http://x">click</a>"#),
            "a&lt;a href=\"http://x\"&gt;click&lt;/a&gt;"
        );
        // Ampersands must escape first, or escaping `<`/`>` would re-escape via the `&` it wrote.
        assert_eq!(escape_markup("Q&A <notes>"), "Q&amp;A &lt;notes&gt;");
    }

    #[test]
    fn an_ordinary_name_is_left_alone() {
        assert_eq!(
            escape_markup("holiday photo (2).jpg"),
            "holiday photo (2).jpg"
        );
        assert_eq!(escape_markup("отчёт.pdf"), "отчёт.pdf");
    }
}

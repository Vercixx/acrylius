//! The Wayland clipboard, through `ext-data-control` / `wlr-data-control`.
//!
//! Two things about Wayland shape this.
//!
//! Whoever sets a selection must stay alive to serve it. A clipboard is not
//! storage; it is a promise to hand over bytes when someone asks. So setting the
//! clipboard starts a thread that serves it, and that thread lives until another
//! client takes the selection away, at which point the compositor tells it so and
//! it exits on its own. `wl-copy` handles this by forking. A long-lived daemon
//! does not have to, and forking a multithreaded process to run a Wayland event
//! loop is worth avoiding, so this uses foreground mode and owns the thread.
//!
//! There is also no watch API. `wl-clipboard-rs` exposes `copy` and `paste` and
//! nothing else, so noticing a change means polling and comparing a hash. That
//! costs a wakeup a second and is the honest option for version 1; binding
//! `ext-data-control` directly to get a `selection` event is the upgrade, and it
//! is contained because it lives behind this module.

use std::io::Read;
use std::time::Duration;

use wl_clipboard_rs::copy::{ClipboardType as CopyType, MimeType as CopyMime, Options, Source};
use wl_clipboard_rs::paste::{
    ClipboardType as PasteType, Error as PasteError, MimeType as PasteMime, Seat, get_contents,
};

/// How often to look for a change. Fast enough to feel immediate, slow enough
/// not to matter.
pub const POLL_INTERVAL: Duration = Duration::from_millis(700);

/// Read the clipboard as UTF-8 text.
///
/// An empty clipboard is `Ok(None)` rather than an error: nothing has gone
/// wrong, there is simply nothing there.
pub async fn read() -> anyhow::Result<Option<Vec<u8>>> {
    tokio::task::spawn_blocking(|| {
        match get_contents(PasteType::Regular, Seat::Unspecified, PasteMime::Text) {
            Ok((mut pipe, _mime)) => {
                let mut buf = Vec::new();
                pipe.read_to_end(&mut buf)?;
                Ok(Some(buf))
            }
            Err(PasteError::NoSeats | PasteError::ClipboardEmpty | PasteError::NoMimeType) => {
                Ok(None)
            }
            Err(e) => Err(anyhow::Error::new(e)),
        }
    })
    .await?
}

/// Put text on the clipboard and keep serving it.
///
/// The serving thread exits by itself once another client takes the selection,
/// including when this function is called again, so there is nothing to cancel.
pub async fn write(data: Vec<u8>) -> anyhow::Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut opts = Options::new();
        opts.foreground(true);
        opts.clipboard(CopyType::Regular);
        let prepared = opts.prepare_copy(Source::Bytes(data.into_boxed_slice()), CopyMime::Text);
        match prepared {
            Ok(copy) => {
                // Report success once the selection is *taken*, not once it is
                // released, or every write would block until someone else
                // copied something.
                let _ = tx.send(Ok(()));
                if let Err(e) = copy.serve() {
                    tracing::debug!(error = %e, "clipboard serving ended");
                }
            }
            Err(e) => {
                let _ = tx.send(Err(anyhow::Error::new(e)));
            }
        }
    });
    rx.await?
}

/// Watch for changes, reporting each new value once.
///
/// The first observation is reported too. A daemon that has just started has no
/// idea what is on the clipboard, and treating the first read as "unchanged"
/// would mean the first copy after a restart went nowhere.
pub async fn watch(mut on_change: impl FnMut(Vec<u8>) + Send + 'static) {
    let mut last: Option<Vec<u8>> = None;
    loop {
        match read().await {
            Ok(Some(current)) => {
                if last.as_ref() != Some(&current) {
                    last = Some(current.clone());
                    on_change(current);
                }
            }
            Ok(None) => last = None,
            Err(e) => tracing::debug!(error = %e, "clipboard read failed"),
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

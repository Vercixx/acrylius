//! The Wayland clipboard, through `ext-data-control` / `wlr-data-control`.
//!
//! Whoever sets a selection must stay alive to serve it, so a write starts a
//! thread that serves until another client takes the selection. There is no
//! watch API in `wl-clipboard-rs`, so change detection polls and compares.

use std::io::Read;
use std::time::Duration;

use wl_clipboard_rs::copy::{ClipboardType as CopyType, MimeType as CopyMime, Options, Source};
use wl_clipboard_rs::paste::{
    ClipboardType as PasteType, Error as PasteError, MimeType as PasteMime, Seat, get_contents,
};

pub const POLL_INTERVAL: Duration = Duration::from_millis(700);

/// Read the clipboard as UTF-8 text; an empty clipboard is `Ok(None)`.
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

/// Put text on the clipboard and keep serving it. The serving thread exits by
/// itself once another client takes the selection; nothing to cancel.
pub async fn write(data: Vec<u8>) -> anyhow::Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut opts = Options::new();
        opts.foreground(true);
        opts.clipboard(CopyType::Regular);
        let prepared = opts.prepare_copy(Source::Bytes(data.into_boxed_slice()), CopyMime::Text);
        match prepared {
            Ok(copy) => {
                // Report success once the selection is taken, not released, or
                // every write would block until someone else copied.
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

/// Watch for changes, reporting each new value once. The first observation is
/// reported too, or the first copy after a restart would go nowhere.
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

//! Sending a file, from a host with no async runtime.
//!
//! The daemon moves these frames over tokio. A phone cannot: nothing async may
//! cross the UniFFI boundary, which is the decision that keeps this whole
//! seam simple, so the sender here is an ordinary blocking socket on whatever
//! thread Swift calls it from.
//!
//! What it does *not* do is have its own idea of the wire format. The framing
//! and the sealing are [`acrylius_proto::bulk`], the same functions the tokio
//! transport calls, because two implementations of a wire format is exactly the
//! failure this project was started to escape.
//!
//! Receiving is not here. A phone cannot accept a connection in the background
//! and has nowhere to put a file, so it refuses an offer outright — see the
//! share plugin.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;

use acrylius_proto::bulk::{CHUNK, hello, seal};

use crate::types::FfiError;

/// Send a file to an endpoint the far end named.
///
/// Blocking, and deliberately so. Call it off the main thread; a transfer takes
/// as long as it takes.
///
/// The path never leaves the device that owns it: it is not in the offer, it is
/// not in any packet, and the peer at the other end of this socket only ever
/// learns how many bytes arrived.
#[uniffi::export]
pub fn bulk_send(
    transfer: u64,
    endpoint: String,
    key: Vec<u8>,
    path: String,
) -> Result<u64, FfiError> {
    send(transfer, &endpoint, &key, Path::new(&path)).map_err(|e| FfiError::Effect {
        detail: e.to_string(),
    })
}

fn send(transfer: u64, endpoint: &str, key: &[u8], path: &Path) -> std::io::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    let mut stream = TcpStream::connect(endpoint)?;
    stream.set_nodelay(true).ok();
    stream.write_all(&hello(transfer))?;

    let mut buf = vec![0u8; CHUNK];
    let mut sent: u64 = 0;
    let mut seq: u64 = 0;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let frame = seal(key, seq, &buf[..n]).map_err(std::io::Error::other)?;
        let len = u32::try_from(frame.len()).map_err(std::io::Error::other)?;
        stream.write_all(&len.to_be_bytes())?;
        stream.write_all(&frame)?;
        seq += 1;
        sent += n as u64;
    }
    // A clean shutdown is what tells the far end the file is finished. Its own
    // byte count is what tells it the file is whole.
    stream.shutdown(std::net::Shutdown::Write)?;
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A phone's sender and a computer's receiver, on one real socket.
    ///
    /// This is the test the whole arrangement exists for. The two ends have
    /// nothing in common but `acrylius_proto::bulk` — one runs on tokio and one
    /// on a blocking socket — so if the format were written twice, this is
    /// where the copies would disagree.
    #[tokio::test]
    async fn a_phone_sends_and_a_daemon_receives() {
        use acrylius_proto::bulk::{CHUNK, key};

        let dir = std::env::temp_dir().join(format!("acr-ffi-bulk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // More than one chunk, and not a whole number of them, so the sequence
        // numbering and the short final chunk are both exercised.
        let source = dir.join("holiday.jpg");
        let bytes: Vec<u8> = (0..CHUNK * 2 + 1234).map(|i| (i % 251) as u8).collect();
        std::fs::write(&source, &bytes).unwrap();

        let k = key(b"a shared handshake hash", 7);
        let listening = acrylius_rt::bulk::listen("127.0.0.1").await.unwrap();
        let endpoint = listening.endpoint.clone();
        let dest = dir.join("arrived.jpg");

        let sending = {
            let k = k.to_vec();
            let path = source.to_string_lossy().into_owned();
            tokio::task::spawn_blocking(move || bulk_send(7, endpoint, k, path))
        };
        let received = listening
            .receive(7, &k, bytes.len() as u64, &dest)
            .await
            .expect("the daemon reads what the phone wrote");

        assert_eq!(sending.await.unwrap().unwrap(), bytes.len() as u64);
        assert_eq!(received, bytes.len() as u64);
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            bytes,
            "every byte, unchanged"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A transfer nobody could have negotiated gets nothing and leaves nothing.
    #[tokio::test]
    async fn a_sender_without_the_session_key_writes_no_file() {
        use acrylius_proto::bulk::key;

        let dir = std::env::temp_dir().join(format!("acr-ffi-wrong-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("holiday.jpg");
        std::fs::write(&source, b"the real thing").unwrap();

        let listening = acrylius_rt::bulk::listen("127.0.0.1").await.unwrap();
        let endpoint = listening.endpoint.clone();
        let dest = dir.join("arrived.jpg");

        let sending = {
            let path = source.to_string_lossy().into_owned();
            // A key from a session this sender never had.
            let wrong = key(b"some other handshake", 7).to_vec();
            tokio::task::spawn_blocking(move || bulk_send(7, endpoint, wrong, path))
        };
        let outcome = listening
            .receive(7, &key(b"a shared handshake hash", 7), 14, &dest)
            .await;
        let _ = sending.await;

        assert!(outcome.is_err(), "nothing it sent would open");
        assert!(!dest.exists(), "and no file was left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_is_not_there_is_an_error_not_an_empty_transfer() {
        let e = bulk_send(
            1,
            "127.0.0.1:1".to_string(),
            vec![0u8; 32],
            "/nonexistent/holiday.jpg".to_string(),
        );
        assert!(e.is_err(), "and it is reported before anything is dialled");
    }
}

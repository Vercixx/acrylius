//! Blocking send/receive of a file, for hosts with no async runtime.
//!
//! Framing and sealing come from [`acrylius_proto::bulk`], the same functions the tokio transport uses, so the wire format has one implementation.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use acrylius_proto::bulk::{CHUNK, MAX_FRAME, hello, open, read_hello, seal};

use crate::types::FfiError;

/// Send a file to an endpoint the far end named. Blocking — call off the main thread.
/// The path itself never crosses the wire; the peer only learns how many bytes arrived.
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
    // Clean shutdown signals end-of-file; the byte count confirms completeness.
    stream.shutdown(std::net::Shutdown::Write)?;
    Ok(sent)
}

/// A name a peer chose, made safe to write. Shared with `acrylius_proto` so
/// both ends enforce the same path-traversal rule.
#[must_use]
#[uniffi::export]
pub fn bulk_safe_name(offered: String) -> String {
    acrylius_proto::bulk::safe_name(&offered)
}

/// A socket waiting for one transfer. Split into bind/accept/receive so
/// waiting for a sender can be abandoned without cutting off a file already
/// arriving.
#[derive(uniffi::Object)]
pub struct BulkListener {
    endpoint: String,
    /// Taken by `accept`; `Option` because a UniFFI object sits behind an
    /// `Arc` and can't be moved out of.
    listener: std::sync::Mutex<Option<TcpListener>>,
    /// Set by `accept`, consumed by `receive`. See `Event::BulkStarted`.
    connected: std::sync::Mutex<Option<TcpStream>>,
}

#[uniffi::export]
impl BulkListener {
    /// Bind a port for one transfer. `host` is the address the far end will be
    /// told to dial. Port zero, so the OS picks.
    #[uniffi::constructor]
    pub fn bind(host: String) -> Result<Self, FfiError> {
        let listener = TcpListener::bind(("0.0.0.0", 0)).map_err(|e| FfiError::Effect {
            detail: e.to_string(),
        })?;
        let port = listener
            .local_addr()
            .map_err(|e| FfiError::Effect {
                detail: e.to_string(),
            })?
            .port();
        Ok(Self {
            endpoint: format!("{host}:{port}"),
            listener: std::sync::Mutex::new(Some(listener)),
            connected: std::sync::Mutex::new(None),
        })
    }

    /// Where to tell the far end to connect. Valid before anything is listening.
    #[must_use]
    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    /// Blocks until the far end connects and confirms it's the expected
    /// transfer. Call off the main thread.
    pub fn accept(&self, transfer: u64) -> Result<(), FfiError> {
        let listener = self
            .listener
            .lock()
            .map_err(|_| FfiError::Effect {
                detail: "the listener is poisoned".to_string(),
            })?
            .take()
            .ok_or_else(|| FfiError::Effect {
                detail: "this listener has already taken its transfer".to_string(),
            })?;
        let stream = accept(&listener, transfer).map_err(|e| FfiError::Effect {
            detail: e.to_string(),
        })?;
        *self.connected.lock().map_err(|_| FfiError::Effect {
            detail: "the listener is poisoned".to_string(),
        })? = Some(stream);
        Ok(())
    }

    /// Write what arrives on the accepted connection to `path`. Blocking —
    /// call off the main thread. Written to a temp file beside `path` and
    /// renamed at the end, so an interrupted transfer never looks complete.
    pub fn receive(&self, key: Vec<u8>, expect_bytes: u64, path: String) -> Result<u64, FfiError> {
        let stream = self
            .connected
            .lock()
            .map_err(|_| FfiError::Effect {
                detail: "the listener is poisoned".to_string(),
            })?
            .take()
            .ok_or_else(|| FfiError::Effect {
                detail: "nothing has connected to this listener".to_string(),
            })?;
        receive(stream, &key, expect_bytes, Path::new(&path)).map_err(|e| FfiError::Effect {
            detail: e.to_string(),
        })
    }
}

fn temp_beside(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

fn accept(listener: &TcpListener, transfer: u64) -> std::io::Result<TcpStream> {
    let (mut stream, _from) = listener.accept()?;
    stream.set_nodelay(true).ok();

    let mut greeting = [0u8; 12];
    stream.read_exact(&mut greeting)?;
    let named = read_hello(&greeting).map_err(std::io::Error::other)?;
    if named != transfer {
        return Err(std::io::Error::other(format!(
            "that connection is for transfer {named}, not {transfer}"
        )));
    }
    Ok(stream)
}

fn receive(
    mut stream: TcpStream,
    key: &[u8],
    expect_bytes: u64,
    dest: &Path,
) -> std::io::Result<u64> {
    let tmp = temp_beside(dest);
    let outcome = (|| -> std::io::Result<u64> {
        let mut file = std::fs::File::create(&tmp)?;
        let mut written: u64 = 0;
        let mut seq: u64 = 0;
        loop {
            let mut len = [0u8; 4];
            match stream.read_exact(&mut len) {
                Ok(()) => {}
                // A clean shutdown is how the sender says it is finished.
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let len = u32::from_be_bytes(len);
            // Checked before allocating, not after: a length is the one field
            // an attacker sets for free.
            if len > MAX_FRAME {
                return Err(std::io::Error::other(format!(
                    "frame of {len} bytes is too big"
                )));
            }
            let mut frame = vec![0u8; len as usize];
            stream.read_exact(&mut frame)?;
            let plain = open(key, seq, &frame).map_err(std::io::Error::other)?;
            seq += 1;
            written += plain.len() as u64;
            if written > expect_bytes {
                return Err(std::io::Error::other("more arrived than was offered"));
            }
            file.write_all(&plain)?;
        }
        file.flush()?;
        if written != expect_bytes {
            return Err(std::io::Error::other(format!(
                "{written} bytes of {expect_bytes} arrived"
            )));
        }
        Ok(written)
    })();

    match outcome {
        Ok(written) => {
            std::fs::rename(&tmp, dest)?;
            Ok(written)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A phone's sender and a computer's receiver, on one real socket.
    #[tokio::test]
    async fn a_phone_sends_and_a_daemon_receives() {
        use acrylius_proto::bulk::{CHUNK, key};

        let dir = std::env::temp_dir().join(format!("acr-ffi-bulk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Not a whole number of chunks: exercises sequencing and a short final chunk.
        let source = dir.join("holiday.jpg");
        let bytes: Vec<u8> = (0..CHUNK * 2 + 1234).map(|i| (i % 251) as u8).collect();
        std::fs::write(&source, &bytes).unwrap();

        let k = key(b"a shared handshake hash", "the-offerer", 7);
        let listening = acrylius_rt::bulk::listen("127.0.0.1").await.unwrap();
        let endpoint = listening.endpoint.clone();
        let dest = dir.join("arrived.jpg");

        let sending = {
            let k = k.to_vec();
            let path = source.to_string_lossy().into_owned();
            tokio::task::spawn_blocking(move || bulk_send(7, endpoint, k, path))
        };
        let received = listening
            .accept(7)
            .await
            .expect("the phone connects")
            .receive(&k, bytes.len() as u64, &dest)
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
            let wrong = key(b"some other handshake", "the-offerer", 7).to_vec();
            tokio::task::spawn_blocking(move || bulk_send(7, endpoint, wrong, path))
        };
        let outcome = match listening.accept(7).await {
            // The greeting isn't secret, so a wrong key still connects; it just can't open a frame.
            Ok(connected) => {
                connected
                    .receive(
                        &key(b"a shared handshake hash", "the-offerer", 7),
                        14,
                        &dest,
                    )
                    .await
            }
            Err(e) => Err(e),
        };
        let _ = sending.await;

        assert!(outcome.is_err(), "nothing it sent would open");
        assert!(!dest.exists(), "and no file was left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other direction: a daemon's tokio sender and a phone's blocking receiver.
    #[tokio::test]
    async fn a_daemon_sends_and_a_phone_receives() {
        use acrylius_proto::bulk::{CHUNK, key};

        let dir = std::env::temp_dir().join(format!("acr-ffi-recv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // More than one chunk, and not a whole number of them.
        let source = dir.join("build.ipa");
        let bytes: Vec<u8> = (0..CHUNK * 2 + 999).map(|i| (i % 251) as u8).collect();
        std::fs::write(&source, &bytes).unwrap();

        let k = key(b"a shared handshake hash", "the-offerer", 11);
        let listener = BulkListener::bind("127.0.0.1".to_string()).unwrap();
        let endpoint = listener.endpoint();
        assert!(
            endpoint.starts_with("127.0.0.1:") && !endpoint.ends_with(":0"),
            "the endpoint names a port the far end can dial: {endpoint}"
        );
        let dest = dir.join("arrived.ipa");

        let size = bytes.len() as u64;
        let receiving = {
            let k = k.to_vec();
            let path = dest.to_string_lossy().into_owned();
            tokio::task::spawn_blocking(move || {
                listener.accept(11)?;
                listener.receive(k, size, path)
            })
        };
        let sent = acrylius_rt::bulk::send(11, &endpoint, &k, &source)
            .await
            .expect("the daemon writes what the phone reads");

        assert_eq!(receiving.await.unwrap().unwrap(), sent);
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            bytes,
            "every byte, unchanged"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A short transfer must not leave something that looks like a whole file.
    #[tokio::test]
    async fn a_transfer_that_stops_early_leaves_nothing_behind() {
        use acrylius_proto::bulk::key;

        let dir = std::env::temp_dir().join(format!("acr-ffi-short-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("build.ipa");
        std::fs::write(&source, b"only fourteen").unwrap();

        let k = key(b"a shared handshake hash", "the-offerer", 12);
        let listener = BulkListener::bind("127.0.0.1".to_string()).unwrap();
        let endpoint = listener.endpoint();
        let dest = dir.join("arrived.ipa");

        let receiving = {
            let k = k.to_vec();
            let path = dest.to_string_lossy().into_owned();
            // Told to expect far more than is coming.
            tokio::task::spawn_blocking(move || {
                listener.accept(12)?;
                listener.receive(k, 9_000, path)
            })
        };
        let _ = acrylius_rt::bulk::send(12, &endpoint, &k, &source).await;

        assert!(
            receiving.await.unwrap().is_err(),
            "a file that arrived short is a failed transfer"
        );
        assert!(
            !dest.exists(),
            "and nothing is left where a whole file goes"
        );
        assert!(!temp_beside(&dest).exists(), "nor beside it, half written");
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

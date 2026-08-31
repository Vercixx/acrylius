//! Bulk file transfer over its own connection; file bytes never cross the core.
//!
//! The only auth is the per-transfer key derived from the Noise session. Chunk
//! nonces carry sequence numbers, so reorder, repeat, or drop fails to open.

use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use acrylius_proto::bulk::{CHUNK, MAX_FRAME, hello, open, read_hello, seal};

async fn write_frame(stream: &mut TcpStream, frame: &[u8]) -> anyhow::Result<()> {
    let len = u32::try_from(frame.len())?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(frame).await?;
    Ok(())
}

/// Read one frame, or `None` at a clean end of stream.
async fn read_frame(stream: &mut TcpStream) -> anyhow::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match stream.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let n = u32::from_be_bytes(len);
    if n > MAX_FRAME {
        anyhow::bail!("a frame of {n} bytes is past the cap");
    }
    let mut buf = vec![0u8; n as usize];
    stream.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

/// Where a listener ended up, and the work of accepting one connection.
pub struct Listening {
    pub endpoint: String,
    listener: TcpListener,
}

/// Start listening for one transfer. The OS picks the port; it travels to the
/// peer in the accept message.
pub async fn listen(advertise_host: &str) -> anyhow::Result<Listening> {
    let listener = TcpListener::bind(("0.0.0.0", 0)).await?;
    let port = listener.local_addr()?.port();
    Ok(Listening {
        endpoint: format!("{advertise_host}:{port}"),
        listener,
    })
}

/// A connection that has arrived and said which transfer it is. Separate from
/// [`Listening`] so waiting for a sender can be bounded on its own.
pub struct Accepted {
    stream: TcpStream,
}

impl Listening {
    /// Wait for one connection, and check it is for the transfer expected.
    pub async fn accept(self, transfer: u64) -> anyhow::Result<Accepted> {
        let (mut stream, from) = self.listener.accept().await?;
        tracing::debug!(%from, transfer, "bulk connection");

        let mut greeting = [0u8; 12];
        stream.read_exact(&mut greeting).await?;
        let named = read_hello(&greeting)?;
        if named != transfer {
            anyhow::bail!("that connection is for transfer {named}, not {transfer}");
        }
        Ok(Accepted { stream })
    }
}

impl Accepted {
    /// Write what arrives to `dest`, via a temporary renamed at the end, so an
    /// interrupted transfer never leaves something that looks complete.
    pub async fn receive(self, key: &[u8], expect_bytes: u64, dest: &Path) -> anyhow::Result<u64> {
        let mut stream = self.stream;
        let tmp = temp_beside(dest);
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut written: u64 = 0;
        let mut seq: u64 = 0;

        let outcome = async {
            while let Some(frame) = read_frame(&mut stream).await? {
                let plain = open(key, seq, &frame)?;
                seq += 1;
                written += plain.len() as u64;
                if written > expect_bytes {
                    anyhow::bail!("more arrived than was offered");
                }
                file.write_all(&plain).await?;
            }
            file.flush().await?;
            if written != expect_bytes {
                anyhow::bail!("{written} bytes of {expect_bytes} arrived");
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        drop(file);
        match outcome {
            Ok(()) => {
                tokio::fs::rename(&tmp, dest).await?;
                Ok(written)
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                Err(e)
            }
        }
    }
}

/// Connect to an endpoint the peer named and send `path`.
pub async fn send(transfer: u64, endpoint: &str, key: &[u8], path: &Path) -> anyhow::Result<u64> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut stream = TcpStream::connect(endpoint).await?;
    stream.set_nodelay(true).ok();
    stream.write_all(&hello(transfer)).await?;

    let mut buf = vec![0u8; CHUNK];
    let mut sent: u64 = 0;
    let mut seq: u64 = 0;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let frame = seal(key, seq, &buf[..n])?;
        write_frame(&mut stream, &frame).await?;
        seq += 1;
        sent += n as u64;
    }
    // A clean shutdown is the end-of-file signal.
    stream.shutdown().await?;
    Ok(sent)
}

/// A name beside the destination, so the final rename stays on one filesystem.
fn temp_beside(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

// Lives in `acrylius_proto::bulk`; a drifting duplicate of this rule would be
// a path traversal.
pub use acrylius_proto::bulk::safe_name;

/// A path in `dir` not already taken, claimed via `create_new` so two
/// transfers of one name cannot race to the same file. The empty file is the
/// reservation: the finisher renames its `.part` over it; a failure removes it.
pub fn reserve_path(dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    let claim = |candidate: &Path| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(candidate)
    };
    let candidate = dir.join(name);
    match claim(&candidate) {
        Ok(_) => return Ok(candidate),
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
        Err(_) => {}
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for n in 2..10_000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        match claim(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
            Err(_) => {}
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("ten thousand files are already called {name}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_file_goes_across_and_arrives_whole() {
        let dir = std::env::temp_dir().join(format!("acr-bulk-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("src.bin");
        let dest = dir.join("dest.bin");
        let _ = std::fs::remove_file(&dest);

        // Larger than one chunk, so the sequencing is actually exercised.
        let payload: Vec<u8> = (0..(CHUNK * 2 + 1234)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &payload).unwrap();

        let key = [7u8; 32];
        let listening = listen("127.0.0.1").await.unwrap();
        let endpoint = listening.endpoint.clone();
        let len = payload.len() as u64;
        let dest2 = dest.clone();
        let recv =
            tokio::spawn(
                async move { listening.accept(1).await?.receive(&key, len, &dest2).await },
            );

        send(1, &endpoint, &key, &src).await.unwrap();
        let got = recv.await.unwrap().unwrap();

        assert_eq!(got, len);
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_wrong_key_gets_nothing_and_leaves_nothing() {
        let dir = std::env::temp_dir().join(format!("acr-bulk-bad-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("src.bin");
        let dest = dir.join("dest.bin");
        std::fs::write(&src, b"secret enough").unwrap();

        let listening = listen("127.0.0.1").await.unwrap();
        let endpoint = listening.endpoint.clone();
        let dest2 = dest.clone();
        let recv = tokio::spawn(async move {
            listening
                .accept(1)
                .await?
                .receive(&[1u8; 32], 13, &dest2)
                .await
        });

        // The dialer knows the port and the transfer id, and neither helps.
        let _ = send(1, &endpoint, &[2u8; 32], &src).await;
        assert!(recv.await.unwrap().is_err(), "nothing should open");
        assert!(!dest.exists(), "and no file should be left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_truncated_transfer_is_a_failure_not_a_short_file() {
        let dir = std::env::temp_dir().join(format!("acr-bulk-short-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("src.bin");
        let dest = dir.join("dest.bin");
        std::fs::write(&src, b"twelve bytes").unwrap();

        let listening = listen("127.0.0.1").await.unwrap();
        let endpoint = listening.endpoint.clone();
        let dest2 = dest.clone();
        // Told to expect more than will arrive.
        let recv = tokio::spawn(async move {
            listening
                .accept(1)
                .await?
                .receive(&[3u8; 32], 999, &dest2)
                .await
        });
        let _ = send(1, &endpoint, &[3u8; 32], &src).await;

        assert!(recv.await.unwrap().is_err());
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_file_of_the_same_name_does_not_replace_the_first() {
        let dir = std::env::temp_dir().join(format!("acr-free-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let first = reserve_path(&dir, "photo.jpg").unwrap();
        std::fs::write(&first, b"one").unwrap();
        let second = reserve_path(&dir, "photo.jpg").unwrap();
        assert_ne!(first, second);
        assert!(second.to_string_lossy().contains("photo (2).jpg"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_transfers_of_one_name_do_not_share_a_destination() {
        // Both paths are decided before either transfer writes a byte.
        let dir = std::env::temp_dir().join(format!("acr-claim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let a = reserve_path(&dir, "photo.jpg").unwrap();
        let b = reserve_path(&dir, "photo.jpg").unwrap();
        assert_ne!(a, b, "one file made of two transfers is the bug");
        assert_ne!(a.with_extension("jpg.part"), b.with_extension("jpg.part"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

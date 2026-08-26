//! Moving a file over its own connection.
//!
//! The core hands out a key and then has nothing more to do with this. Nothing
//! here decodes an envelope, and no byte of a file crosses the state machine:
//! that is the point of a side channel, and it is why a hundred-megabyte
//! transfer does not stall every other message on the link.
//!
//! ## What the key does
//!
//! Everything. The key is derived from the Noise session both ends already
//! share, so a connection nobody else can produce ciphertext for is one only
//! the peer could have opened. There is no handshake here and no identity of
//! its own: an impostor that dials the port and names a transfer gets exactly
//! as far as its first chunk.
//!
//! ## Framing
//!
//! `u32` big-endian length, then that many bytes of ciphertext, capped so a
//! peer cannot name a length and make the other side reserve it before sending
//! anything. Each chunk is sealed under a nonce carrying its own sequence
//! number, so a chunk cannot be reordered, repeated or dropped without the next
//! one failing to open. The key is fresh per transfer, which is what makes
//! counting from zero safe.

use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use acrylius_proto::bulk::{CHUNK, MAX_FRAME, hello, read_hello};

/// A nonce from a chunk's sequence number.
///
/// The whole nonce is the counter, so two chunks of one transfer can never
/// share one. Reusing a nonce under one key is the failure this shape makes
/// impossible rather than merely unlikely.
fn nonce(seq: u64) -> Nonce {
    let mut out = [0u8; 12];
    out[4..].copy_from_slice(&seq.to_be_bytes());
    *Nonce::from_slice(&out)
}

fn cipher(key: &[u8]) -> anyhow::Result<ChaCha20Poly1305> {
    let key: [u8; 32] = key
        .try_into()
        .map_err(|_| anyhow::anyhow!("a bulk key is 32 bytes"))?;
    Ok(ChaCha20Poly1305::new((&key).into()))
}

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

/// Start listening for one transfer.
///
/// Port zero: the operating system picks, and the port travels to the peer in
/// the accept message. A fixed port would be one more thing to configure, one
/// more thing to collide, and one more thing to leave open.
pub async fn listen(advertise_host: &str) -> anyhow::Result<Listening> {
    let listener = TcpListener::bind(("0.0.0.0", 0)).await?;
    let port = listener.local_addr()?.port();
    Ok(Listening {
        endpoint: format!("{advertise_host}:{port}"),
        listener,
    })
}

impl Listening {
    /// Accept one connection and write what arrives to `dest`.
    ///
    /// Written to a temporary beside the destination and renamed at the end, so
    /// an interrupted transfer never leaves something that looks like a
    /// complete file. A short transfer is a failure and the temporary goes.
    pub async fn receive(
        self,
        transfer: u64,
        key: &[u8],
        expect_bytes: u64,
        dest: &Path,
    ) -> anyhow::Result<u64> {
        let cipher = cipher(key)?;
        let (mut stream, from) = self.listener.accept().await?;
        tracing::debug!(%from, transfer, "bulk connection");

        let mut greeting = [0u8; 12];
        stream.read_exact(&mut greeting).await?;
        let named = read_hello(&greeting)?;
        if named != transfer {
            anyhow::bail!("that connection is for transfer {named}, not {transfer}");
        }

        let tmp = temp_beside(dest);
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut written: u64 = 0;
        let mut seq: u64 = 0;

        let outcome = async {
            while let Some(frame) = read_frame(&mut stream).await? {
                let plain = cipher
                    .decrypt(&nonce(seq), frame.as_ref())
                    .map_err(|_| anyhow::anyhow!("chunk {seq} did not open"))?;
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
    let cipher = cipher(key)?;
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
        let frame = cipher
            .encrypt(&nonce(seq), &buf[..n])
            .map_err(|_| anyhow::anyhow!("could not seal chunk {seq}"))?;
        write_frame(&mut stream, &frame).await?;
        seq += 1;
        sent += n as u64;
    }
    // A clean shutdown is what tells the far end the file is finished. Its own
    // byte count is what tells it the file is whole.
    stream.shutdown().await?;
    Ok(sent)
}

/// A name beside the destination, so the rename at the end stays on one
/// filesystem and is therefore atomic.
fn temp_beside(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

/// A file name from a peer, made safe to use.
///
/// A peer chooses what to call its file and nothing else. Anything that could
/// steer where the bytes land is removed rather than rejected, because a
/// refusal over a stray slash helps nobody: `../../.bashrc` becomes `.bashrc`
/// in the directory that was going to be used anyway.
#[must_use]
pub fn safe_name(offered: &str) -> String {
    let base = offered
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(offered)
        .trim()
        .trim_start_matches('.');
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != '\0')
        .take(120)
        .collect();
    if cleaned.is_empty() {
        "received".to_string()
    } else {
        cleaned
    }
}

/// A path in `dir` that is not already taken.
///
/// A transfer never overwrites. Two photos with the same name is a normal thing
/// to happen and losing the first one is not.
#[must_use]
pub fn free_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for n in 2..10_000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_from_a_peer_cannot_choose_a_directory() {
        assert_eq!(safe_name("../../.bashrc"), "bashrc");
        assert_eq!(safe_name("/etc/passwd"), "passwd");
        assert_eq!(safe_name(r"C:\windows\system32\x.dll"), "x.dll");
        assert_eq!(safe_name("holiday.jpg"), "holiday.jpg");
    }

    #[test]
    fn a_name_that_is_nothing_useful_still_gets_one() {
        assert_eq!(safe_name(""), "received");
        assert_eq!(safe_name("   "), "received");
        assert_eq!(safe_name("../.."), "received");
    }

    #[test]
    fn a_control_character_does_not_survive() {
        // A name that rewrites the line it is printed on is a name nobody
        // should have to think about again.
        assert_eq!(safe_name("in\u{1b}[2Kvoice.pdf"), "in[2Kvoice.pdf");
        assert!(!safe_name("a\nb.txt").contains('\n'));
    }

    #[test]
    fn nonces_never_repeat_within_a_transfer() {
        let a = nonce(0);
        let b = nonce(1);
        assert_ne!(a, b);
        assert_eq!(
            nonce(7),
            nonce(7),
            "and are a function of the sequence alone"
        );
    }

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
        let recv = tokio::spawn(async move { listening.receive(1, &key, len, &dest2).await });

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
        let recv = tokio::spawn(async move { listening.receive(1, &[1u8; 32], 13, &dest2).await });

        // The dialer knows the port and the transfer id, and neither helps.
        let _ = send(1, &endpoint, &[2u8; 32], &src).await;
        assert!(recv.await.unwrap().is_err(), "nothing should open");
        assert!(!dest.exists(), "and no file should be left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_truncated_transfer_is_a_failure_not_a_short_file() {
        // The receiver's own byte count is what makes a half-arrived file a
        // failure rather than something that looks complete.
        let dir = std::env::temp_dir().join(format!("acr-bulk-short-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("src.bin");
        let dest = dir.join("dest.bin");
        std::fs::write(&src, b"twelve bytes").unwrap();

        let listening = listen("127.0.0.1").await.unwrap();
        let endpoint = listening.endpoint.clone();
        let dest2 = dest.clone();
        // Told to expect more than will arrive.
        let recv = tokio::spawn(async move { listening.receive(1, &[3u8; 32], 999, &dest2).await });
        let _ = send(1, &endpoint, &[3u8; 32], &src).await;

        assert!(recv.await.unwrap().is_err());
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_file_of_the_same_name_does_not_replace_the_first() {
        let dir = std::env::temp_dir().join(format!("acr-free-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let first = free_path(&dir, "photo.jpg");
        std::fs::write(&first, b"one").unwrap();
        let second = free_path(&dir, "photo.jpg");
        assert_ne!(first, second);
        assert!(second.to_string_lossy().contains("photo (2).jpg"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

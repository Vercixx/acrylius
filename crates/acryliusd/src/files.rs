//! Where a transferred file comes from and goes to.
//!
//! The core never learns what a file is and the transport never decides where
//! one lands. This is the only place that knows both, which is why it is here
//! and not in either of them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use acrylius_core::plugins::share::Offer;
use acrylius_core::vocab::TransferId;
use acrylius_rt::bulk::{self, Accepted, Listening};
use acrylius_rt::runtime::BulkHost;

/// A transfer we are sending.
struct Outgoing {
    path: PathBuf,
}

/// A transfer we have agreed to receive.
struct Incoming {
    dest: PathBuf,
    expect_bytes: u64,
    key: Vec<u8>,
    /// Waiting for a sender. Taken by `accept`, which leaves `connected` in its
    /// place: the two are never both here, and both being gone means this
    /// transfer is already being read.
    listening: Option<Listening>,
    connected: Option<Accepted>,
}

pub struct FileBulk {
    dir: PathBuf,
    /// What to tell a peer to connect to. See [`local_address`].
    host: String,
    /// Offers made to us, with who made them: answering one means sending
    /// a reply, and by then the event that carried the peer is long gone.
    offers: Mutex<BTreeMap<TransferId, (String, Offer)>>,
    outgoing: Mutex<BTreeMap<TransferId, Outgoing>>,
    incoming: Mutex<BTreeMap<TransferId, Incoming>>,
    /// Where a finished transfer's bytes went, until somebody asks once.
    landed: Mutex<BTreeMap<TransferId, PathBuf>>,
    next: AtomicU64,
}

impl FileBulk {
    pub fn new(dir: PathBuf, host: Option<String>) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            host: host.unwrap_or_else(local_address),
            offers: Mutex::new(BTreeMap::new()),
            outgoing: Mutex::new(BTreeMap::new()),
            incoming: Mutex::new(BTreeMap::new()),
            landed: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(0),
        })
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether a file could actually be written here.
    ///
    /// Asked at startup, by writing something and removing it, because the
    /// alternative is finding out during a transfer. The systemd unit runs
    /// under `ProtectHome=read-only`, so a directory outside its
    /// `ReadWritePaths=` exists, is listable, has the right owner and mode, and
    /// still refuses every write — nothing short of trying reveals that.
    pub fn writable(&self) -> Result<(), std::io::Error> {
        let probe = self.dir.join(".acrylius-write-test");
        std::fs::write(&probe, b"")?;
        let _ = std::fs::remove_file(&probe);
        Ok(())
    }

    /// Note a file to send, and give the transfer its id.
    ///
    /// The path stays here. Nothing above this ever sees one, so no plugin and
    /// no peer can name a file on this machine.
    pub fn offer(&self, path: PathBuf, size: u64, name: String, mime: String) -> Offer {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.outgoing
            .lock()
            .expect("bulk map poisoned")
            .insert(TransferId(id), Outgoing { path });
        Offer {
            transfer: id,
            name,
            size,
            mime,
        }
    }

    /// Remember an offer made to us, so accepting it later has a name to use.
    pub fn note_offer(&self, peer: &str, offer: Offer) {
        self.offers
            .lock()
            .expect("bulk map poisoned")
            .insert(TransferId(offer.transfer), (peer.to_string(), offer));
    }

    pub fn offered(&self, transfer: TransferId) -> Option<Offer> {
        self.offers
            .lock()
            .expect("bulk map poisoned")
            .get(&transfer)
            .map(|(_, o)| o.clone())
    }

    /// Who offered a transfer, so an answer can be sent back to them.
    pub fn peer_for(&self, transfer: u64) -> Option<String> {
        self.offers
            .lock()
            .expect("bulk map poisoned")
            .get(&TransferId(transfer))
            .map(|(peer, _)| peer.clone())
    }

    /// Everything waiting on a decision.
    pub fn pending(&self) -> Vec<Offer> {
        self.offers
            .lock()
            .expect("bulk map poisoned")
            .values()
            .map(|(_, o)| o.clone())
            .collect()
    }

    pub fn forget(&self, transfer: TransferId) {
        self.offers.lock().expect("poisoned").remove(&transfer);
        self.outgoing.lock().expect("poisoned").remove(&transfer);
        self.incoming.lock().expect("poisoned").remove(&transfer);
    }

    /// Where an accepted transfer will be written.
    ///
    /// Before it has been. Only the tests ask; see [`Self::landed`] for the
    /// question worth asking afterwards.
    #[cfg(test)]
    fn destination(&self, transfer: TransferId) -> Option<PathBuf> {
        self.incoming
            .lock()
            .expect("poisoned")
            .get(&transfer)
            .map(|i| i.dest.clone())
    }

    /// Where a transfer's bytes actually ended up.
    ///
    /// Taken, not read: it is asked once, to tell somebody where their file
    /// went. Keeping every path a machine has ever received would be a list of
    /// what somebody has been sent, which is nobody's business and is not worth
    /// holding to answer a question once.
    ///
    /// This is the only route by which a path leaves this type, it goes to a
    /// notification on this machine's own screen, and it goes nowhere near a
    /// peer.
    pub fn landed(&self, transfer: TransferId) -> Option<PathBuf> {
        self.landed.lock().expect("poisoned").remove(&transfer)
    }
}

#[async_trait::async_trait]
impl BulkHost for FileBulk {
    async fn listen(
        &self,
        transfer: TransferId,
        key: Vec<u8>,
        expect_bytes: u64,
    ) -> anyhow::Result<String> {
        let offer = self
            .offered(transfer)
            .ok_or_else(|| anyhow::anyhow!("no offer for transfer {}", transfer.0))?;

        // The peer chose a name and nothing else. It is made safe here, and a
        // file already there is never replaced: two photos with the same name
        // is a normal thing to happen and losing the first one is not.
        // Claimed here, not merely chosen. Two transfers of the same name
        // accepted at once both used to be told the name was free.
        let dest = bulk::reserve_path(&self.dir, &bulk::safe_name(&offer.name))?;
        let listening = bulk::listen(&self.host).await?;
        let endpoint = listening.endpoint.clone();

        self.incoming.lock().expect("poisoned").insert(
            transfer,
            Incoming {
                dest,
                expect_bytes,
                key,
                listening: Some(listening),
                connected: None,
            },
        );
        Ok(endpoint)
    }

    async fn accept(&self, transfer: TransferId) -> anyhow::Result<()> {
        // Taken out of the map, because a listener can only be accepted on
        // once and leaving it there would let a second call wait forever.
        let listening = {
            let mut map = self.incoming.lock().expect("poisoned");
            map.get_mut(&transfer)
                .ok_or_else(|| anyhow::anyhow!("nothing listening for {}", transfer.0))?
                .listening
                .take()
                .ok_or_else(|| anyhow::anyhow!("already accepted {}", transfer.0))?
        };
        // Awaited with nothing locked. This is the wait that lasts, and holding
        // the map across it would stop every other transfer on the machine.
        let accepted = listening.accept(transfer.0).await?;
        let mut map = self.incoming.lock().expect("poisoned");
        map.get_mut(&transfer)
            .ok_or_else(|| anyhow::anyhow!("{} was cancelled while waiting", transfer.0))?
            .connected = Some(accepted);
        Ok(())
    }

    async fn receive(&self, transfer: TransferId) -> anyhow::Result<()> {
        let (connected, dest, expect, key) = {
            let mut map = self.incoming.lock().expect("poisoned");
            let entry = map
                .get_mut(&transfer)
                .ok_or_else(|| anyhow::anyhow!("nothing listening for {}", transfer.0))?;
            let connected = entry
                .connected
                .take()
                .ok_or_else(|| anyhow::anyhow!("nothing has connected for {}", transfer.0))?;
            (
                connected,
                entry.dest.clone(),
                entry.expect_bytes,
                entry.key.clone(),
            )
        };

        let result = connected.receive(&key, expect, &dest).await;
        // Answered, either way. An offer left here would keep showing up as
        // waiting for a decision that was already made, and the next transfer
        // of a file by the same name would find the old id first.
        self.forget(transfer);
        match result {
            Ok(bytes) => {
                tracing::info!(path = %dest.display(), bytes, "received a file");
                self.landed
                    .lock()
                    .expect("poisoned")
                    .insert(transfer, dest.clone());
                Ok(())
            }
            Err(e) => {
                // The empty file that reserved this name goes with it. It was
                // created to stop a second transfer of the same name landing on
                // top of this one; leaving it behind would fill the download
                // directory with nothing, and make the next file of that name
                // arrive as "photo (2).jpg" for no reason a person can see.
                //
                // Only if it is still empty: a rename may have already put the
                // received bytes there and failed afterwards.
                if std::fs::metadata(&dest).is_ok_and(|m| m.len() == 0) {
                    let _ = std::fs::remove_file(&dest);
                }
                // Loud, because until now a transfer that failed said nothing
                // anywhere and looked exactly like one that had not happened.
                tracing::warn!(path = %dest.display(), error = %e, "a file did not arrive");
                Err(e)
            }
        }
    }

    async fn send(
        &self,
        transfer: TransferId,
        endpoint: String,
        key: Vec<u8>,
    ) -> anyhow::Result<()> {
        let path = {
            let map = self.outgoing.lock().expect("poisoned");
            map.get(&transfer)
                .map(|o| o.path.clone())
                .ok_or_else(|| anyhow::anyhow!("nothing to send for {}", transfer.0))?
        };
        let result = bulk::send(transfer.0, &endpoint, &key, &path).await;
        self.forget(transfer);
        let bytes = result?;
        tracing::info!(path = %path.display(), bytes, "sent a file");
        Ok(())
    }

    fn cancel(&self, transfer: TransferId) {
        self.forget(transfer);
    }
}

/// This machine's address on the network it routes over.
///
/// Found by asking the kernel which source address it would use to reach a
/// documentation address, which sends nothing and needs no reply. The
/// alternative — picking the first non-loopback interface — gets it wrong on
/// any machine with a VPN, a container bridge, or a second card.
fn local_address() -> String {
    use std::net::UdpSocket;
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            // TEST-NET-1. Routable enough for the kernel to choose an
            // interface, and not somewhere a packet would ever go.
            s.connect("192.0.2.1:9")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk_in(dir: &str) -> FileBulk {
        let dir = std::env::temp_dir().join(format!("{dir}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        FileBulk::new(dir, Some("127.0.0.1".to_string())).unwrap()
    }

    #[test]
    fn a_path_never_leaves_this_type() {
        // What goes to a peer is a name, a size and an id. If a path could
        // reach an offer, a peer would learn where files live on this machine.
        let b = bulk_in("acr-files-a");
        let offer = b.offer(
            PathBuf::from("/home/someone/secret/report.pdf"),
            10,
            "report.pdf".to_string(),
            "application/pdf".to_string(),
        );
        assert_eq!(offer.name, "report.pdf");
        let encoded = format!("{offer:?}");
        assert!(!encoded.contains("/home/someone"), "no path in the offer");
        let _ = std::fs::remove_dir_all(b.dir());
    }

    #[test]
    fn transfer_ids_do_not_repeat() {
        let b = bulk_in("acr-files-b");
        let first = b.offer(PathBuf::from("/a"), 1, "a".into(), String::new());
        let second = b.offer(PathBuf::from("/b"), 1, "b".into(), String::new());
        assert_ne!(first.transfer, second.transfer);
        let _ = std::fs::remove_dir_all(b.dir());
    }

    /// The systemd unit runs under `ProtectHome=read-only`, and a directory
    /// outside its `ReadWritePaths=` exists, lists, and has the right owner and
    /// mode while refusing every write. Nothing short of trying reveals it, and
    /// finding out halfway through receiving a file is how it was found.
    #[test]
    fn a_directory_that_cannot_be_written_to_says_so_before_a_transfer() {
        let b = bulk_in("acr-files-w");
        assert!(b.writable().is_ok(), "an ordinary directory");

        let mut perms = std::fs::metadata(b.dir()).unwrap().permissions();
        let readable = 0o555;
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(readable);
        }
        std::fs::set_permissions(b.dir(), perms).unwrap();
        assert!(b.writable().is_err(), "and one nothing may write to");

        let mut perms = std::fs::metadata(b.dir()).unwrap().permissions();
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
        }
        let _ = std::fs::set_permissions(b.dir(), perms);
        let _ = std::fs::remove_dir_all(b.dir());
    }

    #[tokio::test]
    async fn listening_for_an_offer_nobody_made_is_refused() {
        // A key without an offer is a transfer this device never agreed to.
        let b = bulk_in("acr-files-c");
        assert!(b.listen(TransferId(42), vec![0u8; 32], 10).await.is_err());
        let _ = std::fs::remove_dir_all(b.dir());
    }

    #[tokio::test]
    async fn a_transfer_that_is_over_stops_waiting_for_an_answer() {
        // It sat in the list forever, so `offers` showed decisions already
        // made, and a second file of the same name matched the finished id
        // first — accepting which waited on a transfer that no longer existed.
        let b = bulk_in("acr-files-e");
        b.note_offer(
            "peer",
            Offer {
                transfer: 7,
                name: "photo.bin".to_string(),
                size: 4,
                mime: String::new(),
            },
        );
        let key = vec![7u8; 32];
        let endpoint = b.listen(TransferId(7), key.clone(), 4).await.unwrap();
        assert_eq!(b.pending().len(), 1, "waiting on a decision");

        let source = b.dir().join("source.bin");
        std::fs::write(&source, b"data").unwrap();
        let sending = tokio::spawn(async move { bulk::send(7, &endpoint, &key, &source).await });
        b.accept(TransferId(7)).await.unwrap();
        b.receive(TransferId(7)).await.unwrap();
        sending.await.unwrap().unwrap();

        assert!(b.pending().is_empty(), "answered, so no longer waiting");
        let _ = std::fs::remove_dir_all(b.dir());
    }

    #[tokio::test]
    async fn a_name_from_a_peer_lands_in_the_download_directory() {
        let b = bulk_in("acr-files-d");
        b.note_offer(
            "peer",
            Offer {
                transfer: 1,
                name: "../../escape.txt".to_string(),
                size: 4,
                mime: String::new(),
            },
        );
        b.listen(TransferId(1), vec![0u8; 32], 4).await.unwrap();
        let dest = b.destination(TransferId(1)).unwrap();
        assert_eq!(dest.parent(), Some(b.dir()), "inside the directory, always");
        assert_eq!(dest.file_name().unwrap(), "escape.txt");
        let _ = std::fs::remove_dir_all(b.dir());
    }
}

//! Where a transferred file comes from and goes to — the only place that knows
//! both the path and the peer.

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
    /// Taken by `accept`, which leaves `connected` in its place; both gone
    /// means the transfer is already being read.
    listening: Option<Listening>,
    connected: Option<Accepted>,
    /// The number the sender greets us with, not the one this map is keyed by.
    offered_as: u64,
}

pub struct FileBulk {
    dir: PathBuf,
    /// What to tell a peer to connect to. See [`local_address`].
    host: String,
    /// Offers made to us, with who made them.
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

    /// Probed by writing: under `ProtectHome=read-only` a directory can have
    /// the right owner and mode and still refuse every write.
    pub fn writable(&self) -> Result<(), std::io::Error> {
        let probe = self.dir.join(".acrylius-write-test");
        std::fs::write(&probe, b"")?;
        let _ = std::fs::remove_file(&probe);
        Ok(())
    }

    /// Note a file to send, and give the transfer its id. The path stays here:
    /// no plugin or peer can name a file on this machine.
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

    /// The transfer a person meant, from the number they typed.
    ///
    /// Matched against offers actually waiting, rather than reconstructed from
    /// `TransferId::short`, so a number naming nothing is refused rather than misread as something else.
    pub fn resolve(&self, typed: u64) -> Option<TransferId> {
        self.offers
            .lock()
            .expect("bulk map poisoned")
            .keys()
            .find(|id| id.written_as(typed))
            .copied()
    }

    /// Who offered a transfer, so an answer can be sent back to them.
    pub fn peer_for(&self, transfer: TransferId) -> Option<String> {
        self.offers
            .lock()
            .expect("bulk map poisoned")
            .get(&transfer)
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
    /// Taken, not read, since it's asked once. This is the only route by which
    /// a path leaves this type, and it goes to a notification on this machine's own screen, never to a peer.
    pub fn landed(&self, transfer: TransferId) -> Option<PathBuf> {
        self.landed.lock().expect("poisoned").remove(&transfer)
    }
}

#[async_trait::async_trait]
impl BulkHost for FileBulk {
    async fn listen(
        &self,
        transfer: TransferId,
        offered_as: u64,
        key: Vec<u8>,
        expect_bytes: u64,
    ) -> anyhow::Result<String> {
        let offer = self
            .offered(transfer)
            .ok_or_else(|| anyhow::anyhow!("no offer for transfer {}", transfer.0))?;

        // The peer only chose a name; made safe here, and never replaces an existing file.
        // Claimed here, not just chosen, so two concurrent transfers with the same name can't both be told it's free.
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
                offered_as,
            },
        );
        Ok(endpoint)
    }

    async fn accept(&self, transfer: TransferId) -> anyhow::Result<()> {
        // Taken out of the map: a listener can only be accepted once.
        let (listening, offered_as) = {
            let mut map = self.incoming.lock().expect("poisoned");
            let entry = map
                .get_mut(&transfer)
                .ok_or_else(|| anyhow::anyhow!("nothing listening for {}", transfer.0))?;
            let offered_as = entry.offered_as;
            let listening = entry
                .listening
                .take()
                .ok_or_else(|| anyhow::anyhow!("already accepted {}", transfer.0))?;
            (listening, offered_as)
        };
        // Awaited with nothing locked: this wait can last, and holding the map would stall every other transfer.
        // Matched against the sender's number, not ours: the dialer writes the greeting and only knows its own numbering.
        let accepted = listening.accept(offered_as).await?;
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
        // Answered either way: leaving the offer here would look like an
        // undecided transfer and shadow the next one with the same name.
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
                // Remove the empty placeholder file too (only if still empty —
                // a rename may have placed the bytes and failed after), so a
                // failed transfer doesn't push the next same-named file to "photo (2).jpg".
                if std::fs::metadata(&dest).is_ok_and(|m| m.len() == 0) {
                    let _ = std::fs::remove_file(&dest);
                }
                // Loud: a failed transfer used to look exactly like one that never happened.
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
/// Asks the kernel which source address it'd use to reach a documentation
/// address (sends nothing); picking the first non-loopback interface instead
/// breaks with a VPN, bridge, or second NIC.
fn local_address() -> String {
    use std::net::UdpSocket;
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            // TEST-NET-1: routable enough to pick an interface, never actually reachable.
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
        // A peer only ever gets name/size/id — never the path, which would leak this machine's filesystem layout.
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

    /// Under `ProtectHome=read-only`, a directory outside `ReadWritePaths=`
    /// looks entirely normal (right owner, mode, lists fine) and only fails when something actually writes to it.
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
    async fn an_offer_is_answered_by_the_number_a_person_was_shown() {
        // Listed short, typed back short: the id's range-marker bit isn't something a person should have to retype.
        let b = bulk_in("acr-files-f");
        let id = TransferId(acrylius_core::vocab::MINTED_HERE | 3);
        b.note_offer(
            "peer",
            Offer {
                transfer: id.0,
                name: "photo.bin".to_string(),
                size: 4,
                mime: String::new(),
            },
        );
        assert_eq!(b.resolve(id.short()), Some(id), "the number as shown");
        assert_eq!(
            b.resolve(id.0),
            Some(id),
            "and the full one, which a script may have captured"
        );
        assert_eq!(
            b.resolve(4242),
            None,
            "a number naming nothing is refused, not invented into one"
        );
        let _ = std::fs::remove_dir_all(b.dir());
    }

    #[tokio::test]
    async fn listening_for_an_offer_nobody_made_is_refused() {
        // A key without an offer is a transfer this device never agreed to.
        let b = bulk_in("acr-files-c");
        assert!(
            b.listen(TransferId(42), 42, vec![0u8; 32], 10)
                .await
                .is_err()
        );
        let _ = std::fs::remove_dir_all(b.dir());
    }

    #[tokio::test]
    async fn a_transfer_that_is_over_stops_waiting_for_an_answer() {
        // Previously sat in the list forever, so a same-named file's new offer could match the old, finished id.
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
        // Numbered 7 here, 3 by the sender — the ordinary case, since each side
        // mints its own id. Checking the greeting against ours instead of the
        // sender's number broke every transfer; matching numbers would hide the bug.
        let key = vec![7u8; 32];
        let endpoint = b.listen(TransferId(7), 3, key.clone(), 4).await.unwrap();
        assert_eq!(b.pending().len(), 1, "waiting on a decision");

        let source = b.dir().join("source.bin");
        std::fs::write(&source, b"data").unwrap();
        let sending = tokio::spawn(async move { bulk::send(3, &endpoint, &key, &source).await });
        b.accept(TransferId(7)).await.unwrap();
        b.receive(TransferId(7)).await.unwrap();
        sending.await.unwrap().unwrap();
        assert_eq!(
            std::fs::read(b.landed(TransferId(7)).expect("it landed")).unwrap(),
            b"data",
            "the bytes have to arrive, not merely the negotiation"
        );

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
        b.listen(TransferId(1), 1, vec![0u8; 32], 4).await.unwrap();
        let dest = b.destination(TransferId(1)).unwrap();
        assert_eq!(dest.parent(), Some(b.dir()), "inside the directory, always");
        assert_eq!(dest.file_name().unwrap(), "escape.txt");
        let _ = std::fs::remove_dir_all(b.dir());
    }
}

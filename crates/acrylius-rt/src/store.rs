//! Persistence, for a Rust host.
//!
//! Plain files, no database. Peers number under ten, and the old project's
//! SQLite was there for a nonce table that the Noise session and a one-`u64`
//! watermark have made unnecessary.
//!
//! Every write is a create-then-rename, so a crash mid-write leaves the previous
//! record intact rather than a truncated one. A peer record with half a key in
//! it would be indistinguishable from a corrupted pairing.

use std::io;
use std::path::{Path, PathBuf};

use acrylius_core::peer::PeerRecord;
use acrylius_core::vocab::Sensitivity;

pub trait Store: Send {
    fn put(&mut self, key: &str, value: Option<&[u8]>, sensitivity: Sensitivity) -> io::Result<()>;
    fn load_peers(&self) -> io::Result<Vec<PeerRecord>>;
}

pub struct FileStore {
    root: PathBuf,
}

impl FileStore {
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("peer"))?;
        set_private(&root)?;
        Ok(Self { root })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Keys are `peer/<device-id>`. A device id is strict base64url, which
    /// contains no `/` and no `.`, so it cannot climb out of the directory, but
    /// reject anything unexpected anyway rather than rely on that.
    fn path_for(&self, key: &str) -> io::Result<PathBuf> {
        if key.contains("..") || key.starts_with('/') || key.matches('/').count() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bad key {key}"),
            ));
        }
        Ok(self.root.join(key))
    }
}

#[cfg(unix)]
fn set_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> io::Result<()> {
    Ok(())
}

impl Store for FileStore {
    fn put(&mut self, key: &str, value: Option<&[u8]>, sensitivity: Sensitivity) -> io::Result<()> {
        let path = self.path_for(key)?;
        let Some(bytes) = value else {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                other => other,
            };
        };
        let tmp = path.with_extension("tmp");
        {
            // Written and flushed to the disk before the rename, not merely to
            // the page cache. A rename is atomic with respect to *readers*, and
            // that is all it is: the kernel is free to commit the rename before
            // the bytes, so a machine that lost power here came back with a peer
            // file that existed and was empty. A paired phone became a stranger,
            // and the only way back was to pair it again.
            let file = std::fs::File::create(&tmp)?;
            if sensitivity == Sensitivity::Secret {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    // Before the bytes, so the key is never briefly readable.
                    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                }
            }
            let mut file = file;
            std::io::Write::write_all(&mut file, bytes)?;
            file.sync_all()?;
        }
        // Rename is atomic within a filesystem, so a reader sees either the old
        // record or the new one and never a partial write.
        std::fs::rename(&tmp, &path)?;
        // And the directory entry itself, or the rename is the thing that can be
        // lost. Best effort: a filesystem that will not open a directory for
        // this is not a reason to fail a write that has otherwise succeeded.
        if let Ok(dir) = std::fs::File::open(&self.root) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    fn load_peers(&self) -> io::Result<Vec<PeerRecord>> {
        let dir = self.root.join("peer");
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "tmp") {
                continue;
            }
            let bytes = std::fs::read(&path)?;
            match minicbor::decode::<PeerRecord>(&bytes) {
                Ok(r) => out.push(r),
                // A record we cannot read is a record we must not silently
                // treat as absent, because "absent" means "this peer is a
                // stranger" and that is a security-relevant difference.
                Err(e) => {
                    tracing::error!(path = %path.display(), error = %e, "unreadable peer record");
                }
            }
        }
        Ok(out)
    }
}

/// Discards everything. For tests and for a client that keeps nothing.
#[derive(Default)]
pub struct MemoryStore {
    pub entries: std::collections::BTreeMap<String, Vec<u8>>,
}

impl Store for MemoryStore {
    fn put(&mut self, key: &str, value: Option<&[u8]>, _s: Sensitivity) -> io::Result<()> {
        match value {
            Some(v) => {
                self.entries.insert(key.to_string(), v.to_vec());
            }
            None => {
                self.entries.remove(key);
            }
        }
        Ok(())
    }
    fn load_peers(&self) -> io::Result<Vec<PeerRecord>> {
        Ok(self
            .entries
            .iter()
            .filter(|(k, _)| k.starts_with("peer/"))
            .filter_map(|(_, v)| minicbor::decode(v).ok())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acr-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    #[cfg(unix)]
    fn a_directory_holding_a_private_key_is_not_readable_by_anyone_else() {
        // Mutation testing found this: `set_private` could be replaced with a
        // no-op and nothing objected. It is the only thing standing between the
        // device's long-term identity and every other account on the machine.
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("perms");
        let mut s = FileStore::open(&dir).unwrap();
        let mode = std::fs::metadata(s.root()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "the store directory");

        s.put("peer/aaa", Some(b"not a secret"), Sensitivity::Ordinary)
            .unwrap();
        s.put("peer/bbb", Some(b"key material"), Sensitivity::Secret)
            .unwrap();
        let secret = std::fs::metadata(dir.join("peer/bbb"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(secret, 0o600, "anything marked Secret");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_that_could_leave_its_directory_is_refused() {
        // `path_for`'s guard, which nothing asserted. A device id is strict
        // base64url and cannot contain any of these, so this is about the guard
        // surviving a future caller rather than about today's ids.
        let dir = scratch("paths");
        let mut s = FileStore::open(&dir).unwrap();
        for bad in [
            "peer/../../etc/passwd",
            "/etc/passwd",
            "peer/a/b",
            "nodir",
            "..",
        ] {
            assert!(
                s.put(bad, Some(b"x"), Sensitivity::Ordinary).is_err(),
                "{bad} should be refused"
            );
        }
        assert!(
            s.put("peer/legit", Some(b"x"), Sensitivity::Ordinary)
                .is_ok()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

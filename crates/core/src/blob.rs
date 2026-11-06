use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn is_address(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Content-addressed files at `<data dir>/blobs/<sha256>`. Calls block on the filesystem.
#[derive(Debug, Clone)]
pub struct BlobStore {
    dir: PathBuf,
}

impl BlobStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join("blobs"),
        }
    }

    fn path(&self, sha256: &str) -> Result<PathBuf> {
        if is_address(sha256) {
            Ok(self.dir.join(sha256))
        } else {
            Err(Error::InvalidBlobAddress)
        }
    }

    /// Stores `bytes` and returns their address. A file already at that address is only replaced
    /// if its content is wrong.
    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let sha256 = sha256_hex(bytes);
        let path = self.path(&sha256)?;
        match fs::read(&path) {
            Ok(existing) if sha256_hex(&existing) == sha256 => return Ok(sha256),
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        fs::create_dir_all(&self.dir)?;
        let temp = self.dir.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let written = write_synced(&temp, bytes).and_then(|()| fs::rename(&temp, &path));
        if let Err(e) = written {
            let _ = fs::remove_file(&temp);
            return Err(e.into());
        }
        File::open(&self.dir)?.sync_all()?;
        Ok(sha256)
    }

    /// Reads a blob and verifies it against its address.
    pub fn get(&self, sha256: &str) -> Result<Vec<u8>> {
        let bytes = fs::read(self.path(sha256)?)?;
        if sha256_hex(&bytes) == sha256 {
            Ok(bytes)
        } else {
            Err(Error::BlobCorrupt(sha256.to_owned()))
        }
    }

    pub fn exists(&self, sha256: &str) -> Result<bool> {
        Ok(self.path(sha256)?.try_exists()?)
    }

    /// Removes a blob if present. Blobs are shared: check that no document still references it,
    /// holding the database's write lock across the check and this call.
    pub fn delete(&self, sha256: &str) -> Result<()> {
        match fs::remove_file(self.path(sha256)?) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Addresses of all stored blobs.
    pub fn list(&self) -> Result<Vec<String>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut addresses = Vec::new();
        for entry in entries {
            if let Some(name) = entry?.file_name().to_str().filter(|n| is_address(n)) {
                addresses.push(name.to_owned());
            }
        }
        addresses.sort();
        Ok(addresses)
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn store() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        (dir, store)
    }

    #[test]
    fn put_get_list_delete_round_trip() {
        let (_dir, store) = store();
        assert_eq!(store.put(b"hello").unwrap(), HELLO);
        assert_eq!(store.put(b"hello").unwrap(), HELLO);
        assert_eq!(store.get(HELLO).unwrap(), b"hello");
        assert_eq!(store.list().unwrap(), [HELLO]);
        assert!(store.exists(HELLO).unwrap());
        store.delete(HELLO).unwrap();
        store.delete(HELLO).unwrap();
        assert!(!store.exists(HELLO).unwrap());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn put_leaves_no_temp_files() {
        let (dir, store) = store();
        store.put(b"hello").unwrap();
        let names: Vec<_> = fs::read_dir(dir.path().join("blobs")).unwrap().collect();
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn get_detects_corruption_and_put_repairs_it() {
        let (dir, store) = store();
        store.put(b"hello").unwrap();
        fs::write(dir.path().join("blobs").join(HELLO), b"tampered").unwrap();
        assert!(matches!(store.get(HELLO), Err(Error::BlobCorrupt(_))));
        store.put(b"hello").unwrap();
        assert_eq!(store.get(HELLO).unwrap(), b"hello");
    }

    #[test]
    fn addresses_cannot_escape_the_directory() {
        let (_dir, store) = store();
        for bad in ["../../etc/passwd", "ABC", "", &HELLO.to_uppercase()] {
            assert!(
                matches!(store.get(bad), Err(Error::InvalidBlobAddress)),
                "{bad}"
            );
        }
    }
}

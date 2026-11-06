//! Preview PNGs on disk, `<data>/cache/renders/`. Backups copy only the database and its blobs, so
//! they skip this, and anything may delete it: every failure here means one compile more.
//!
//! An entry is only ever written after the integrity check passed, and its key holds the source's
//! hash and the facts version it passed against, so a facts change can't show an old result.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// About a thousand one-page previews.
const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct RenderCache {
    dir: PathBuf,
    cap: u64,
}

impl RenderCache {
    /// Starts empty: a new binary may render or judge differently, and the key can't see that.
    pub fn open(data_dir: &Path) -> Self {
        Self::with_cap(data_dir.join("cache").join("renders"), MAX_CACHE_BYTES)
    }

    fn with_cap(dir: PathBuf, cap: u64) -> Self {
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir);
        Self { dir, cap }
    }

    fn path(&self, source_sha256: &str, facts_version: i64) -> PathBuf {
        self.dir.join(format!("{source_sha256}-{facts_version}"))
    }

    pub fn get(&self, source_sha256: &str, facts_version: i64) -> Option<Vec<Vec<u8>>> {
        decode(&fs::read(self.path(source_sha256, facts_version)).ok()?)
    }

    /// Written under a temporary name and renamed, so a reader never sees half a file.
    pub fn put(&self, source_sha256: &str, facts_version: i64, pages: &[Vec<u8>]) {
        let path = self.path(source_sha256, facts_version);
        let temporary = path.with_extension(format!("tmp{}", std::process::id()));
        let written = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)
            .and_then(|mut file| file.write_all(&encode(pages)))
            .and_then(|()| fs::rename(&temporary, &path));
        if let Err(error) = written {
            tracing::warn!(error = %error.kind(), "could not cache a preview");
            let _ = fs::remove_file(&temporary);
        }
        self.trim();
    }

    /// Oldest first, until the cache fits again.
    fn trim(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
            .flatten()
            .filter_map(|entry| {
                let meta = entry.metadata().ok()?;
                Some((meta.modified().ok()?, meta.len(), entry.path()))
            })
            .collect();
        let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
        files.sort();
        for (_, size, path) in files {
            if total <= self.cap {
                break;
            }
            if fs::remove_file(path).is_ok() {
                total -= size;
            }
        }
    }
}

fn encode(pages: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for page in pages {
        out.extend_from_slice(&(page.len() as u32).to_le_bytes());
        out.extend_from_slice(page);
    }
    out
}

fn decode(mut bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut pages = Vec::new();
    while !bytes.is_empty() {
        let (length, rest) = bytes.split_first_chunk::<4>()?;
        let length = u32::from_le_bytes(*length) as usize;
        if rest.len() < length {
            return None;
        }
        let (page, rest) = rest.split_at(length);
        pages.push(page.to_vec());
        bytes = rest;
    }
    Some(pages)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn an_entry_belongs_to_one_source_and_one_facts_version() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RenderCache::open(dir.path());
        cache.put("abc", 3, &[b"one".to_vec(), b"second page".to_vec()]);
        assert_eq!(
            cache.get("abc", 3).unwrap(),
            [b"one".to_vec(), b"second page".to_vec()]
        );
        assert!(cache.get("abc", 4).is_none());
        assert!(cache.get("abd", 3).is_none());
        assert!(dir.path().join("cache/renders/abc-3").exists());
    }

    #[test]
    fn the_cap_removes_the_oldest_entries_first() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RenderCache::with_cap(dir.path().join("renders"), 250);
        for (index, key) in ["a", "b", "c"].iter().enumerate() {
            cache.put(key, 1, &[vec![index as u8; 100]]);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(cache.get("a", 1).is_none(), "the oldest went");
        assert!(cache.get("b", 1).is_some() && cache.get("c", 1).is_some());
    }

    #[test]
    fn a_corrupt_entry_reads_as_a_miss_and_a_restart_empties_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RenderCache::open(dir.path());
        cache.put("abc", 1, &[vec![1, 2, 3]]);
        fs::write(cache.path("bad", 1), [200, 0, 0, 0, 1]).unwrap();
        assert!(cache.get("bad", 1).is_none());
        let reopened = RenderCache::open(dir.path());
        assert!(reopened.get("abc", 1).is_none());
    }
}

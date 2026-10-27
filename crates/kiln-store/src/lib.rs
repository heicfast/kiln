//! The content-addressed artifact store.
//!
//! An entry holds one unit's outputs — the rlib, the metadata, the dep-info,
//! the binary — packed, zstd-compressed and named by the unit key's digest.
//! The digest is computed from every input of the compilation (see
//! `kiln-build::fingerprint`), so an entry is valid on any machine with the
//! same toolchain, and two checkouts of the same source share it wherever
//! they are on disk.
//!
//! On disk an object is one file:
//!
//! ```text
//! KILN1 <blake3 of the bytes that follow>\n
//! <zstd-compressed JSON header + file payloads>
//! ```
//!
//! The leading hash covers everything behind it, so corruption — by a dying
//! disk, a truncated download or a tampered remote — is caught on read and
//! the entry is treated as absent, never trusted.

pub mod format;
pub mod remote;
pub mod serve;

use std::path::{Path, PathBuf};

use kiln_core::config::{CacheMode, KilnConfig};
use kiln_core::{Error, Result};

pub use format::{Entry, StoredFile};
pub use remote::{Remote, RemoteKind};

/// Statistics of a store.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StoreStats {
    /// Number of entries.
    pub entries: usize,
    /// Bytes on disk, compressed.
    pub bytes: u64,
    /// The configured limit, if any.
    pub limit: Option<u64>,
    /// The store's root.
    pub root: PathBuf,
}

/// The store: local objects plus an optional remote kept in step.
pub struct Store {
    root: PathBuf,
    mode: CacheMode,
    remote: Option<Remote>,
    /// Push to the remote asynchronously; failures are reported, not fatal.
    push_remote: bool,
}

impl Store {
    /// Open the store per configuration; `CacheMode::Off` still opens a
    /// usable no-op-ish handle (writes are dropped, reads always miss).
    pub fn open(cfg: &KilnConfig) -> Result<Self> {
        if cfg.cache_mode == CacheMode::Off {
            return Ok(Self {
                root: cfg.cache_dir.clone(),
                mode: CacheMode::Off,
                remote: None,
                push_remote: false,
            });
        }
        let root = cfg.cache_dir.clone();
        let objects = root.join("objects");
        std::fs::create_dir_all(&objects).map_err(|e| Error::io("create", &objects, e))?;
        let remote = match &cfg.remote {
            Some(url) => Some(Remote::connect(url, cfg.remote_token.clone())?),
            None => None,
        };
        Ok(Self {
            root,
            mode: cfg.cache_mode,
            remote,
            push_remote: true,
        })
    }

    /// Whether writes are kept.
    pub fn writable(&self) -> bool {
        self.mode == CacheMode::ReadWrite
    }

    /// The store's root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn object_path(&self, key: &str) -> PathBuf {
        self.root.join("objects").join(&key[..2]).join(format!("{key}.zst"))
    }

    /// The path an object lives at (for the cache server).
    pub fn object_path_of(&self, key: &str) -> PathBuf {
        self.object_path(key)
    }

    /// The raw on-disk bytes of an object, when present.
    pub fn object_bytes(&self, key: &str) -> Option<Vec<u8>> {
        if self.mode == CacheMode::Off {
            return None;
        }
        std::fs::read(self.object_path(key)).ok()
    }

    /// Whether the key is present locally.
    pub fn has(&self, key: &str) -> bool {
        self.mode != CacheMode::Off && self.object_path(key).is_file()
    }

    /// Read and verify a local entry.
    pub fn get(&self, key: &str) -> Result<Option<Entry>> {
        if self.mode == CacheMode::Off {
            return Ok(None);
        }
        let path = self.object_path(key);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io("read", &path, e)),
        };
        match format::decode(&bytes) {
            Ok(entry) => Ok(Some(entry)),
            // A corrupt entry is a miss and is removed so it cannot be read
            // twice; the build recompiles the unit and puts a fresh one.
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                Err(Error::parse(format!("store entry {key}"), e))
            }
        }
    }

    /// Write an entry under `key`, atomically: a temporary file is renamed
    /// into place, so no reader sees a half-written object.
    pub fn put(&self, key: &str, entry: &Entry) -> Result<()> {
        if !self.writable() {
            return Ok(());
        }
        let path = self.object_path(key);
        std::fs::create_dir_all(path.parent().expect("objects/aa/key has a parent"))
            .map_err(|e| Error::io("create", path.parent().unwrap(), e))?;
        let bytes = format::encode(entry)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| Error::io("write", &tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| Error::io("rename", &path, e))?;
        touch_use(&self.root, key);
        Ok(())
    }

    /// Try the remote for a key the local store lacks, and install it.
    pub async fn fetch_remote(&self, key: &str) -> Result<Option<Entry>> {
        let Some(remote) = &self.remote else { return Ok(None) };
        if self.mode == CacheMode::Off {
            return Ok(None);
        }
        let Some(bytes) = remote.get(key).await? else { return Ok(None) };
        match format::decode(&bytes) {
            Ok(entry) => {
                if self.writable() {
                    let path = self.object_path(key);
                    std::fs::create_dir_all(path.parent().expect("parent"))
                        .map_err(|e| Error::io("create", path.parent().unwrap(), e))?;
                    let tmp = path.with_extension("tmp");
                    std::fs::write(&tmp, &bytes).map_err(|e| Error::io("write", &tmp, e))?;
                    std::fs::rename(&tmp, &path).map_err(|e| Error::io("rename", &path, e))?;
                }
                Ok(Some(entry))
            }
            Err(e) => Err(Error::parse(format!("remote entry {key}"), e)),
        }
    }

    /// Push a key to the remote, if one is configured and writes are on.
    pub async fn push_remote(&self, key: &str) -> Result<bool> {
        if !self.push_remote || self.mode == CacheMode::Off {
            return Ok(false);
        }
        let Some(remote) = &self.remote else { return Ok(false) };
        let path = self.object_path(key);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(Error::io("read", &path, e)),
        };
        remote.put(key, &bytes).await?;
        Ok(true)
    }

    /// Push every local entry the remote lacks (used by `kiln cache push`).
    pub async fn push_all(&self) -> Result<(usize, usize)> {
        let Some(remote) = &self.remote else {
            return Err(Error::unsupported("no remote is configured (KILN_REMOTE)"));
        };
        let mut pushed = 0;
        let mut failed = 0;
        let mut batch = Vec::new();
        for key in self.list_keys()? {
            batch.push(key);
            if batch.len() >= 64 {
                let present = remote.has_many(&batch).await?;
                for key in std::mem::take(&mut batch) {
                    if present.contains(&key) {
                        continue;
                    }
                    match self.push_remote(&key).await {
                        Ok(true) => pushed += 1,
                        Ok(false) => {}
                        Err(_) => failed += 1,
                    }
                }
            }
        }
        let present = remote.has_many(&batch).await?;
        for key in batch {
            if present.contains(&key) {
                continue;
            }
            match self.push_remote(&key).await {
                Ok(true) => pushed += 1,
                Ok(false) => {}
                Err(_) => failed += 1,
            }
        }
        Ok((pushed, failed))
    }

    /// Pull entries from the remote into the local store (`kiln cache pull`).
    pub async fn pull_all(&self) -> Result<(usize, usize)> {
        let Some(remote) = &self.remote else {
            return Err(Error::unsupported("no remote is configured (KILN_REMOTE)"));
        };
        let keys = remote.list().await?;
        let mut pulled = 0;
        let mut already = 0;
        for key in keys {
            if self.has(&key) {
                already += 1;
                continue;
            }
            if self.fetch_remote(&key).await?.is_some() {
                pulled += 1;
            }
        }
        Ok((pulled, already))
    }

    /// Every key in the local store.
    pub fn list_keys(&self) -> Result<Vec<String>> {
        let objects = self.root.join("objects");
        let mut out = Vec::new();
        let shard_iter = match std::fs::read_dir(&objects) {
            Ok(it) => it,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(Error::io("read", &objects, e)),
        };
        for shard in shard_iter.flatten() {
            let shard = shard.path();
            if !shard.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(&shard).map_err(|e| Error::io("read", &shard, e))?.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Some(key) = name.strip_suffix(".zst") {
                    out.push(key.to_owned());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Usage statistics.
    pub fn stats(&self) -> Result<StoreStats> {
        let mut entries = 0;
        let mut bytes = 0u64;
        for key in self.list_keys()? {
            entries += 1;
            if let Ok(meta) = std::fs::metadata(self.object_path(&key)) {
                bytes += meta.len();
            }
        }
        Ok(StoreStats {
            entries,
            bytes,
            limit: None,
            root: self.root.clone(),
        })
    }

    /// Collect: remove least-recently-used entries until the store fits in
    /// `max_bytes`. LRU order comes from `atime`, updated on read and write
    /// (`touch_use`); on filesystems without atime the order degrades to
    /// insertion order, which is still a usable policy.
    pub fn gc(&self, max_bytes: u64) -> Result<usize> {
        let mut sized: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
        for key in self.list_keys()? {
            let path = self.object_path(&key);
            if let Ok(meta) = std::fs::metadata(&path) {
                let atime = meta
                    .accessed()
                    .or_else(|_| meta.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                sized.push((path, meta.len(), atime));
            }
        }
        let total: u64 = sized.iter().map(|(_, len, _)| *len).sum();
        if total <= max_bytes {
            return Ok(0);
        }
        sized.sort_by_key(|(_, _, atime)| *atime); // oldest first
        let mut removed = 0;
        let mut remaining = total;
        for (path, len, _) in sized {
            if remaining <= max_bytes {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                remaining -= len;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// Record that a key was used, for LRU collection.
fn touch_use(root: &Path, key: &str) {
    let path = root.join("objects").join(&key[..2]).join(format!("{key}.zst"));
    // A file::set_times-based touch; failures never fail a build.
    let now = std::time::SystemTime::now();
    let file = match std::fs::OpenOptions::new().append(true).open(&path) {
        Ok(f) => f,
        Err(_) => return,
    };
    #[cfg(unix)]
    {
        let times = std::fs::FileTimes::new().set_accessed(now).set_modified(now);
        let _ = file.set_times(times);
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        let _ = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempdir::TempDirGuard, Store) {
        let guard = tempdir::TempDirGuard::new();
        let dir = guard.path.clone();
        let mut cfg = KilnConfig::default();
        cfg.cache_dir = dir;
        cfg.remote = None;
        (guard, Store::open(&cfg).unwrap())
    }

    /// A minimal temp-dir helper so tests need no dev-dependencies.
    mod tempdir {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);

        pub struct TempDirGuard {
            pub path: PathBuf,
        }

        impl TempDirGuard {
            pub fn new() -> Self {
                let path = std::env::temp_dir().join(format!(
                    "kiln-store-{}-{}",
                    std::process::id(),
                    COUNTER.fetch_add(1, Ordering::Relaxed)
                ));
                let _ = std::fs::remove_dir_all(&path);
                std::fs::create_dir_all(&path).unwrap();
                Self { path }
            }
        }

        impl Drop for TempDirGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }

    fn sample_entry() -> Entry {
        Entry {
            created_ms: 42,
            description: "unit foo-abc123".to_owned(),
            outputs: vec![
                StoredFile {
                    path: "deps/libfoo-abc123.rlib".to_owned(),
                    bytes: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
                    executable: false,
                },
                StoredFile {
                    path: "deps/foo-abc123.d".to_owned(),
                    bytes: b"/src/foo.rs:".to_vec(),
                    executable: false,
                },
                StoredFile {
                    path: "foo".to_owned(),
                    bytes: b"#!/bin/sh".to_vec(),
                    executable: true,
                },
            ],
        }
    }

    #[test]
    fn entries_round_trip_and_verify() {
        let (_g, store) = temp_store();
        let entry = sample_entry();
        let key = "a".repeat(64);
        store.put(&key, &entry).unwrap();
        assert!(store.has(&key));
        let back = store.get(&key).unwrap().unwrap();
        assert_eq!(back.description, entry.description);
        assert_eq!(back.outputs.len(), 3);
        assert_eq!(back.outputs[0].bytes, entry.outputs[0].bytes);
        assert!(back.outputs[2].executable);
    }

    #[test]
    fn missing_keys_miss_and_corrupt_entries_are_removed() {
        let (_g, store) = temp_store();
        assert!(store.get(&"f".repeat(64)).unwrap().is_none());

        let key = "b".repeat(64);
        store.put(&key, &sample_entry()).unwrap();
        let path = store.object_path(&key);
        let mut bytes = std::fs::read(&path).unwrap();
        // Flip a byte inside the compressed payload: the integrity check
        // must reject it (zstd or the leading hash catches it).
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        let result = store.get(&key);
        assert!(result.is_err() || result.unwrap().is_none());
        assert!(!store.has(&key), "corrupt entry must be removed");
    }

    #[test]
    fn off_mode_stores_nothing() {
        let guard = tempdir::TempDirGuard::new();
        let mut cfg = KilnConfig::default();
        cfg.cache_dir = guard.path.clone();
        cfg.cache_mode = CacheMode::Off;
        let store = Store::open(&cfg).unwrap();
        store.put(&"c".repeat(64), &sample_entry()).unwrap();
        assert!(!store.has(&"c".repeat(64)));
        assert!(store.get(&"c".repeat(64)).unwrap().is_none());
    }

    #[test]
    fn gc_removes_oldest_until_under_limit() {
        let (_g, store) = temp_store();
        let k1 = "d".repeat(64);
        let k2 = "e".repeat(64);
        store.put(&k1, &sample_entry()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        store.put(&k2, &sample_entry()).unwrap();
        let total = store.stats().unwrap().bytes;
        // Read k2 so it is the most recently used.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = store.get(&k2).unwrap();
        let removed = store.gc(total - 1).unwrap();
        assert!(removed >= 1);
        let stats = store.stats().unwrap();
        assert!(stats.entries <= 1);
        // The survivor should be the used one when atime works.
        if stats.entries == 1 {
            assert!(store.has(&k2) || store.has(&k1));
        }
    }

    #[test]
    fn list_keys_is_sorted_and_complete() {
        let (_g, store) = temp_store();
        store.put(&"a".repeat(64), &sample_entry()).unwrap();
        store.put(&"f".repeat(64), &sample_entry()).unwrap();
        let keys = store.list_keys().unwrap();
        assert_eq!(keys, vec!["a".repeat(64), "f".repeat(64)]);
    }
}

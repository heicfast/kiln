//! Entry serialisation: the `KILN1` object format with end-to-end integrity.

use std::fmt;

use kiln_core::{Error, Result};

/// One stored file inside an entry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredFile {
    /// Path relative to the profile directory (e.g. `deps/libfoo-hash.rlib`).
    pub path: String,
    /// The file's bytes.
    pub bytes: Vec<u8>,
    /// Whether the executable bit is on.
    pub executable: bool,
}

/// One unit's outputs, packed for the store.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// A human line for `kiln cache stats` and logs.
    pub description: String,
    /// The files this entry materialises.
    pub outputs: Vec<StoredFile>,
    /// When the entry was created (unix millis).
    pub created_ms: u128,
}

/// The format version this kiln writes.
pub const FORMAT: &str = "KILN1";

impl Entry {
    /// Build an entry from file paths on disk, reading each one.
    pub fn from_files(description: &str, files: &[(String, std::path::PathBuf, bool)]) -> Result<Self> {
        let mut outputs = Vec::with_capacity(files.len());
        for (rel, path, executable) in files {
            let bytes = std::fs::read(path).map_err(|e| Error::io("read", path, e))?;
            outputs.push(StoredFile { path: rel.clone(), bytes, executable: *executable });
        }
        Ok(Entry {
            description: description.to_owned(),
            outputs,
            created_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        })
    }

    /// Write every stored file under `base`, with the executable bit set
    /// where recorded. Directories are created as needed.
    pub fn materialize(&self, base: &std::path::Path) -> Result<()> {
        for file in &self.outputs {
            let dest = base.join(&file.path);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| Error::io("create", parent, e))?;
            }
            let tmp = dest.with_extension("kiln-tmp");
            std::fs::write(&tmp, &file.bytes).map_err(|e| Error::io("write", &tmp, e))?;
            std::fs::rename(&tmp, &dest).map_err(|e| Error::io("rename", &dest, e))?;
            #[cfg(unix)]
            if file.executable {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o755);
                std::fs::set_permissions(&dest, perms).map_err(|e| Error::io("chmod", &dest, e))?;
            }
        }
        Ok(())
    }
}

/// Encode an entry to the on-disk object format.
pub fn encode(entry: &Entry) -> Result<Vec<u8>> {
    let header = serde_json::to_vec(entry).map_err(|e| Error::parse("store entry", e))?;
    let mut z = zstd::bulk::Compressor::new(3).map_err(|e| Error::parse("zstd", e.to_string()))?;
    let compressed = z.compress(&header).map_err(|e| Error::parse("zstd", e.to_string()))?;
    let hash = blake3::hash(&compressed);
    let mut out = Vec::with_capacity(compressed.len() + 80);
    out.extend_from_slice(FORMAT.as_bytes());
    out.push(b' ');
    out.extend_from_slice(hex(hash.as_bytes()).as_bytes());
    out.push(b'\n');
    out.extend_from_slice(&compressed);
    Ok(out)
}

/// Decode and integrity-check an object.
pub fn decode(bytes: &[u8]) -> std::result::Result<Entry, FormatError> {
    let newline = bytes
        .iter()
        .position(|&b| b == b'\n')
        .ok_or(FormatError::Truncated("no header line"))?;
    let header = std::str::from_utf8(&bytes[..newline]).map_err(|_| FormatError::Truncated("header is not utf-8"))?;
    let (magic, hash_hex) = header
        .split_once(' ')
        .ok_or(FormatError::Truncated("no space in header"))?;
    if magic != FORMAT {
        return Err(FormatError::WrongFormat(magic.to_owned()));
    }
    let hash_hex = hash_hex.trim();
    let actual = blake3::hash(&bytes[newline + 1..]);
    if hex(actual.as_bytes()) != hash_hex {
        return Err(FormatError::Corrupt {
            expected: hash_hex.to_owned(),
            actual: hex(actual.as_bytes()),
        });
    }
    let mut d = zstd::bulk::Decompressor::new().map_err(|e| FormatError::Zstd(e.to_string()))?;
    let plain = d
        .decompress(&bytes[newline + 1..], 64 << 20)
        .map_err(|e| FormatError::Zstd(e.to_string()))?;
    serde_json::from_slice(&plain).map_err(FormatError::Json)
}

/// Why an object could not be decoded.
#[derive(Debug)]
pub enum FormatError {
    /// The file ends before its header.
    Truncated(&'static str),
    /// A foreign format.
    WrongFormat(String),
    /// The integrity hash does not match.
    Corrupt {
        /// What the header promised.
        expected: String,
        /// What the bytes hash to.
        actual: String,
    },
    /// zstd failed.
    Zstd(String),
    /// The JSON header is invalid.
    Json(serde_json::Error),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Truncated(what) => write!(f, "truncated object: {what}"),
            FormatError::WrongFormat(m) => write!(f, "unknown store format `{m}`"),
            FormatError::Corrupt { expected, actual } => {
                write!(f, "integrity check failed: expected {expected}, got {actual}")
            }
            FormatError::Zstd(e) => write!(f, "zstd: {e}"),
            FormatError::Json(e) => write!(f, "json header: {e}"),
        }
    }
}

impl std::error::Error for FormatError {}

impl From<FormatError> for Error {
    fn from(e: FormatError) -> Self {
        Error::parse("store object", e)
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push(TABLE[(b >> 4) as usize] as char);
        out.push(TABLE[(b & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trips() {
        let entry = Entry {
            description: "test unit".into(),
            outputs: vec![StoredFile {
                path: "deps/x.rlib".into(),
                bytes: (0..1000u32).map(|i| (i % 251) as u8).collect(),
                executable: false,
            }],
            created_ms: 12345,
        };
        let bytes = encode(&entry).unwrap();
        assert!(bytes.starts_with(b"KILN1 "));
        let back = decode(&bytes).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn tampered_bytes_fail_the_integrity_check() {
        let entry = Entry { description: "x".into(), outputs: vec![], created_ms: 0 };
        let mut bytes = encode(&entry).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        match decode(&bytes) {
            Err(FormatError::Corrupt { .. }) => {}
            other => panic!("expected corruption, got {other:?}"),
        }
    }

    #[test]
    fn foreign_formats_are_named() {
        assert!(matches!(decode(b"NOTIT abc\n"), Err(FormatError::WrongFormat(_))));
    }

    #[test]
    fn materialize_writes_files_and_modes() {
        let entry = Entry {
            description: "m".into(),
            outputs: vec![
                StoredFile { path: "deps/a.rlib".into(), bytes: b"rlib!".to_vec(), executable: false },
                StoredFile { path: "bin/tool".into(), bytes: b"#!/bin/sh".to_vec(), executable: true },
            ],
            created_ms: 0,
        };
        let dir = std::env::temp_dir().join(format!("kiln-mat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        entry.materialize(&dir).unwrap();
        assert_eq!(std::fs::read(dir.join("deps/a.rlib")).unwrap(), b"rlib!");
        assert!(dir.join("bin/tool").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("bin/tool")).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "executable bit not set");
            let mode = std::fs::metadata(dir.join("deps/a.rlib")).unwrap().permissions().mode();
            assert!(mode & 0o111 == 0, "executable bit should be off");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Downloading and extracting `.crate` tarballs, and where extracted sources
//! live: `~/.cache/kiln/registry/src/{name}-{version}`.

use std::io::Read;
use std::path::{Path, PathBuf};

use kiln_core::{Error, Result};
use semver::Version;
use sha2::{Digest, Sha256};

/// Where extracted registry sources live.
pub fn registry_src_dir(name: &str, version: &Version) -> PathBuf {
    crate::registry_cache_root()
        .join("src")
        .join(format!("{name}-{version}"))
}

/// The on-disk home of downloaded `.crate` tarballs.
pub fn registry_cache_dir() -> PathBuf {
    crate::registry_cache_root().join("cache")
}

/// The download URL of one crate version against the crates.io CDN.
pub fn crate_download_url(name: &str, version: &Version) -> String {
    format!("https://static.crates.io/crates/{name}/{name}-{version}.crate")
}

/// Registry sources on disk: what the build engine reads from.
pub struct RegistrySources {
    http: reqwest::Client,
    /// `--offline`: never touch the network.
    pub offline: bool,
}

impl RegistrySources {
    /// A client with kiln's user agent.
    pub fn new(offline: bool) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("kiln/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Network {
                context: "building the download client".into(),
                source: Box::new(e),
            })?;
        Ok(Self { http, offline })
    }

    /// Make sure the sources of `name`-`version` are extracted locally,
    /// verifying the sha256 against the index. Returns the package root.
    pub async fn ensure(
        &self,
        name: &str,
        version: &Version,
        checksum: Option<&str>,
    ) -> Result<PathBuf> {
        let src = registry_src_dir(name, version);
        if src.join("Cargo.toml").is_file() {
            return Ok(src);
        }
        if self.offline {
            return Err(Error::Resolve(format!(
                "sources of `{name} {version}` are not in the local registry cache and the network is off (--offline)"
            )));
        }
        let tarball = self.download(name, version, checksum).await?;
        extract_crate(&tarball, &src)?;
        Ok(src)
    }

    async fn download(
        &self,
        name: &str,
        version: &Version,
        checksum: Option<&str>,
    ) -> Result<PathBuf> {
        let cache_dir = registry_cache_dir();
        std::fs::create_dir_all(&cache_dir).map_err(|e| Error::io("create", &cache_dir, e))?;
        let file = cache_dir.join(format!("{name}-{version}.crate"));
        if file.is_file() {
            if checksum_matches(&file, checksum) {
                return Ok(file);
            }
            // A corrupted cache entry is removed, not trusted.
            std::fs::remove_file(&file).map_err(|e| Error::io("remove", &file, e))?;
        }
        let url = crate_download_url(name, version);
        let bytes = self
            .http
            .get(&url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| Error::Network {
                context: format!("downloading {name} {version}"),
                source: Box::new(e),
            })?
            .bytes()
            .await
            .map_err(|e| Error::Network {
                context: format!("reading {name} {version}"),
                source: Box::new(e),
            })?;
        if let Some(expected) = checksum {
            let actual = hex_sha256(&bytes);
            if actual != expected.to_ascii_lowercase() {
                return Err(Error::Network {
                    context: format!("downloading {name} {version}"),
                    source: Box::new(std::io::Error::other(format!(
                        "checksum mismatch: index says {expected}, download says {actual}"
                    ))),
                });
            }
        }
        let tmp = file.with_extension("part");
        std::fs::write(&tmp, &bytes).map_err(|e| Error::io("write", &tmp, e))?;
        std::fs::rename(&tmp, &file).map_err(|e| Error::io("rename", &file, e))?;
        Ok(file)
    }
}

/// Extract a `.crate` tarball (one `name-version/` root) into `dest`.
pub fn extract_crate(tarball: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(tarball).map_err(|e| Error::io("open", tarball, e))?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    let staging = dest.with_extension("extracting");
    std::fs::create_dir_all(&staging).map_err(|e| Error::io("create", &staging, e))?;
    archive
        .unpack(&staging)
        .map_err(|e| Error::parse(tarball.to_string_lossy(), e))?;
    // The tarball has a single top-level directory; move its contents up.
    let mut it = std::fs::read_dir(&staging).map_err(|e| Error::io("read", &staging, e))?;
    let inner = it
        .next()
        .ok_or_else(|| Error::missing(tarball, "the archive is empty"))?;
    let inner = inner.map_err(|e| Error::io("read", &staging, e))?.path();
    if !inner.is_dir() {
        return Err(Error::parse(
            tarball.to_string_lossy(),
            "the archive's first entry is not a directory",
        ));
    }
    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(|e| Error::io("remove", dest, e))?;
    }
    std::fs::rename(&inner, dest).map_err(|e| Error::io("rename", dest, e))?;
    std::fs::remove_dir_all(&staging).ok();
    Ok(())
}

fn checksum_matches(file: &Path, checksum: Option<&str>) -> bool {
    let Some(expected) = checksum else {
        return true;
    };
    let mut hasher = Sha256::new();
    let Ok(mut f) = std::fs::File::open(file) else {
        return false;
    };
    let mut buf = [0u8; 64 * 1024];
    while let Ok(n) = f.read(&mut buf) {
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    hex(hasher.finalize()) == expected.to_ascii_lowercase()
}

fn hex_sha256(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes))
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_urls_are_the_crates_io_layout() {
        let url = crate_download_url("serde", &Version::new(1, 0, 210));
        assert_eq!(
            url,
            "https://static.crates.io/crates/serde/serde-1.0.210.crate"
        );
    }

    #[test]
    fn a_tarball_extracts_to_its_package_root() {
        let dir = std::env::temp_dir().join(format!("kiln-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tarball = dir.join("demo.crate");
        let file = std::fs::File::create(&tarball).unwrap();
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut archive = tar::Builder::new(gz);
        let mut header = {
            let mut h = tar::Header::new_gnu();
            h.set_size(0);
            h.set_mode(0o644);
            h.set_cksum();
            h
        };
        archive
            .append_data(&mut header, "demo-0.1.0/Cargo.toml", std::io::empty())
            .unwrap();
        archive
            .append_data(&mut header, "demo-0.1.0/src/lib.rs", std::io::empty())
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();

        let dest = dir.join("out");
        extract_crate(&tarball, &dest).unwrap();
        assert!(dest.join("Cargo.toml").is_file());
        assert!(dest.join("src/lib.rs").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

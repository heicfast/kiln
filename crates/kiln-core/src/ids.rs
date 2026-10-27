//! Package, source and target-triple identity.

use std::fmt;
use std::path::{Path, PathBuf};

use semver::Version;

/// Where a package's files come from.
///
/// The registry source is the crates.io sparse index by default; path sources
/// cover workspace members and path dependencies.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum SourceId {
    /// A registry, named by its download URL (crates.io unless replaced).
    Registry(String),
    /// A path on this machine, canonicalised when constructed.
    Path(PathBuf),
}

impl SourceId {
    /// The canonical crates.io sparse registry.
    pub const CRATES_IO: &'static str = "sparse+https://index.crates.io/";

    /// The crates.io registry source id.
    pub fn crates_io() -> Self {
        SourceId::Registry(Self::CRATES_IO.to_owned())
    }

    /// Whether this is a registry (as opposed to a local path).
    pub fn is_registry(&self) -> bool {
        matches!(self, SourceId::Registry(_))
    }

    /// The registry URL, if this is a registry source.
    pub fn registry_url(&self) -> Option<&str> {
        match self {
            SourceId::Registry(url) => Some(url),
            SourceId::Path(_) => None,
        }
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceId::Registry(url) => write!(f, "registry+{url}"),
            SourceId::Path(p) => write!(f, "path+{}", p.display()),
        }
    }
}

/// A package's identity: name, version and where it comes from.
///
/// Two `PackageId`s are equal only when all three agree, which is what makes
/// them safe keys for cache namespacing and graph nodes.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct PackageId {
    /// The crate name, as the manifest spells it.
    pub name: String,
    /// The exact version.
    pub version: Version,
    /// Where its files live.
    pub source: SourceId,
}

impl PackageId {
    /// A package id for a path-resolved package.
    pub fn path(name: impl Into<String>, version: Version, root: &Path) -> Self {
        Self {
            name: name.into(),
            version,
            source: SourceId::Path(root.to_path_buf()),
        }
    }

    /// A package id for a registry-resolved package.
    pub fn registry(name: impl Into<String>, version: Version, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version,
            source: SourceId::Registry(url.into()),
        }
    }

    /// `name-version`, the layout the registry and cargo both use on disk.
    pub fn dir_name(&self) -> String {
        format!("{}-{}", self.name, self.version)
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            SourceId::Registry(_) => write!(f, "{} {}", self.name, self.version),
            SourceId::Path(p) => write!(f, "{} {} (path {})", self.name, self.version, p.display()),
        }
    }
}

/// A compiled-for target: the host, or a `--target` triple.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct TargetTriple(pub String);

impl TargetTriple {
    /// The triple for the machine kiln runs on; discovered by asking rustc.
    pub fn host() -> Self {
        // The build engine probes `rustc -vV`; the fallback name is only used
        // by pure unit tests that never spawn a compiler.
        if let Ok(vv) = std::process::Command::new("rustc").arg("-vV").output() {
            let text = String::from_utf8_lossy(&vv.stdout);
            for line in text.lines() {
                if let Some(host) = line.strip_prefix("host: ") {
                    return TargetTriple(host.trim().to_owned());
                }
            }
        }
        TargetTriple(std::env::consts::ARCH.to_string() + "-unknown-none")
    }
}

impl fmt::Display for TargetTriple {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_ids_order_stably() {
        let a = SourceId::crates_io();
        let b = SourceId::Path(PathBuf::from("/x"));
        assert_ne!(a, b);
        assert!(a.to_string().starts_with("registry+"));
        assert!(b.to_string().starts_with("path+"));
    }

    #[test]
    fn package_id_dir_name_matches_registry_layout() {
        let id = PackageId::registry("serde", Version::new(1, 2, 3), SourceId::CRATES_IO);
        assert_eq!(id.dir_name(), "serde-1.2.3");
        assert_eq!(id.to_string(), "serde 1.2.3");
    }
}

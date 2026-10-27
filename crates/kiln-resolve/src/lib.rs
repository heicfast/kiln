//! Dependency resolution: from a workspace and a lockfile to a fully
//! determined set of packages with their sources on disk.
//!
//! Resolution prefers the lockfile: what cargo locked is what kiln builds,
//! byte for byte. Only when the lock is missing or no longer satisfies the
//! manifests does the resolver run, and its result is written back as a
//! lockfile cargo accepts unchanged.

pub mod features;
pub mod index;
pub mod resolver;
pub mod source;

pub use features::resolve_features;
pub use index::{IndexCache, IndexDependency, IndexEntry};
pub use resolver::{ResolvedPackage, ResolvedWorkspace, resolve};
pub use source::{RegistrySources, extract_crate, registry_src_dir};

use std::path::PathBuf;

/// The registry cache root: `~/.cache/kiln/registry`.
pub fn registry_cache_root() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|h| PathBuf::from(h).join(".cache"))
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("kiln").join("registry")
}

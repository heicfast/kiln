//! Cargo.toml, workspace and Cargo.lock parsing.
//!
//! This crate reads what cargo reads — the same files, the same semantics —
//! and refuses (rather than half-understands) the parts of the manifest
//! language it does not model: a manifest kiln misreads is a build that
//! differs from cargo's, which is the one failure mode that matters.

pub mod cfg;
pub mod dependency;
pub mod lockfile;
pub mod manifest;
pub mod targets;
pub mod workspace;

pub use cfg::eval_cfg;
pub use dependency::{DepKind, Dependency};
pub use lockfile::{LockDep, LockPackage, Lockfile};
pub use manifest::{LibKind, Manifest, PackageTarget};
pub use targets::{TargetSpec, discover_targets};
pub use workspace::{Workspace, WorkspaceMember};

/// An edition, old to new; the default when a manifest names none is 2015,
/// exactly as cargo's.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Edition {
    /// The 2015 edition.
    E2015,
    /// The 2018 edition.
    E2018,
    /// The 2021 edition.
    E2021,
    /// The 2024 edition.
    E2024,
}

impl Edition {
    /// Parse from a manifest string.
    pub fn parse(s: &str) -> kiln_core::Result<Self> {
        match s {
            "2015" => Ok(Edition::E2015),
            "2018" => Ok(Edition::E2018),
            "2021" => Ok(Edition::E2021),
            "2024" => Ok(Edition::E2024),
            other => Err(kiln_core::Error::parse(
                "package.edition",
                format!("unknown edition `{other}`"),
            )),
        }
    }

    /// The rustc `--edition` spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Edition::E2015 => "2015",
            Edition::E2018 => "2018",
            Edition::E2021 => "2021",
            Edition::E2024 => "2024",
        }
    }

    /// The resolver this edition implies (edition 2021+ = resolver 2).
    pub fn default_resolver(self) -> u8 {
        if self >= Edition::E2021 { 2 } else { 1 }
    }
}

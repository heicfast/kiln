//! Content fingerprints: what a unit's key covers.
//!
//! A unit's inputs are its package's files, the resolved feature set, the
//! compiler-facing profile, the exact toolchain, the target, and the keys of
//! every unit it consumes. Hashing is blake3 over sorted file contents,
//! parallelised; the result is the cache key, and two builds with the same
//! key are the same build.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kiln_core::unit::{CompileMode, Unit, UnitKey};
use kiln_core::Result;
use kiln_resolve::ResolvedWorkspace;

use crate::toolchain::Toolchain;

/// A per-invocation fingerprinter. It holds no caches yet — hashing is fast
/// (blake3 runs at GB/s) and the daemon layer adds stat-based memoisation
/// where it pays.
pub struct Fingerprinter<'a> {
    resolved: &'a ResolvedWorkspace,
    toolchain: &'a Toolchain,
    target: &'a str,
    profile_fingerprint: u64,
    rustflags: &'a [String],
}

/// The file content hash of one hashed input.
#[derive(Debug, Clone, Copy)]
pub struct InputHash(pub [u8; 32]);

impl<'a> Fingerprinter<'a> {
    /// New fingerprinter for one build's shared facts.
    pub fn new(
        resolved: &'a ResolvedWorkspace,
        toolchain: &'a Toolchain,
        target: &'a str,
        profile_fingerprint: u64,
        rustflags: &'a [String],
    ) -> Self {
        Self { resolved, toolchain, target, profile_fingerprint, rustflags }
    }

    /// The input hash of one unit: everything about the unit except its
    /// dependencies' artifacts (the graph pass adds those).
    pub fn unit_input_hash(
        &self,
        unit: &Unit,
        build_script_output: Option<&[u8]>,
        extra_env: &BTreeMap<String, String>,
    ) -> Result<InputHash> {
        let pkg = self.resolved.package(&unit.pkg.name)?;
        let root = &pkg.manifest.root;
        let mut hasher = blake3::Hasher::new();

        // What is being built.
        hasher.update(unit.pkg.name.as_bytes());
        hasher.update(unit.pkg.version.to_string().as_bytes());
        hasher.update(unit.crate_name().as_bytes());
        hasher.update(unit.target_path.as_bytes());
        hasher.update(format!("{:?}", unit.kind).as_bytes());
        hasher.update(format!("{}", unit.mode).as_bytes());
        hasher.update(format!("{}", unit.test).as_bytes());

        // The compile settings.
        hasher.update(self.toolchain.rustc_version.as_bytes());
        hasher.update(self.toolchain.host.as_bytes());
        hasher.update(self.target.as_bytes());
        hasher.update(&self.profile_fingerprint.to_le_bytes());
        for flag in self.rustflags {
            hasher.update(flag.as_bytes());
        }

        // The features on this package, sorted (BTreeSet iterates sorted).
        for feature in unit.features.iter() {
            hasher.update(feature.as_bytes());
        }

        // The build script's outputs, when the package has one.
        if let Some(output) = build_script_output {
            hasher.update(b"build-script:");
            hasher.update(output);
        }

        // Environment variables the compile reads (build script outputs).
        for (key, value) in extra_env {
            hasher.update(b"env:");
            hasher.update(key.as_bytes());
            hasher.update(b"=");
            hasher.update(value.as_bytes());
        }

        // Every file of the package, sorted by path: over-invalidation is
        // correct (a module added by an edit changes the hash), and a
        // package's files hash in milliseconds.
        let files = package_files(root)?;
        for (rel, bytes) in files {
            hasher.update(b"file:");
            hasher.update(rel.as_bytes());
            hasher.update(b"\0");
            hasher.update(&bytes);
        }

        Ok(InputHash(*hasher.finalize().as_bytes()))
    }

    /// Assemble the full unit key, with dependencies folded in by artifact
    /// digest.
    pub fn unit_key(
        &self,
        unit: &Unit,
        input_hash: InputHash,
        dep_artifacts: &[(String, [u8; 32])],
    ) -> UnitKey {
        UnitKey {
            unit: unit.clone(),
            profile_fingerprint: self.profile_fingerprint,
            target: self.target.to_owned(),
            host: self.toolchain.host.clone(),
            rustc: self.toolchain.rustc_version.clone(),
            dep_artifacts: dep_artifacts.to_vec(),
            input_hash: input_hash.0,
        }
    }
}

/// Read every file of a package (excluding `target/`, dot-directories and
/// non-source odds and ends), sorted by relative path, in parallel.
pub fn package_files(root: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let mut paths = Vec::new();
    collect_files(root, root, &mut paths)?;
    paths.sort();
    let contents: Vec<(String, std::io::Result<Vec<u8>>)> = paths
        .into_iter()
        .map(|(rel, abs)| {
            let bytes = std::fs::read(&abs);
            (rel, bytes)
        })
        .collect();
    let mut out = Vec::with_capacity(contents.len());
    for (rel, bytes) in contents {
        let bytes = bytes.map_err(|e| kiln_core::Error::io("read", root.join(&rel), e))?;
        out.push((rel, bytes));
    }
    Ok(out)
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> kiln_core::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(kiln_core::Error::io("read", dir, e)),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "target" || name.starts_with('.') || name == "Cargo.lock" {
            continue;
        }
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, path));
        }
    }
    Ok(())
}

/// The mode a unit compiles in, for fingerprinting and argv building.
pub fn emit_for(mode: CompileMode, kind: kiln_core::unit::TargetKind) -> &'static str {
    match (mode, kind) {
        (CompileMode::Check, _) => "dep-info,metadata",
        (CompileMode::Build | CompileMode::Test, kiln_core::unit::TargetKind::BuildScript) => "dep-info,link",
        (CompileMode::Build | CompileMode::Test, _) => "dep-info,metadata,link",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_core::ids::PackageId;
    use kiln_core::unit::{TargetKind, Unit};
    use semver::Version;
    use std::collections::BTreeSet;

    fn unit_for(pkg_name: &str) -> Unit {
        Unit {
            pkg: PackageId::path(pkg_name, Version::new(0, 1, 0), Path::new("/ws")),
            target_name: pkg_name.to_owned(),
            target_path: "src/lib.rs".to_owned(),
            kind: TargetKind::Lib,
            test: false,
            mode: CompileMode::Build,
            features: BTreeSet::new(),
        }
    }

    fn fixture_workspace(dir: &Path, name: &str, file_text: &str) -> ResolvedWorkspace {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), file_text).unwrap();
        let manifest = Manifest::parse(dir).unwrap();
        let mut packages = BTreeMap::new();
        packages.insert(
            name.to_owned(),
            kiln_resolve::ResolvedPackage {
                id: PackageId::path(name, Version::new(0, 1, 0), dir),
                manifest,
                deps: BTreeMap::new(),
                features: BTreeSet::new(),
            },
        );
        ResolvedWorkspace { packages, reused_lock: true }
    }

    use kiln_manifest::Manifest;

    #[test]
    fn hashes_change_with_every_modelled_input() {
        let dir = tempfile::tempdir().unwrap();
        let ws = fixture_workspace(dir.path(), "app", "fn a() {}");
        let tc = Toolchain {
            rustc: PathBuf::from("/bin/rustc"),
            rustc_version: "1.99.0 (abc)".into(),
            host: "x86_64-unknown-test".into(),
            linker: crate::toolchain::LinkerChoice::Default,
        };
        let fp = Fingerprinter::new(&ws, &tc, "x86_64-unknown-test", 7, &[]);
        let unit = unit_for("app");
        let base = fp.unit_input_hash(&unit, None, &BTreeMap::new()).unwrap();

        // Same inputs, same hash.
        let again = fp.unit_input_hash(&unit, None, &BTreeMap::new()).unwrap();
        assert_eq!(base.0, again.0);

        // A source edit changes it.
        std::fs::write(dir.path().join("src/lib.rs"), "fn a() { 1 }").unwrap();
        let ws2 = fixture_workspace(dir.path(), "app", "fn a() { 1 }");
        let fp2 = Fingerprinter::new(&ws2, &tc, "x86_64-unknown-test", 7, &[]);
        let edited = fp2.unit_input_hash(&unit, None, &BTreeMap::new()).unwrap();
        assert_ne!(base.0, edited.0);

        // A new file changes it (over-invalidation is correct).
        std::fs::write(dir.path().join("src/new.rs"), "fn b() {}").unwrap();
        let ws3 = fixture_workspace(dir.path(), "app", "fn a() { 1 }");
        let fp3 = Fingerprinter::new(&ws3, &tc, "x86_64-unknown-test", 7, &[]);
        let added = fp3.unit_input_hash(&unit, None, &BTreeMap::new()).unwrap();
        assert_ne!(edited.0, added.0);

        // Build script output changes it.
        let with_script = fp3.unit_input_hash(&unit, Some(b"cargo:rustc-cfg=foo"), &BTreeMap::new()).unwrap();
        assert_ne!(added.0, with_script.0);

        // Features change it.
        let mut featured = unit_for("app");
        featured.features.insert("serde".into());
        let feat_hash = fp3.unit_input_hash(&featured, None, &BTreeMap::new()).unwrap();
        assert_ne!(added.0, feat_hash.0);

        // Profile changes it.
        let fp4 = Fingerprinter::new(&ws3, &tc, "x86_64-unknown-test", 8, &[]);
        let other_profile = fp4.unit_input_hash(&unit, None, &BTreeMap::new()).unwrap();
        assert_ne!(added.0, other_profile.0);

        // RUSTFLAGS change it.
        let flags = ["-Cdebug-assertions=off".to_owned()];
        let fp5 = Fingerprinter::new(&ws3, &tc, "x86_64-unknown-test", 7, &flags);
        let flagged = fp5.unit_input_hash(&unit, None, &BTreeMap::new()).unwrap();
        assert_ne!(added.0, flagged.0);
    }

    #[test]
    fn package_files_skip_target_and_hidden() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("target/debug/junk.o"), b"").unwrap();
        std::fs::write(dir.path().join(".git/config"), b"").unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), b"x").unwrap();
        std::fs::write(dir.path().join("README.md"), b"readme").unwrap();
        let files = package_files(dir.path()).unwrap();
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"src/lib.rs"));
        assert!(names.contains(&"README.md"));
        assert!(!names.iter().any(|n| n.starts_with("target")));
        assert!(!names.iter().any(|n| n.starts_with(".git")));
    }
}

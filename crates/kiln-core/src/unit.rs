//! Compile units: what a graph node is, and what mode it builds in.

use std::collections::BTreeSet;
use std::fmt;


use crate::ids::PackageId;

/// What a target is, from the manifest's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum TargetKind {
    /// A library (`--crate-type lib` or `rlib`).
    Lib,
    /// A library compiled as a proc-macro dylib.
    ProcMacro,
    /// A binary.
    Bin,
    /// A test target (`tests/*.rs`, `--test`).
    Test,
    /// A benchmark (`benches/*.rs`, `--bench`).
    Bench,
    /// An example (`examples/*.rs`).
    Example,
    /// A build script (`build.rs`), compiled as a host binary.
    BuildScript,
    /// The unit test harness of a lib or bin (`--test` on its own sources).
    TestHarness,
}

impl TargetKind {
    /// Whether units of this kind are linked as executables.
    pub fn is_executable(self) -> bool {
        matches!(self, TargetKind::Bin | TargetKind::Test | TargetKind::Bench | TargetKind::Example | TargetKind::BuildScript)
    }

    /// Whether this is a test-flavoured unit.
    pub fn is_test(self) -> bool {
        matches!(self, TargetKind::Test | TargetKind::Bench | TargetKind::TestHarness)
    }
}

/// What is being done to a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum CompileMode {
    /// Emit dep-info, metadata and the final artifact (rlib/dylib/bin).
    Build,
    /// Emit dep-info and metadata only: the `kiln check` fast path.
    Check,
    /// Build and then run the resulting test executable.
    Test,
}

impl fmt::Display for CompileMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CompileMode::Build => "build",
            CompileMode::Check => "check",
            CompileMode::Test => "test",
        })
    }
}

/// One target of one package, as discovered from the manifest tree.
///
/// `test` is `true` when the unit is compiled with `--cfg test` (the harness
/// of the same sources), which changes the crate's exported surface.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Unit {
    /// The package this unit belongs to.
    pub pkg: PackageId,
    /// The target's own name (crate name in the graph).
    pub target_name: String,
    /// Where the target's sources start.
    pub target_path: String,
    /// What kind of target it is.
    pub kind: TargetKind,
    /// Whether `--cfg test` is set for this unit.
    pub test: bool,
    /// The compile mode.
    pub mode: CompileMode,
    /// Features enabled for this unit's package, sorted.
    ///
    /// Part of identity: two feature sets are two units, never one unit that
    /// flips its flags between builds.
    pub features: BTreeSet<String>,
}

impl Unit {
    /// The crate name rustc will be told (`--crate-name`).
    ///
    /// Test harnesses and test targets compile the same sources under a
    /// different crate name so their artifacts never collide with the lib's.
    pub fn crate_name(&self) -> String {
        match (self.kind, self.test) {
            (TargetKind::Lib | TargetKind::ProcMacro, true) => format!("{}-test", self.target_name),
            (TargetKind::Bin, true) => format!("{}-test", self.target_name),
            _ => self.target_name.clone(),
        }
    }
}

impl fmt::Display for Unit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match self.mode {
            CompileMode::Build => "",
            CompileMode::Check => " (check)",
            CompileMode::Test => " (test)",
        };
        let kind = match self.kind {
            TargetKind::Lib => "lib",
            TargetKind::ProcMacro => "proc-macro",
            TargetKind::Bin => "bin",
            TargetKind::Test => "test",
            TargetKind::Bench => "bench",
            TargetKind::Example => "example",
            TargetKind::BuildScript => "build script",
            TargetKind::TestHarness => "test harness",
        };
        write!(f, "{} v{} [{kind}]{mode}", self.pkg.name, self.pkg.version)
    }
}

/// The identity a cache entry is stored under: a unit plus everything its
/// compilation consumes. The fingerprint module in `kiln-build` computes the
/// hash that completes this key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct UnitKey {
    /// The unit being compiled.
    pub unit: Unit,
    /// The profile's compiler-facing settings, folded in by value.
    pub profile_fingerprint: u64,
    /// The target triple compiled for.
    pub target: String,
    /// The exact rustc (version and commit) used.
    pub rustc: String,
    /// The host triple.
    pub host: String,
    /// Hashes of the artifacts of every dependency unit, in a stable order.
    pub dep_artifacts: Vec<(String, [u8; 32])>,
    /// Hash of the unit's own sources and build-script output.
    pub input_hash: [u8; 32],
}

impl UnitKey {
    /// The 16-hex-char short form used in rustc's `-Cmetadata` and filenames.
    pub fn short(&self) -> String {
        hex(&blake3_of_key(self))[..16].to_owned()
    }

    /// The full hex digest that names this entry in the store.
    pub fn digest(&self) -> String {
        hex(&blake3_of_key(self))
    }

    /// A stable ordering token for logs.
    pub fn label(&self) -> String {
        format!("{}-{}", self.unit.crate_name(), &self.short()[..8])
    }
}

/// Lowercase hex, without a dependency: 32 bytes in, 64 characters out.
pub fn hex(bytes: &[u8; 32]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push(TABLE[(b >> 4) as usize] as char);
        out.push(TABLE[(b & 0xf) as usize] as char);
    }
    out
}

fn blake3_of_key(key: &UnitKey) -> [u8; 32] {
    // serde_json of a UnitKey is canonical enough for hashing: every field is
    // either a string, a number or a fixed-order sequence, and the type is
    // closed over kiln's own versioning (a format change changes every key,
    // which is exactly what a cache format bump wants).
    let text = serde_json::to_string(key).expect("UnitKey serialises");
    *blake3::hash(text.as_bytes()).as_bytes()
}

/// Version of the key format; bumping this invalidates every cache entry.
pub const KEY_FORMAT: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::SourceId;

    fn unit(name: &str, kind: TargetKind) -> Unit {
        Unit {
            pkg: PackageId::registry(name, Version::new(0, 1, 0), SourceId::CRATES_IO),
            target_name: name.to_owned(),
            target_path: format!("src/{name}.rs"),
            kind,
            test: false,
            mode: CompileMode::Build,
            features: BTreeSet::new(),
        }
    }

    #[test]
    fn test_harnesses_get_distinct_crate_names() {
        let lib = unit("foo", TargetKind::Lib);
        let mut harness = lib.clone();
        harness.test = true;
        assert_eq!(lib.crate_name(), "foo");
        assert_eq!(harness.crate_name(), "foo-test");
    }

    #[test]
    fn unit_keys_hash_stably_and_differ_per_field() {
        let u = unit("foo", TargetKind::Lib);
        let base = UnitKey {
            unit: u.clone(),
            profile_fingerprint: 7,
            target: "x".into(),
            host: "y".into(),
            rustc: "1.0".into(),
            dep_artifacts: vec![],
            input_hash: [1; 32],
        };
        let same = UnitKey { unit: u.clone(), ..base.clone() };
        assert_eq!(base.digest(), same.digest());
        assert_eq!(base.short().len(), 16);

        let mut other = base.clone();
        other.profile_fingerprint = 8;
        assert_ne!(base.digest(), other.digest());

        other = base.clone();
        other.dep_artifacts = vec![("bar".into(), [9; 32])];
        assert_ne!(base.digest(), other.digest());

        other = base.clone();
        other.input_hash = [2; 32];
        assert_ne!(base.digest(), other.digest());
    }
}

//! From manifests to kiln-core targets: the `TargetSpec` list the build
//! engine schedules over.

use crate::manifest::Manifest;
use kiln_core::Result;
use kiln_core::unit::TargetKind;

/// A target, resolved for a particular package selection: what to build and
/// under what name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TargetSpec {
    /// The package this target belongs to (name; the engine joins with the
    /// resolved package table).
    pub package: String,
    /// The target name (crate name for compilation).
    pub name: String,
    /// Path of the entry file, relative to the package root.
    pub path: String,
    /// The kind.
    pub kind: TargetKind,
    /// Features that must be on for this target to exist at all.
    pub required_features: Vec<String>,
    /// Whether this target needs dev-dependencies (tests, examples, benches).
    pub needs_dev_deps: bool,
}

/// Discover every target of `manifest`, filtered by what `include_tests`
/// asks for. Examples and benches are discovered but only built on demand.
pub fn discover_targets(manifest: &Manifest, include_tests: bool) -> Result<Vec<TargetSpec>> {
    let mut out = Vec::new();
    let pkg = &manifest.name;

    if let Some(lib) = &manifest.lib {
        out.push(TargetSpec {
            package: pkg.clone(),
            name: lib.name.clone(),
            path: lib.path.clone(),
            kind: lib.kind,
            required_features: lib.required_features.clone(),
            needs_dev_deps: false,
        });
    }

    for bin in &manifest.bins {
        out.push(TargetSpec {
            package: pkg.clone(),
            name: bin.name.clone(),
            path: bin.path.clone(),
            kind: TargetKind::Bin,
            required_features: bin.required_features.clone(),
            needs_dev_deps: false,
        });
    }

    if include_tests {
        // The lib's own test harness.
        if let Some(lib) = &manifest.lib {
            out.push(TargetSpec {
                package: pkg.clone(),
                name: lib.name.clone(),
                path: lib.path.clone(),
                kind: TargetKind::TestHarness,
                required_features: lib.required_features.clone(),
                needs_dev_deps: true,
            });
        }
        // Each bin's test harness.
        for bin in &manifest.bins {
            out.push(TargetSpec {
                package: pkg.clone(),
                name: bin.name.clone(),
                path: bin.path.clone(),
                kind: TargetKind::TestHarness,
                required_features: bin.required_features.clone(),
                needs_dev_deps: true,
            });
        }
        for test in &manifest.tests {
            out.push(TargetSpec {
                package: pkg.clone(),
                name: test.name.clone(),
                path: test.path.clone(),
                kind: TargetKind::Test,
                required_features: test.required_features.clone(),
                needs_dev_deps: true,
            });
        }
        for bench in &manifest.benches {
            out.push(TargetSpec {
                package: pkg.clone(),
                name: bench.name.clone(),
                path: bench.path.clone(),
                kind: TargetKind::Bench,
                required_features: bench.required_features.clone(),
                needs_dev_deps: true,
            });
        }
    }

    for example in &manifest.examples {
        out.push(TargetSpec {
            package: pkg.clone(),
            name: example.name.clone(),
            path: example.path.clone(),
            kind: TargetKind::Example,
            required_features: example.required_features.clone(),
            needs_dev_deps: true,
        });
    }

    if let Some(script) = &manifest.build_script {
        out.push(TargetSpec {
            package: pkg.clone(),
            name: format!("build-script-{}", pkg.replace('-', "_")),
            path: script.clone(),
            kind: TargetKind::BuildScript,
            required_features: Vec::new(),
            needs_dev_deps: false,
        });
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_cover_lib_bins_scripts_and_tests() {
        let root = std::env::temp_dir().join("kiln-t1");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"t\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(root.join("build.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("tests/it.rs"), "").unwrap();

        let m = Manifest::parse(&root).unwrap();
        let all = discover_targets(&m, true).unwrap();
        let kinds: Vec<TargetKind> = all.iter().map(|t| t.kind).collect();
        assert!(kinds.contains(&TargetKind::Lib));
        assert!(kinds.contains(&TargetKind::Bin));
        assert!(kinds.contains(&TargetKind::BuildScript));
        assert_eq!(kinds.iter().filter(|k| **k == TargetKind::Test).count(), 1);
        assert!(kinds.contains(&TargetKind::TestHarness));
        assert!(all.iter().filter(|t| t.needs_dev_deps).count() >= 3);

        let no_tests = discover_targets(&m, false).unwrap();
        assert!(!no_tests.iter().any(|t| t.kind == TargetKind::TestHarness));
        assert!(!no_tests.iter().any(|t| t.needs_dev_deps));
        let _ = std::fs::remove_dir_all(&root);
    }
}

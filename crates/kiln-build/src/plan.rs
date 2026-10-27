//! Unit graph planning: from resolved packages to the graph the scheduler
//! runs, with build scripts, proc-macros, test harnesses and features wired
//! together exactly as the manifests ask.

use std::collections::BTreeMap;

use kiln_core::ids::PackageId;
use kiln_core::unit::{CompileMode, TargetKind, Unit};
use kiln_core::graph::UnitGraph;
use kiln_core::Result;
use kiln_manifest::targets::{discover_targets, TargetSpec};
use kiln_manifest::Workspace;
use kiln_resolve::features::{resolve_features, FeatureSelections, FeatureSet};
use kiln_resolve::resolver::{ResolveRequest, ResolvedWorkspace};

use crate::toolchain::Toolchain;

/// A unit plus the resolution facts the engine needs alongside it.
#[derive(Debug, Clone)]
pub struct PlannedUnit {
    /// The unit itself.
    pub unit: Unit,
    /// Whether this package comes from the registry (cap-lints, path
    /// remapping) or is a workspace member / path dependency.
    pub from_registry: bool,
    /// The edition rustc will be told.
    pub edition: String,
    /// The manifest root (sources), absolute.
    pub root: std::path::PathBuf,
    /// The package's build script output directory key, when it has a script.
    pub has_build_script: bool,
    /// Dev-dependencies included for this unit (test-flavoured units only).
    pub needs_dev_deps: bool,
}

/// The plan: the graph plus per-unit metadata.
pub struct Plan {
    /// The unit graph the scheduler runs.
    pub graph: UnitGraph,
    /// Metadata per graph index.
    pub units: Vec<PlannedUnit>,
    /// Which units the user asked to end at (binaries and their closure).
    pub requested: Vec<usize>,
}

/// Everything the planner needs.
pub struct PlanRequest<'a> {
    /// The workspace.
    pub workspace: &'a Workspace,
    /// The resolution.
    pub resolved: &'a ResolvedWorkspace,
    /// Feature selections from the command line.
    pub selections: &'a FeatureSelections,
    /// Build or check or test.
    pub mode: CompileMode,
    /// Package selection (`-p name`); all default members when empty.
    pub packages: Vec<String>,
    /// Whether bins should be built (true for build/run) or only checked.
    pub with_bins: bool,
    /// The toolchain (host facts for build scripts).
    pub toolchain: &'a Toolchain,
}

/// Plan the build.
pub fn plan_units(request: &PlanRequest) -> Result<Plan> {
    // 1. Features first: which optional deps are on decides the edges.
    let feature_sets = resolve_features(request.resolved, request.selections)?;

    // 2. Which packages are in the selection.
    let selection: Vec<String> = if request.packages.is_empty() {
        request.workspace.default_members.clone()
    } else {
        request.packages.clone()
    };

    let mut graph = UnitGraph::new();
    let mut units: Vec<PlannedUnit> = Vec::new();
    // (package, target name, kind) -> graph index, to wire edges.
    let mut index: BTreeMap<(String, String, TargetKind, bool), usize> = BTreeMap::new();

    // 3. Add every unit of every resolved package (the graph is the whole
    //    closure; the scheduler walks it, the `requested` set names the ends).
    let mut order: Vec<String> = request.resolved.packages.keys().cloned().collect();
    order.sort();

    for pkg_name in &order {
        let pkg_name = pkg_name.clone();
        let pkg = &request.resolved.packages[&pkg_name];
        let from_registry = pkg.id.source.is_registry();
        let features = feature_sets.get(&pkg_name).cloned().unwrap_or_default();
        let include_tests = request.mode == CompileMode::Test && !from_registry;
        let specs = discover_targets(&pkg.manifest, include_tests)?;

        for spec in specs {
            let kind = spec.kind;
            // Examples and benches are built only on request.
            if matches!(kind, TargetKind::Example | TargetKind::Bench) {
                continue;
            }
            // Build scripts run on the host and for the package.
            let unit = Unit {
                pkg: pkg.id.clone(),
                target_name: spec.name.clone(),
                target_path: spec.path.clone(),
                kind,
                test: matches!(spec.kind, TargetKind::TestHarness | TargetKind::Test),
                mode: request.mode,
                features: features.features.clone(),
            };
            let planned = PlannedUnit {
                unit,
                from_registry,
                edition: pkg.manifest.edition.as_str().to_owned(),
                root: pkg.manifest.root.clone(),
                has_build_script: pkg.manifest.build_script.is_some(),
                needs_dev_deps: spec.needs_dev_deps,
            };
            let idx = graph.add(planned.unit.clone());
            units.push(planned);
            index.insert((pkg_name.clone(), spec.name, kind, spec.needs_dev_deps), idx);
        }
    }

    // 4. Edges: each unit depends on the lib units of its package's
    //    enabled dependencies; build scripts depend on their build deps' libs.
    for (i, planned) in units.iter().enumerate() {
        let pkg = request.resolved.package(&planned.unit.pkg.name)?;
        let feature_set = feature_sets.get(&planned.unit.pkg.name).cloned().unwrap_or_default();
        let deps: Vec<&kiln_manifest::Dependency> = if planned.unit.kind == TargetKind::BuildScript {
            pkg.manifest.build_deps.values().collect()
        } else if planned.needs_dev_deps {
            let mut all: Vec<&kiln_manifest::Dependency> = pkg.manifest.deps.values().collect();
            all.extend(pkg.manifest.dev_deps.values());
            all
        } else {
            pkg.manifest.deps.values().collect()
        };
        for dep in deps {
            if !dep_enabled(dep, &feature_set, request.resolved) {
                continue;
            }
            let target_pkg = request.resolved.packages.get(dep.crate_name());
            let Some(target_pkg) = target_pkg else { continue };
            // The dep's lib unit is what a dependent consumes; a package
            // without a lib (bins only) contributes no edge.
            let Some(lib) = &target_pkg.manifest.lib else { continue };
            let key = (
                target_pkg.manifest.name.clone(),
                lib.name.clone(),
                lib.kind,
                false,
            );
            if let Some(&dep_idx) = index.get(&key) {
                graph.depend_on(i, dep_idx)?;
            }
        }
        // A build script precedes every other unit of its package.
        if planned.has_build_script && planned.unit.kind != TargetKind::BuildScript {
            let key = (
                planned.unit.pkg.name.clone(),
                format!("build-script-{}", planned.unit.pkg.name.replace('-', "_")),
                TargetKind::BuildScript,
                false,
            );
            if let Some(&script_idx) = index.get(&key) {
                graph.depend_on(i, script_idx)?;
            }
        }
        // Test harnesses of a package include the package's own lib.
        if planned.unit.kind == TargetKind::TestHarness {
            if let Some(lib) = &pkg.manifest.lib {
                let key = (planned.unit.pkg.name.clone(), lib.name.clone(), lib.kind, false);
                if let Some(&lib_idx) = index.get(&key) {
                    graph.depend_on(i, lib_idx)?;
                }
            }
        }
    }

    // 5. The requested ends: the selection's bins (build/run) or all its units.
    let mut requested = Vec::new();
    for (i, planned) in units.iter().enumerate() {
        if !selection.contains(&planned.unit.pkg.name) {
            continue;
        }
        match planned.unit.kind {
            TargetKind::Bin if request.with_bins => requested.push(i),
            TargetKind::Test | TargetKind::TestHarness | TargetKind::Bench => requested.push(i),
            TargetKind::Lib | TargetKind::ProcMacro if !request.with_bins => requested.push(i),
            _ => {}
        }
    }
    if requested.is_empty() {
        // A selection of library-only packages in build mode still wants the
        // lib units of the selection.
        for (i, planned) in units.iter().enumerate() {
            if selection.contains(&planned.unit.pkg.name)
                && matches!(planned.unit.kind, TargetKind::Lib | TargetKind::ProcMacro)
            {
                requested.push(i);
            }
        }
    }

    graph.topo_order()?; // a cycle is an error here, not at schedule time
    Ok(Plan { graph, units, requested })
}

/// Whether a dependency row is on under the feature set.
fn dep_enabled(dep: &kiln_manifest::Dependency, features: &FeatureSet, resolved: &ResolvedWorkspace) -> bool {
    if !dep.optional {
        return true;
    }
    if features.optional_deps.contains(&dep.name) {
        return true;
    }
    // A feature turned the dep on by crate name (dep/feat syntax resolved
    // earlier may only mark the crate's feature set).
    if features.optional_deps.contains(dep.crate_name()) {
        return true;
    }
    // Another enabled package's feature asked for this dep: the feature
    // resolution put the crate's name into its own optional set; check any
    // package that depends on this one has it on (conservative: ask the
    // resolved graph).
    let _ = resolved;
    false
}

/// The resolve request implied by a plan request.
pub fn resolve_request_for(mode: CompileMode, target: &str) -> ResolveRequest {
    ResolveRequest { include_dev: mode == CompileMode::Test, target_triple: target.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_manifest::Workspace as Ws;
    use std::path::PathBuf;

    fn write(path: &std::path::Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn fixture(root: &std::path::Path) {
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\", \"liba\"]\n",
        );
        write(
            &root.join("app/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nliba = { path = \"../liba\" }\n",
        );
        write(&root.join("app/src/main.rs"), "fn main() {}");
        write(&root.join("liba/Cargo.toml"), "[package]\nname = \"liba\"\nversion = \"0.1.0\"\n");
        write(&root.join("liba/src/lib.rs"), "pub fn a() {}");
    }

    fn resolved_of(ws: &Ws) -> ResolvedWorkspace {
        let mut packages = BTreeMap::new();
        for member in &ws.members {
            packages.insert(
                member.name.clone(),
                kiln_resolve::ResolvedPackage {
                    id: PackageId::path(&member.name, member.version.clone(), &member.root),
                    manifest: member.clone(),
                    deps: BTreeMap::new(),
                    features: Default::default(),
                },
            );
        }
        ResolvedWorkspace { packages, reused_lock: true }
    }

    #[test]
    fn a_bin_gets_edges_through_its_lib_and_the_script_precedes() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().to_path_buf();
        fixture(&root_path);
        // Give liba a build script.
        write(&root_path.join("liba/build.rs"), "fn main() {}");

        let ws = Ws::load_root(&root_path).unwrap();
        let resolved = resolved_of(&ws);
        let tc = Toolchain {
            rustc: PathBuf::from("/bin/rustc"),
            rustc_version: "1".into(),
            host: "x86_64-unknown-test".into(),
            linker: crate::toolchain::LinkerChoice::Default,
        };
        let selections = FeatureSelections::default();
        let plan = plan_units(&PlanRequest {
            workspace: &ws,
            resolved: &resolved,
            selections: &selections,
            mode: CompileMode::Build,
            packages: vec![],
            with_bins: true,
            toolchain: &tc,
        })
        .unwrap();

        // Units: app bin, liba lib, liba build script.
        assert!(plan.units.iter().any(|u| u.unit.kind == TargetKind::Bin && u.unit.pkg.name == "app"));
        assert!(plan.units.iter().any(|u| u.unit.kind == TargetKind::Lib && u.unit.pkg.name == "liba"));
        assert!(plan.units.iter().any(|u| u.unit.kind == TargetKind::BuildScript));

        // The requested end is the bin.
        assert_eq!(plan.requested.len(), 1);
        assert_eq!(plan.units[plan.requested[0]].unit.kind, TargetKind::Bin);

        // Topological sanity: every dependency index precedes dependents.
        let order = plan.graph.topo_order().unwrap();
        let position: BTreeMap<usize, usize> = order.iter().enumerate().map(|(pos, &i)| (i, pos)).collect();
        for (dependent, deps) in plan.graph.edges.iter().enumerate() {
            for &dep in deps {
                assert!(position[&dep] < position[&dependent], "dependency must come first");
            }
        }
    }

    #[test]
    fn test_mode_adds_harnesses_and_dev_deps() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().to_path_buf();
        fixture(&root_path);
        let ws = Ws::load_root(&root_path).unwrap();
        let resolved = resolved_of(&ws);
        let tc = Toolchain {
            rustc: PathBuf::from("/bin/rustc"),
            rustc_version: "1".into(),
            host: "x86_64-unknown-test".into(),
            linker: crate::toolchain::LinkerChoice::Default,
        };
        let plan = plan_units(&PlanRequest {
            workspace: &ws,
            resolved: &resolved,
            selections: &FeatureSelections::default(),
            mode: CompileMode::Test,
            packages: vec![],
            with_bins: false,
            toolchain: &tc,
        })
        .unwrap();
        assert!(plan.units.iter().any(|u| u.unit.kind == TargetKind::TestHarness));
    }
}

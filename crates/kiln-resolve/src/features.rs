//! Feature resolution: which features are on for which package, and which
//! optional dependencies they turn on.
//!
//! One fixed-point pass over the package graph: a feature on a package turns
//! on its listed features and `dep/feature` references on its dependencies;
//! features unify (union) across everything that asks, the way resolver v2
//! unifies them per package.

use std::collections::{BTreeMap, BTreeSet};

use kiln_core::Result;
use kiln_manifest::dependency::split_feature_ref;

use crate::resolver::ResolvedWorkspace;

/// The feature decisions for one package.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeatureSet {
    /// The enabled features of the package itself.
    pub features: BTreeSet<String>,
    /// The optional dependencies that are on.
    pub optional_deps: BTreeSet<String>,
}

impl FeatureSet {
    /// Whether `dep` (an import name) is built.
    pub fn dep_enabled(&self, dep: &str, optional: bool) -> bool {
        !optional || self.optional_deps.contains(dep)
    }
}

/// Per-package user selections: package name -> (features to turn on, use
/// default features).
#[derive(Debug, Clone, Default)]
pub struct FeatureSelections {
    /// package -> explicit features.
    pub features: BTreeMap<String, BTreeSet<String>>,
    /// packages whose default features are off.
    pub no_default: BTreeSet<String>,
    /// Turn default features off everywhere (`--no-default-features`).
    pub no_default_all: bool,
}

/// Resolve features across the whole workspace.
///
/// `deps_enabled` carries, per package, the import names other packages and
/// the user turned on (every non-optional dependency, plus optional ones a
/// feature asked for). The result is the fixed point of feature expansion.
pub fn resolve_features(
    ws: &ResolvedWorkspace,
    selections: &FeatureSelections,
) -> Result<BTreeMap<String, FeatureSet>> {
    // (package, feature) pairs still to expand.
    let mut queue: Vec<(String, String)> = Vec::new();
    let mut out: BTreeMap<String, FeatureSet> = BTreeMap::new();

    // Seed: defaults and explicit selections on every resolved package.
    for (name, pkg) in &ws.packages {
        let set = out.entry(name.clone()).or_default();
        let use_default = !selections.no_default_all && !selections.no_default.contains(name);
        if use_default && pkg.manifest.features.contains_key("default") {
            // `default` counts as enabled itself, the way cargo reports it.
            set.features.insert("default".to_owned());
            for f in &pkg.manifest.features["default"] {
                queue.push((name.clone(), f.clone()));
            }
        }
        if let Some(explicit) = selections.features.get(name) {
            for f in explicit {
                queue.push((name.clone(), f.clone()));
            }
        }
        // Every declared feature of a dependency row turns its target on.
        for dep in pkg.deps.values() {
            if dep.optional {
                continue;
            }
            for f in &dep.features {
                queue.push((dep.crate_name().to_owned(), f.clone()));
            }
        }
    }

    // Expand to the fixed point.
    while let Some((pkg_name, feature)) = queue.pop() {
        let Some(pkg) = ws.packages.get(&pkg_name) else { continue };
        let manifest = &pkg.manifest;
        let set = out.entry(pkg_name.clone()).or_default();

        // A feature reference can be: this package's feature, an implicit
        // optional-dependency enable, or `dep/feature`.
        if manifest.features.contains_key(&feature) {
            if set.features.insert(feature.clone()) {
                for reference in &manifest.features[&feature] {
                    let (dep, feat) = split_feature_ref(reference);
                    match dep {
                        Some(dep) if dep == "?" => {
                            // `?` syntax: only when the dep is already on.
                        }
                        Some(dep) => {
                            // dep/feat: turn the dep's feature on (and the
                            // dep itself if optional).
                            let target = dependency_by_crate_name(pkg, dep).map(str::to_owned);
                            if let Some(target) = target {
                                // The dep's own feature name goes to the
                                // package it resolves to (crate name).
                                if ws.packages.contains_key(&target) {
                                    let tset = out.entry(target.clone()).or_default();
                                    let _ = tset;
                                }
                                queue.push((target, feat.to_owned()));
                            }
                        }
                        None => queue.push((pkg_name.clone(), feat.to_owned())),
                    }
                }
            }
        } else if manifest.deps.values().any(|d| d.name == feature && d.optional) {
            // An implicit optional-dependency enable: `foo = ["bar"]` where
            // `bar` is an optional dependency.
            set.optional_deps.insert(feature);
        } else if manifest.deps.values().any(|d| d.crate_name() == feature) {
            // A feature naming a non-optional dependency: nothing to enable,
            // the dep is already in the build.
        }
        // An unknown feature is not silently ignored: features that name
        // nothing are a manifest error. (Except `default`, handled above.)
        else if feature != "default" {
            return Err(kiln_core::Error::parse(
                format!("features of `{pkg_name}`"),
                format!("`{feature}` is neither a feature nor an optional dependency of `{pkg_name}`"),
            ));
        }
    }

    // Non-optional dependencies are always on; fold them in from the
    // manifests, the source of truth for what a package declares.
    for (name, pkg) in &ws.packages {
        let set = out.entry(name.clone()).or_default();
        for dep in pkg.manifest.deps.values() {
            if !dep.optional {
                set.optional_deps.insert(dep.name.clone());
            }
        }
    }
    Ok(out)
}

fn dependency_by_crate_name<'a>(pkg: &'a crate::resolver::ResolvedPackage, crate_name: &str) -> Option<&'a str> {
    pkg.deps
        .values()
        .find(|d| d.crate_name() == crate_name)
        .map(|d| d.name.as_str())
}

fn dependency_import_name(pkg: &crate::resolver::ResolvedPackage, crate_name: &str) -> String {
    dependency_by_crate_name(pkg, crate_name)
        .unwrap_or(crate_name)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_core::ids::{PackageId, SourceId};
    use kiln_manifest::dependency::{DepKind, Dependency};
    use kiln_manifest::Manifest;

    fn pkg_with_features(name: &str, features: &[(&str, &[&str])], optional_deps: &[&str]) -> (String, crate::resolver::ResolvedPackage) {
        let root = std::env::temp_dir().join(format!("kiln-feat-{name}"));
        let features_table: String = features
            .iter()
            .map(|(f, list)| {
                let items = list.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ");
                format!("{f} = [{items}]")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let optional_rows: String = optional_deps
            .iter()
            .map(|d| format!("{d} = {{ version = \"1\", optional = true }}"))
            .collect::<Vec<_>>()
            .join("\n");
        let manifest = Manifest::from_str(
            &root,
            &format!(
                "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n[dependencies]\n{optional_rows}\n\n[features]\n{features_table}\n"
            ),
        )
        .unwrap();
        (
            name.to_owned(),
            crate::resolver::ResolvedPackage {
                id: PackageId::path(name, semver::Version::new(0, 1, 0), &root),
                manifest,
                deps: Default::default(),
                features: Default::default(),
            },
        )
    }

    fn resolved(pkgs: Vec<(String, crate::resolver::ResolvedPackage)>) -> ResolvedWorkspace {
        ResolvedWorkspace { packages: pkgs.into_iter().collect(), reused_lock: true }
    }

    #[test]
    fn defaults_expand_recursively() {
        // default -> [big], big -> [opt1]; opt1 is an optional dep.
        let ws = resolved(vec![pkg_with_features(
            "app",
            &[("default", &["big"]), ("big", &["opt1"])],
            &["opt1"],
        )]);
        let selections = FeatureSelections::default();
        let sets = resolve_features(&ws, &selections).unwrap();
        let app = &sets["app"];
        assert!(app.features.contains("default"));
        assert!(app.features.contains("big"));
        assert!(app.optional_deps.contains("opt1"));
    }

    #[test]
    fn explicit_selections_and_no_default() {
        let ws = resolved(vec![pkg_with_features(
            "app",
            &[("default", &["big"]), ("big", &[]), ("fast", &[])],
            &["opt1"],
        )]);
        let mut selections = FeatureSelections::default();
        selections.features.insert("app".into(), ["fast".to_owned()].into_iter().collect());
        let sets = resolve_features(&ws, &selections).unwrap();
        // Default is still on alongside the explicit selection.
        assert!(sets["app"].features.contains("big"));
        assert!(sets["app"].features.contains("fast"));

        selections.no_default.insert("app".into());
        let sets = resolve_features(&ws, &selections).unwrap();
        assert!(!sets["app"].features.contains("default"));
        assert!(!sets["app"].features.contains("big"));
        assert!(sets["app"].features.contains("fast"));
    }

    #[test]
    fn unknown_features_are_errors_not_silence() {
        let ws = resolved(vec![pkg_with_features("app", &[], &[])]);
        let mut selections = FeatureSelections::default();
        selections.features.insert("app".into(), ["ghost".to_owned()].into_iter().collect());
        let err = resolve_features(&ws, &selections).unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn dependency_features_propagate() {
        // a depends on b with features ["b-fast"]; b-fast turns on b's opt.
        let mut a = pkg_with_features("a", &[], &[]);
        let dep_b = Dependency::parse(
            "b",
            &toml::Value::Table({
                let mut t = toml::Table::new();
                t.insert("version".into(), "1".into());
                t.insert("features".into(), toml::Value::Array(vec![toml::Value::String("b-fast".into())]));
                t
            }),
            DepKind::Normal,
            std::path::Path::new("/tmp"),
        )
        .unwrap();
        a.1.deps.insert("b".into(), dep_b);
        let b = pkg_with_features("b", &[("b-fast", &["opt-b"])], &["opt-b"]);
        let ws = resolved(vec![a, b]);
        let sets = resolve_features(&ws, &FeatureSelections::default()).unwrap();
        assert!(sets["b"].features.contains("b-fast"), "{:?}", sets["b"]);
        assert!(sets["b"].optional_deps.contains("opt-b"));
    }
}

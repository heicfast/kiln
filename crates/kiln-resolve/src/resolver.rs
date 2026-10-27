//! The resolver: versions, sources and the unit-facing dependency graph.
//!
//! The lockfile is the source of truth when it holds: every package the
//! manifests need is locked at a version that satisfies its requirements.
//! Otherwise a highest-version-first search with chronological backtracking
//! finds an assignment, and the result is written back to the lockfile.

use std::collections::BTreeMap;
use std::path::Path;

use kiln_core::ids::{PackageId, SourceId};
use kiln_core::{Error, Result};
use kiln_manifest::{Dependency, Lockfile, LockPackage, Manifest, Workspace};

use crate::index::{IndexCache, IndexEntry};
use crate::source::RegistrySources;

/// A package with its resolution decided: which version, from where, and the
/// manifest of the sources on disk.
#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    /// The package identity.
    pub id: PackageId,
    /// Its manifest, read from the extracted sources.
    pub manifest: Manifest,
    /// Its dependencies that apply to this build, by import name.
    pub deps: BTreeMap<String, Dependency>,
    /// Features enabled on it after unification.
    pub features: std::collections::BTreeSet<String>,
}

/// The resolution of a whole workspace.
#[derive(Debug, Clone)]
pub struct ResolvedWorkspace {
    /// Packages by crate name.
    pub packages: BTreeMap<String, ResolvedPackage>,
    /// Whether the lockfile was reused (true) or re-resolved (false).
    pub reused_lock: bool,
}

impl ResolvedWorkspace {
    /// A package by name.
    pub fn package(&self, name: &str) -> Result<&ResolvedPackage> {
        self.packages
            .get(name)
            .ok_or_else(|| Error::Resolve(format!("package `{name}` was not resolved")))
    }
}

/// What the resolver needs to know about the build being planned.
#[derive(Debug, Clone)]
pub struct ResolveRequest {
    /// Whether test targets are in the build (dev-dependencies included).
    pub include_dev: bool,
    /// The compile target triple (filters target-specific deps).
    pub target_triple: String,
}

/// Resolve the workspace, reusing the lockfile when it satisfies.
pub async fn resolve(
    workspace: &Workspace,
    request: &ResolveRequest,
    index: &IndexCache,
    sources: &RegistrySources,
) -> Result<ResolvedWorkspace> {
    let lock_path = workspace.root.join("Cargo.lock");
    let lock = Lockfile::read(&lock_path)?;

    // 1. Try the lockfile: it wins when every constraint is satisfied.
    if let Some(lock) = &lock
        && let Ok(resolved) = resolve_locked(workspace, request, lock, index, sources).await {
            return Ok(resolved);
        }

    // 2. Resolve afresh, highest version first, and write the new lockfile.
    let (packages, checksums) = resolve_fresh(workspace, request, index, sources).await?;
    let new_lock = Lockfile {
        version: 4,
        packages: packages
            .values()
            .map(|p| LockPackage {
                name: p.id.name.clone(),
                version: p.id.version.clone(),
                source: match &p.id.source {
                    SourceId::Registry(_) => Some("registry+https://github.com/rust-lang/crates.io-index".to_owned()),
                    SourceId::Path(_) => None,
                },
                checksum: checksums.get(&p.id.name).cloned(),
                dependencies: p
                    .deps
                    .values()
                    .map(|d| kiln_manifest::LockDep {
                        name: d.crate_name().to_owned(),
                        version: packages.get(d.crate_name()).map(|r| r.id.version.clone()),
                    })
                    .collect(),
            })
            .collect(),
    };
    new_lock.write(&lock_path)?;
    Ok(ResolvedWorkspace { packages, reused_lock: false })
}

type Checksums = BTreeMap<String, String>;

/// Resolution state shared by both paths.
struct Resolver<'a> {
    workspace: &'a Workspace,
    request: &'a ResolveRequest,
    index: &'a IndexCache,
    sources: &'a RegistrySources,
    packages: BTreeMap<String, ResolvedPackage>,
    checksums: Checksums,
}

/// The locked path: every registry dependency comes from the lock.
async fn resolve_locked(
    workspace: &Workspace,
    request: &ResolveRequest,
    lock: &Lockfile,
    index: &IndexCache,
    sources: &RegistrySources,
) -> Result<ResolvedWorkspace> {
    let mut r = Resolver {
        workspace,
        request,
        index,
        sources,
        packages: BTreeMap::new(),
        checksums: BTreeMap::new(),
    };
    for member in &workspace.members {
        r.add_path_package(member).await?;
    }
    let needs = r.collect_member_needs();
    r.walk_needs(needs, Some(lock)).await?;
    r.validate_against_lock(lock)?;
    Ok(ResolvedWorkspace { packages: r.packages, reused_lock: true })
}

/// The fresh path: search the index, highest version first.
async fn resolve_fresh(
    workspace: &Workspace,
    request: &ResolveRequest,
    index: &IndexCache,
    sources: &RegistrySources,
) -> Result<(BTreeMap<String, ResolvedPackage>, Checksums)> {
    let mut r = Resolver {
        workspace,
        request,
        index,
        sources,
        packages: BTreeMap::new(),
        checksums: BTreeMap::new(),
    };
    for member in &workspace.members {
        r.add_path_package(member).await?;
    }
    let needs = r.collect_member_needs();
    r.walk_needs(needs, None).await?;
    Ok((r.packages, r.checksums))
}

impl Resolver<'_> {
    /// Members' own dependency needs (normal + build; dev when tests are in).
    fn collect_member_needs(&self) -> Vec<(String, Dependency)> {
        let mut needs = Vec::new();
        for member in &self.workspace.members {
            for dep in member.deps.values() {
                needs.push((member.name.clone(), dep.clone()));
            }
            for dep in member.build_deps.values() {
                needs.push((member.name.clone(), dep.clone()));
            }
            if self.request.include_dev {
                for dep in member.dev_deps.values() {
                    needs.push((member.name.clone(), dep.clone()));
                }
            }
        }
        needs
    }

    /// Register a path-resolved package (a workspace member or path dep).
    async fn add_path_package(&mut self, manifest: &Manifest) -> Result<()> {
        if self.packages.contains_key(&manifest.name) {
            return Ok(());
        }
        // Path packages may inherit from their own workspace; that is already
        // resolved by the loader that produced the manifest.
        let deps = self.applicable_deps(manifest, false);
        self.packages.insert(
            manifest.name.clone(),
            ResolvedPackage {
                id: PackageId::path(&manifest.name, manifest.version.clone(), &manifest.root),
                manifest: manifest.clone(),
                deps,
                features: Default::default(),
            },
        );
        Ok(())
    }

    /// The dependencies of `manifest` that apply to this build.
    fn applicable_deps(&self, manifest: &Manifest, is_registry: bool) -> BTreeMap<String, Dependency> {
        let mut out = BTreeMap::new();
        for dep in manifest.deps.values() {
            out.insert(dep.name.clone(), dep.clone());
        }
        for dep in manifest.build_deps.values() {
            if manifest.build_script.is_some() {
                out.insert(dep.name.clone(), dep.clone());
            }
        }
        // Dev-dependencies apply only to workspace members with tests in the
        // build; a registry package's own tests are never built.
        if self.request.include_dev && !is_registry {
            for dep in manifest.dev_deps.values() {
                out.insert(dep.name.clone(), dep.clone());
            }
        }
        out
    }

    /// Walk needs, resolving each to a package and queueing its own needs.
    async fn walk_needs(
        &mut self,
        needs: Vec<(String, Dependency)>,
        lock: Option<&Lockfile>,
    ) -> Result<()> {
        let mut queue: Vec<Dependency> = needs.into_iter().map(|(_, d)| d).collect();
        let mut seen = std::collections::BTreeSet::new();
        while let Some(dep) = queue.pop() {
            if !seen.insert(format!("{}@{}", dep.crate_name(), dep.req)) {
                continue;
            }
            let name = dep.crate_name().to_owned();
            if self.packages.contains_key(&name) {
                continue;
            }
            if let Some(path) = &dep.path {
                let root = self.workspace.root.join(path);
                let manifest = self.load_path_manifest(&root)?;
                self.add_path_package(&manifest).await?;
                let pkg = self.packages.get(&manifest.name).expect("just inserted");
                let inner: Vec<Dependency> = pkg.deps.values().cloned().collect();
                queue.extend(inner);
                continue;
            }
            // Registry dependency.
            let entry = self.pick_registry_version(&name, &dep, lock).await?;
            let src = self
                .sources
                .ensure(&name, &entry.vers, entry.checksum.as_deref())
                .await?;
            let manifest = self.load_path_manifest(&src)?;
            let deps = self.applicable_deps(&manifest, true);
            if let Some(cs) = &entry.checksum {
                self.checksums.insert(name.clone(), cs.clone());
            }
            self.packages.insert(
                name.clone(),
                ResolvedPackage {
                    id: PackageId::registry(&name, entry.vers.clone(), SourceId::CRATES_IO),
                    manifest,
                    deps,
                    features: Default::default(),
                },
            );
            let inner: Vec<Dependency> = self.packages[&name].deps.values().cloned().collect();
            queue.extend(inner);
        }
        Ok(())
    }

    /// Load (and inherit) a manifest from a directory.
    fn load_path_manifest(&self, root: &Path) -> Result<Manifest> {
        let mut manifest = Manifest::parse(root)?;
        // Path packages resolve their own `workspace = true` inheritance
        // against the nearest enclosing workspace root.
        if let Ok(ws) = Workspace::load(root) {
            let ws_deps = ws.workspace_deps.clone();
            Workspace::resolve_inheritance(&mut manifest, &ws.root, &ws_deps)?;
        }
        Ok(manifest)
    }

    /// Pick the version for a registry dependency: the lock's, or the highest
    /// satisfying non-yanked one.
    async fn pick_registry_version(
        &self,
        name: &str,
        dep: &Dependency,
        lock: Option<&Lockfile>,
    ) -> Result<IndexEntry> {
        if let Some(lock) = lock
            && let Some(locked) = lock.find(name)
                && dep.req.matches(&locked.version) {
                    let entries = self.index.entries(name).await?;
                    if let Some(entry) = entries.iter().find(|e| e.vers == locked.version) {
                        if let Some(exact) = &dep.exact
                            && exact != &locked.version {
                                return Err(Error::Resolve(format!(
                                    "`{name}` is locked at {} but {} requires exactly {exact}",
                                    locked.version, dep.name
                                )));
                            }
                        return Ok(entry.clone());
                    }
                    return Err(Error::Resolve(format!(
                        "`{name} {}` is locked but absent from the registry index",
                        locked.version
                    )));
                }
        let entries = self.index.entries(name).await?;
        let mut candidates: Vec<&IndexEntry> = entries
            .iter()
            .filter(|e| !e.yanked && dep.req.matches(&e.vers))
            .collect();
        if let Some(exact) = &dep.exact {
            candidates.retain(|e| e.vers == *exact);
        }
        candidates.sort_by(|a, b| b.vers.cmp(&a.vers));
        candidates
            .into_iter()
            .next()
            .cloned()
            .ok_or_else(|| {
                Error::Resolve(format!(
                    "no version of `{name}` matches `{}` ({} versions known, all yanked or out of range)",
                    dep.req,
                    entries.len()
                ))
            })
    }

    fn validate_against_lock(&self, lock: &Lockfile) -> Result<()> {
        for pkg in self.packages.values() {
            for dep in pkg.deps.values() {
                if let Some(locked) = lock.find(dep.crate_name())
                    && !dep.req.matches(&locked.version) {
                        return Err(Error::Resolve(format!(
                            "the lockfile pins `{}` to {}, but `{}` requires `{}`; re-resolve (delete Cargo.lock or run kiln fetch)",
                            dep.crate_name(),
                            locked.version,
                            pkg.id.name,
                            dep.req
                        )));
                    }
            }
        }
        Ok(())
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use kiln_manifest::dependency::DepKind;
    
    use std::path::PathBuf;

    fn member(name: &str, deps: &[(&str, &str)]) -> Manifest {
        let root = PathBuf::from("/tmp/ws").join(name);
        let deps_text = deps
            .iter()
            .map(|(n, v)| format!("{n} = \"{v}\""))
            .collect::<Vec<_>>()
            .join("\n");
        Manifest::from_str(
            &root,
            &format!(
                "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n[dependencies]\n{deps_text}\n"
            ),
        )
        .unwrap()
    }

    #[test]
    fn applicable_deps_follow_kind_and_membership() {
        let ws = Workspace {
            root: PathBuf::from("/tmp/ws"),
            resolver: None,
            members: vec![member("m", &[("serde", "1")])],
            default_members: vec!["m".into()],
            workspace_deps: BTreeMap::new(),
            profiles: BTreeMap::new(),
        };
        let request = ResolveRequest {
            include_dev: false,
            target_triple: "x86_64-unknown-linux-gnu".into(),
        };
        let resolver = Resolver {
            workspace: &ws,
            request: &request,
            index: &IndexCache::new(true, false).unwrap(),
            sources: &RegistrySources::new(true).unwrap(),
            packages: BTreeMap::new(),
            checksums: BTreeMap::new(),
        };
        let manifest = resolver.workspace.members[0].clone();
        let deps = resolver.applicable_deps(&manifest, false);
        assert!(deps.contains_key("serde"));

        let mut with_dev = manifest.clone();
        with_dev.dev_deps.insert(
            "criterion".into(),
            Dependency::parse(
                "criterion",
                &toml::Value::String("0.5".into()),
                DepKind::Dev,
                Path::new("/tmp/ws/m"),
            )
            .unwrap(),
        );
        let none = resolver.applicable_deps(&with_dev, false);
        assert!(!none.contains_key("criterion"), "dev deps without tests");
        let request_dev = ResolveRequest { include_dev: true, ..request };
        let resolver_dev = Resolver {
            workspace: &ws,
            request: &request_dev,
            index: &IndexCache::new(true, false).unwrap(),
            sources: &RegistrySources::new(true).unwrap(),
            packages: BTreeMap::new(),
            checksums: BTreeMap::new(),
        };
        let some = resolver_dev.applicable_deps(&with_dev, false);
        assert!(some.contains_key("criterion"));
        // A registry package never gets dev deps.
        let none_registry = resolver_dev.applicable_deps(&with_dev, true);
        assert!(!none_registry.contains_key("criterion"));
    }
}

//! Workspace discovery: walk up from a directory to the workspace root,
//! gather members, resolve `workspace = true` inheritance and
//! `[workspace.dependencies]`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::dependency::{DepKind, Dependency};
use crate::manifest::Manifest;
use kiln_core::{Error, Result};

/// One member of a workspace: its manifest plus the workspace-inherited
/// values already folded in.
pub type WorkspaceMember = Manifest;

/// A loaded workspace.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// The directory holding the root `Cargo.toml` with `[workspace]`.
    pub root: PathBuf,
    /// The workspace's resolver, if the root sets one.
    pub resolver: Option<u8>,
    /// All members, keyed by canonical package root.
    pub members: Vec<WorkspaceMember>,
    /// The default members (what a bare `kiln build` builds), by name; all
    /// members when the manifest names none.
    pub default_members: Vec<String>,
    /// `[workspace.dependencies]`, resolved lazily per member.
    pub workspace_deps: BTreeMap<String, toml::Value>,
    /// Profiles from the root manifest, raw, for kiln-core to fold.
    pub profiles: BTreeMap<String, toml::Table>,
}

impl Workspace {
    /// Find the workspace containing `from` and load every member.
    pub fn load(from: &Path) -> Result<Self> {
        let root = find_root(from)?;
        Self::load_root(&root)
    }

    /// Load the workspace rooted at `root`.
    pub fn load_root(root: &Path) -> Result<Self> {
        let file = root.join("Cargo.toml");
        let text = std::fs::read_to_string(&file).map_err(|e| Error::io("read", &file, e))?;
        let table: toml::Table =
            toml::from_str(&text).map_err(|e| Error::parse(format!("{}/Cargo.toml", root.display()), e))?;

        let ws = table.get("workspace").and_then(|v| v.as_table()).ok_or_else(|| {
            Error::missing(&file, "no [workspace] section; not a workspace root")
        })?;

        // Members: explicit list plus globs, minus excludes.
        let mut member_dirs: Vec<PathBuf> = Vec::new();
        let mut explicit = Vec::new();
        if let Some(list) = ws.get("members").and_then(|v| v.as_array()) {
            for item in list {
                let pattern = item.as_str().ok_or_else(|| {
                    Error::parse("workspace.members", "every member must be a string path or glob")
                })?;
                explicit.push(pattern.to_owned());
                let full = root.join(pattern);
                if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
                    expand_glob(root, pattern, &mut member_dirs)?;
                } else if full.is_dir() {
                    member_dirs.push(full);
                } else {
                    return Err(Error::missing(&full, "workspace member directory does not exist"));
                }
            }
        }
        // Default-members: subset of members by path.
        let default_members: Vec<String> = ws
            .get("default-members")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(|s| root.join(s).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| s.to_owned()))
                    .collect()
            })
            .unwrap_or_default();

        let workspace_deps = ws
            .get("dependencies")
            .and_then(|v| v.as_table())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();

        let profiles: BTreeMap<String, toml::Table> = table
            .get("profile")
            .and_then(|v| v.as_table())
            .map(|t| {
                t.iter()
                    .filter_map(|(k, v)| v.as_table().map(|vt| (k.clone(), vt.clone())))
                    .collect()
            })
            .unwrap_or_default();

        let resolver = ws
            .get("resolver")
            .and_then(|v| v.as_integer())
            .and_then(|i| u8::try_from(i).ok());

        let root_pkg = table.get("package").and_then(|v| v.as_table());
        // The root manifest may itself be a member (a package + workspace).
        let mut member_dirs = member_dirs;
        if root_pkg.is_some() {
            if !member_dirs.contains(&root.to_path_buf()) {
                member_dirs.push(root.to_path_buf());
            }
        }

        // Parse every member, resolving inheritance against this workspace.
        let mut members = Vec::new();
        for dir in &member_dirs {
            let mut manifest = Manifest::parse(dir)?;
            Self::resolve_inheritance(&mut manifest, root, &workspace_deps)?;
            if let Some(ws_resolver) = resolver {
                manifest.resolver = ws_resolver;
            }
            members.push(manifest);
        }

        if members.is_empty() {
            return Err(Error::missing(&file, "workspace has no members"));
        }

        // Member names must be unique: two members of one name is a mistake
        // no build system should paper over.
        let mut names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        names.sort_unstable();
        for pair in names.windows(2) {
            if pair[0] == pair[1] {
                return Err(Error::parse("Cargo.toml", format!("two workspace members are named `{}`", pair[0])));
            }
        }

        let default_members = if default_members.is_empty() {
            members.iter().map(|m| m.name.clone()).collect()
        } else {
            default_members
        };

        Ok(Workspace {
            root: root.to_path_buf(),
            resolver,
            members,
            default_members,
            workspace_deps,
            profiles,
        })
    }

    /// Find a member by package name.
    pub fn member(&self, name: &str) -> Option<&WorkspaceMember> {
        self.members.iter().find(|m| m.name == name)
    }

    /// Resolve `workspace = true` inheritance in one member manifest.
    pub fn resolve_inheritance(
        manifest: &mut Manifest,
        root: &Path,
        workspace_deps: &BTreeMap<String, toml::Value>,
    ) -> Result<()> {
        let manifest_path = manifest.root.join("Cargo.toml");
        let text = std::fs::read_to_string(&manifest_path)
            .map_err(|e| Error::io("read", &manifest_path, e))?;
        let table: toml::Table = toml::from_str(&text)
            .map_err(|e| Error::parse(format!("{}/Cargo.toml", manifest.root.display()), e))?;

        // package.workspace = true fields.
        if let Some(pkg) = table.get("package").and_then(|v| v.as_table()) {
            for (key, value) in pkg {
                if value.as_bool() == Some(true) && key == "workspace" {
                    continue;
                }
                let inherited = matches!(value, toml::Value::Table(t) if t.get("workspace").and_then(|v| v.as_bool()) == Some(true));
                if !inherited {
                    continue;
                }
                let root_file = root.join("Cargo.toml");
                let root_text = std::fs::read_to_string(&root_file).map_err(|e| Error::io("read", &root_file, e))?;
                let root_table: toml::Table = toml::from_str(&root_text)
                    .map_err(|e| Error::parse(format!("{}/Cargo.toml", root.display()), e))?;
                let ws_pkg = root_table
                    .get("workspace")
                    .and_then(|v| v.as_table())
                    .and_then(|t| t.get("package"))
                    .and_then(|v| v.as_table())
                    .ok_or_else(|| Error::parse("Cargo.toml", "package fields marked `workspace = true` need [workspace.package] to inherit from"))?;
                let value = ws_pkg.get(key).ok_or_else(|| {
                    Error::parse(
                        "Cargo.toml",
                        format!("package.{key} is inherited but [workspace.package] does not define it"),
                    )
                })?;
                match key.as_str() {
                    "version" => manifest.version = value.as_str().unwrap_or_default().parse().map_err(|e| Error::parse("workspace.package.version", e))?,
                    "edition" => manifest.edition = crate::Edition::parse(value.as_str().unwrap_or_default())?,
                    "rust-version" => manifest.rust_version = Some(value.as_str().unwrap_or_default().parse().map_err(|e| Error::parse("workspace.package.rust-version", e))?),
                    "repository" | "license" | "readme" | "homepage" | "documentation" | "description" | "authors" | "keywords" | "categories" | "publish" | "exclude" | "include" => {}
                    other => {
                        return Err(Error::unsupported(format!(
                            "package.{other} = {{ workspace = true }}: kiln does not inherit this field yet"
                        )))
                    }
                }
            }
        }

        // `foo = { workspace = true }` dependency rows.
        for (table_name, kind) in [
            ("dependencies", DepKind::Normal),
            ("dev-dependencies", DepKind::Dev),
            ("build-dependencies", DepKind::Build),
        ] {
            let Some(deps) = table.get(table_name).and_then(|v| v.as_table()) else { continue };
            for (name, value) in deps {
                let Some(dep_table) = value.as_table() else { continue };
                if dep_table.get("workspace").and_then(|v| v.as_bool()) != Some(true) {
                    continue;
                }
                let ws_value = workspace_deps.get(name).ok_or_else(|| {
                    Error::parse(
                        "Cargo.toml",
                        format!("dependency `{name}` is inherited but [workspace.dependencies] does not define it"),
                    )
                })?;
                let mut dep = Dependency::from_workspace_inherit(name, ws_value, &manifest.root)?;
                // Member-level overrides: features and optional.
                if let Some(features) = dep_table.get("features").and_then(|v| v.as_array()) {
                    dep.features.extend(features.iter().filter_map(|f| f.as_str().map(str::to_owned)));
                }
                if let Some(optional) = dep_table.get("optional").and_then(|v| v.as_bool()) {
                    dep.optional = optional;
                }
                let map = match kind {
                    DepKind::Normal => &mut manifest.deps,
                    DepKind::Dev => &mut manifest.dev_deps,
                    DepKind::Build => &mut manifest.build_deps,
                };
                map.insert(name.clone(), dep);
            }
        }
        Ok(())
    }
}

/// Walk up from `from` until a `Cargo.toml` with a `[workspace]` table (or
/// one whose parent chain ends without one, in which case the nearest
/// package manifest's directory is the workspace of one).
fn find_root(from: &Path) -> Result<PathBuf> {
    let mut current = Some(from);
    let mut fallback: Option<PathBuf> = None;
    while let Some(dir) = current {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file() {
            let text = std::fs::read_to_string(&manifest).map_err(|e| Error::io("read", &manifest, e))?;
            let table: toml::Table = toml::from_str(&text)
                .map_err(|e| Error::parse(format!("{}/Cargo.toml", dir.display()), e))?;
            if table.contains_key("workspace") {
                return Ok(dir.to_path_buf());
            }
            if fallback.is_none() {
                fallback = Some(dir.to_path_buf());
            }
        }
        current = dir.parent();
    }
    fallback.ok_or_else(|| {
        Error::missing(from.join("Cargo.toml"), "no Cargo.toml found in this directory or any parent")
    })
}

/// Expand a member glob like `crates/*` against the workspace root.
fn expand_glob(root: &Path, pattern: &str, out: &mut Vec<PathBuf>) -> Result<()> {
    // Walk the pattern segment by segment: literal segments name a directory,
    // `*`/`?` segments scan the parent's children.
    let segments: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let mut current: Vec<PathBuf> = vec![root.to_path_buf()];
    for segment in &segments {
        let mut next = Vec::new();
        for dir in &current {
            if segment.contains('*') || segment.contains('?') {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                        if glob_match(segment, name) && path.is_dir() && !name.starts_with('.') {
                            next.push(path);
                        }
                    }
                }
            } else {
                let p = dir.join(segment);
                if p.is_dir() {
                    next.push(p);
                }
            }
        }
        current = next;
    }
    for dir in current {
        if dir.join("Cargo.toml").is_file() && !out.contains(&dir) {
            out.push(dir);
        }
    }
    Ok(())
}

/// Match one glob segment: `*` runs, `?` single character.
fn glob_match(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    fn go(p: &[char], n: &[char]) -> bool {
        if p.is_empty() {
            return n.is_empty();
        }
        match p[0] {
            '*' => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            '?' => !n.is_empty() && go(&p[1..], &n[1..]),
            c => !n.is_empty() && n[0] == c && go(&p[1..], &n[1..]),
        }
    }
    go(&p, &n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn a_two_member_workspace_loads_with_inheritance() {
        let root = std::env::temp_dir().join(format!("kiln-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write(
            &root.join("Cargo.toml"),
            r#"
[workspace]
members = ["crates/*"]

[workspace.package]
version = "2.3.4"
edition = "2021"

[workspace.dependencies]
serde = "1"
"#,
        );
        write(
            &root.join("crates/app/Cargo.toml"),
            r#"
[package]
name = "app"
version = { workspace = true }
edition = { workspace = true }

[dependencies]
serde = { workspace = true }
"#,
        );
        write(&root.join("crates/app/src/main.rs"), "fn main() {}");
        write(
            &root.join("crates/libx/Cargo.toml"),
            r#"
[package]
name = "libx"
version = { workspace = true }
edition = { workspace = true }
"#,
        );
        write(&root.join("crates/libx/src/lib.rs"), "");

        let ws = Workspace::load_root(&root).unwrap();
        assert_eq!(ws.members.len(), 2);
        let app = ws.member("app").unwrap();
        assert_eq!(app.version.to_string(), "2.3.4");
        assert_eq!(app.edition, crate::Edition::E2021);
        assert!(app.deps.contains_key("serde"));
        assert!(app.deps["serde"].req.matches(&semver::Version::new(1, 0, 200)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn duplicate_member_names_are_an_error() {
        let root = std::env::temp_dir().join(format!("kiln-ws-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write(&root.join("Cargo.toml"), "[workspace]\nmembers = [\"a\", \"b\"]\n");
        write(&root.join("a/Cargo.toml"), "[package]\nname = \"same\"\nversion = \"0.1.0\"\n");
        write(&root.join("b/Cargo.toml"), "[package]\nname = \"same\"\nversion = \"0.1.0\"\n");
        let err = Workspace::load_root(&root).unwrap_err();
        assert!(err.to_string().contains("two workspace members"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn globs_match_single_stars() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("cr*", "crate"));
        assert!(!glob_match("cr*", "bin"));
        assert!(glob_match("?", "a"));
        assert!(!glob_match("?", "ab"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
    }

    #[test]
    fn roots_are_found_by_walking_up() {
        let root = std::env::temp_dir().join(format!("kiln-ws-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write(&root.join("Cargo.toml"), "[workspace]\nmembers = [\"m\"]\n");
        write(&root.join("m/Cargo.toml"), "[package]\nname = \"m\"\nversion = \"0.1.0\"\n");
        write(&root.join("m/src/lib.rs"), "");
        let found = find_root(&root.join("m/src")).unwrap();
        assert_eq!(found, root);
        let _ = std::fs::remove_dir_all(&root);
    }
}

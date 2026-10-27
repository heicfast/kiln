//! Dependency declarations: one row of `[dependencies]`, `[dev-dependencies]`
//! or `[build-dependencies]`, in every form cargo spells them.

use std::collections::BTreeMap;

use semver::VersionReq;

/// Which table a dependency came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DepKind {
    /// `[dependencies]`: compiled into the artifact.
    Normal,
    /// `[dev-dependencies]`: only for tests, examples and benches of
    /// workspace members; never for a dependency's own tests.
    Dev,
    /// `[build-dependencies]`: for the package's build script.
    Build,
}

/// One dependency, after resolving the `workspace = true` indirection.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Dependency {
    /// The name this package is referred to by (after `rename`, if any).
    pub name: String,
    /// The real crate, when `package = "..."` renames it.
    pub package: Option<String>,
    /// The version requirement, `*` when none is given (path deps).
    pub req: VersionReq,
    /// The exact version required (`=1.2.3`), if any.
    pub exact: Option<semver::Version>,
    /// Whether the dependency can be left out by features.
    pub optional: bool,
    /// The default features are on unless this is `false`.
    pub default_features: bool,
    /// Features the declaration turns on beyond the default set.
    pub features: Vec<String>,
    /// A path dependency, relative to the owning manifest's directory.
    pub path: Option<String>,
    /// The registry it must come from, when `registry = "..."` names one.
    pub registry: Option<String>,
}

impl Dependency {
    /// The crate name to resolve against: `package` when set, else `name`.
    pub fn crate_name(&self) -> &str {
        self.package.as_deref().unwrap_or(&self.name)
    }

    /// Parse one dependency entry. `table_value` is the right-hand side:
    /// either a bare version string or a table.
    pub fn parse(
        dep_name: &str,
        table_value: &toml::Value,
        _kind: DepKind,
        _manifest_dir: &std::path::Path,
    ) -> kiln_core::Result<Self> {
        let mut dep = Dependency {
            name: dep_name.to_owned(),
            package: None,
            req: VersionReq::STAR,
            exact: None,
            optional: false,
            default_features: true,
            features: Vec::new(),
            path: None,
            registry: None,
        };
        match table_value {
            toml::Value::String(req) => {
                dep.req = parse_req(dep_name, req)?;
                if let Some(exact) = req.strip_prefix('=') {
                    dep.exact = Some(exact.trim().parse().map_err(|e| {
                        kiln_core::Error::parse(format!("dependency {dep_name}"), e)
                    })?);
                }
            }
            toml::Value::Table(t) => {
                for (key, value) in t {
                    match key.as_str() {
                        "version" => {
                            let req = string_of(dep_name, "version", value)?;
                            dep.req = parse_req(dep_name, &req)?;
                            if let Some(exact) = req.strip_prefix('=') {
                                dep.exact = Some(exact.trim().parse().map_err(|e| {
                                    kiln_core::Error::parse(format!("dependency {dep_name}"), e)
                                })?);
                            }
                        }
                        "path" => dep.path = Some(string_of(dep_name, "path", value)?),
                        "package" => dep.package = Some(string_of(dep_name, "package", value)?),
                        "optional" => dep.optional = bool_of(dep_name, "optional", value)?,
                        "default-features" => {
                            dep.default_features = bool_of(dep_name, "default-features", value)?
                        }
                        "features" => {
                            dep.features = string_list_of(dep_name, "features", value)?;
                        }
                        "registry" => dep.registry = Some(string_of(dep_name, "registry", value)?),
                        // `default_features` is the old spelling; cargo still
                        // accepts both, so kiln does too.
                        "default_features" => {
                            dep.default_features = bool_of(dep_name, "default_features", value)?
                        }
                        // git and branch/tag/rev dependencies: refused by
                        // name, never silently treated as a missing dep.
                        "git" => {
                            return Err(kiln_core::Error::unsupported(format!(
                                "dependency `{dep_name}` is a git dependency; kiln does not build git dependencies yet (path and registry dependencies are supported)"
                            )));
                        }
                        "branch" | "tag" | "rev" | "sparse-registry" => {
                            return Err(kiln_core::Error::unsupported(format!(
                                "dependency `{dep_name}` uses `{key}`, which belongs to a git dependency; kiln does not build git dependencies yet"
                            )));
                        }
                        // Public-facing metadata that never changes a build.
                        "public" | "public-registry" => {}
                        other => {
                            return Err(kiln_core::Error::unsupported(format!(
                                "dependency `{dep_name}`: unknown key `{other}`"
                            )));
                        }
                    }
                }
                if dep.path.is_none() && t.get("version").is_none() {
                    return Err(kiln_core::Error::parse(
                        format!("dependency {dep_name}"),
                        "needs a version or a path (rows with only `workspace = true` are resolved against [workspace.dependencies] before reaching this parser)",
                    ));
                }
            }
            other => {
                return Err(kiln_core::Error::parse(
                    format!("dependency {dep_name}"),
                    format!("expected a version string or a table, got {other:?}"),
                ));
            }
        }
        // Path dependencies are kept exactly as the manifest spells them:
        // the loader resolves them against the owning manifest's directory,
        // which keeps the spelling in the cache key stable across platforms.
        Ok(dep)
    }

    /// Fill a `workspace = true` dependency from the workspace table.
    pub fn from_workspace_inherit(
        dep_name: &str,
        ws_value: &toml::Value,
        manifest_dir: &std::path::Path,
    ) -> kiln_core::Result<Self> {
        Dependency::parse(dep_name, ws_value, DepKind::Normal, manifest_dir)
    }
}

fn parse_req(name: &str, req: &str) -> kiln_core::Result<VersionReq> {
    VersionReq::parse(req)
        .map_err(|e| kiln_core::Error::parse(format!("dependency {name}: `{req}`"), e))
}

fn string_of(dep: &str, key: &str, v: &toml::Value) -> kiln_core::Result<String> {
    v.as_str().map(str::to_owned).ok_or_else(|| {
        kiln_core::Error::parse(format!("dependency {dep}.{key}"), "expected a string")
    })
}

fn bool_of(dep: &str, key: &str, v: &toml::Value) -> kiln_core::Result<bool> {
    v.as_bool().ok_or_else(|| {
        kiln_core::Error::parse(format!("dependency {dep}.{key}"), "expected true or false")
    })
}

fn string_list_of(dep: &str, key: &str, v: &toml::Value) -> kiln_core::Result<Vec<String>> {
    let array = v.as_array().ok_or_else(|| {
        kiln_core::Error::parse(
            format!("dependency {dep}.{key}"),
            "expected an array of strings",
        )
    })?;
    let mut out = Vec::with_capacity(array.len());
    for item in array {
        out.push(
            item.as_str()
                .ok_or_else(|| {
                    kiln_core::Error::parse(
                        format!("dependency {dep}.{key}"),
                        "every feature must be a string",
                    )
                })?
                .to_owned(),
        );
    }
    Ok(out)
}

/// Split a feature list's entries into dependency-features (`dep/feat`) and
/// this package's own features, the way cargo's `[features]` table uses them.
pub fn split_feature_ref(reference: &str) -> (Option<&str>, &str) {
    match reference.split_once('?') {
        Some((dep, rest)) => (Some(dep), rest),
        None => match reference.split_once('/') {
            Some((dep, rest)) => (Some(dep), rest),
            None => (None, reference),
        },
    }
}

/// The empty dependency map, for packages with no dependencies.
pub fn no_deps() -> BTreeMap<String, Dependency> {
    BTreeMap::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_dep(src: &str) -> kiln_core::Result<Dependency> {
        let value: toml::Value = toml::from_str(src).unwrap();
        let (name, value) = value.as_table().unwrap().iter().next().unwrap();
        Dependency::parse(
            name,
            value,
            DepKind::Normal,
            std::path::Path::new("/ws/crate"),
        )
    }

    #[test]
    fn bare_version_strings_parse() {
        let d = parse_dep("serde = \"1\"").unwrap();
        assert_eq!(d.crate_name(), "serde");
        assert!(d.req.matches(&semver::Version::new(1, 9, 9)));
        assert!(!d.req.matches(&semver::Version::new(2, 0, 0)));
        assert!(d.default_features);
    }

    #[test]
    fn table_dependencies_parse() {
        let d = parse_dep(
            "tokio = { version = \"1.40\", default-features = false, features = [\"rt\", \"macros\"], optional = true }",
        )
        .unwrap();
        assert!(d.optional);
        assert!(!d.default_features);
        assert_eq!(d.features, vec!["rt", "macros"]);
    }

    #[test]
    fn renames_and_paths_apply() {
        let d = parse_dep("inner = { path = \"../inner\", package = \"real-crate\" }").unwrap();
        assert_eq!(d.name, "inner");
        assert_eq!(d.crate_name(), "real-crate");
        assert_eq!(d.path.as_deref(), Some("../inner"));
    }

    #[test]
    fn git_dependencies_are_refused_by_name() {
        let err = parse_dep("foo = { git = \"https://github.com/x/foo\" }").unwrap_err();
        assert!(err.to_string().contains("git"), "{err}");
    }

    #[test]
    fn unknown_dependency_keys_are_refused() {
        let err = parse_dep("foo = { version = \"1\", mystery = 3 }").unwrap_err();
        assert!(err.to_string().contains("mystery"), "{err}");
    }

    #[test]
    fn feature_references_split_on_slash() {
        assert_eq!(split_feature_ref("serde/derive"), (Some("serde"), "derive"));
        assert_eq!(split_feature_ref("derive"), (None, "derive"));
    }
}

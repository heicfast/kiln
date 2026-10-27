//! One package's manifest: `Cargo.toml` parsed into the parts kiln models,
//! with everything unmodelled refused by name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use semver::Version;

use crate::cfg::TargetFacts;
use crate::dependency::{DepKind, Dependency};
use kiln_core::{Error, Result};

/// How a `[lib]` target asks to be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LibKind {
    /// `rlib` (the default): a static archive rustc links into dependents.
    Rlib,
    /// `proc-macro`: a dylib loaded by rustc itself.
    ProcMacro,
}

/// A named target of a package, discovered from the manifest or the
/// filesystem conventions (`src/bin/*.rs`, `tests/*.rs`, ...).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PackageTarget {
    /// The target's name; for libs the crate name.
    pub name: String,
    /// Where its entry point lives, relative to the package root.
    pub path: String,
    /// What kind of target it is, in kiln-core's terms.
    pub kind: kiln_core::unit::TargetKind,
    /// `required-features` from the manifest: the target exists only when
    /// every one of these is on.
    pub required_features: Vec<String>,
}

/// A parsed package manifest.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    /// The package's name.
    pub name: String,
    /// The package's version.
    pub version: Version,
    /// The edition, defaulted per cargo's rules.
    pub edition: crate::Edition,
    /// The resolver version the manifest (or its edition) implies.
    pub resolver: u8,
    /// `rust-version`, if declared.
    pub rust_version: Option<Version>,
    /// The package's own directory.
    pub root: PathBuf,
    /// A build script, when `build = "..."` or `build.rs` exists.
    pub build_script: Option<String>,
    /// A `links = "..."` declaration, for the build script's environment.
    pub links: Option<String>,
    /// Normal dependencies, by the name they are imported under.
    pub deps: BTreeMap<String, Dependency>,
    /// Dev dependencies (tests, examples, benches).
    pub dev_deps: BTreeMap<String, Dependency>,
    /// Build dependencies (the build script's own imports).
    pub build_deps: BTreeMap<String, Dependency>,
    /// The `[features]` table.
    pub features: BTreeMap<String, Vec<String>>,
    /// `[lib]`, when the package has one.
    pub lib: Option<PackageTarget>,
    /// The lib's crate kind.
    pub lib_kind: LibKind,
    /// `[[bin]]` targets.
    pub bins: Vec<PackageTarget>,
    /// `[[test]]` targets.
    pub tests: Vec<PackageTarget>,
    /// `[[bench]]` targets.
    pub benches: Vec<PackageTarget>,
    /// `[[example]]` targets.
    pub examples: Vec<PackageTarget>,
    /// Whether `[[bin]]` discovery by directory scan is on (default).
    pub autobins: bool,
    /// Whether `[[test]]` discovery by directory scan is on (default).
    pub autotests: bool,
    /// Whether `[[example]]` discovery by directory scan is on (default).
    pub autoexamples: bool,
    /// Whether `[[bench]]` discovery by directory scan is on (default).
    pub autobenches: bool,
}

impl Manifest {
    /// Parse the manifest at `root/Cargo.toml`.
    pub fn parse(root: &Path) -> Result<Self> {
        let file = root.join("Cargo.toml");
        let text = std::fs::read_to_string(&file).map_err(|e| Error::io("read", &file, e))?;
        Self::from_str(root, &text)
    }

    /// Parse manifest text for the package rooted at `root`.
    pub fn from_str(root: &Path, text: &str) -> Result<Self> {
        let table: toml::Table =
            toml::from_str(text).map_err(|e| Error::parse(format!("{}/Cargo.toml", root.display()), e))?;
        let pkg_table = table
            .get("package")
            .and_then(|v| v.as_table())
            .ok_or_else(|| Error::parse("Cargo.toml", "no [package] section (is this the workspace root?)"))?;

        let name = required_str(pkg_table, "name")?;
        // `version = { workspace = true }` is a placeholder the workspace pass
        // fills in; parsing accepts it and marks the manifest as inheriting.
        let mut version_inherited = false;
        let version = match pkg_table.get("version") {
            Some(toml::Value::Table(t)) if t.get("workspace").and_then(|v| v.as_bool()) == Some(true) => {
                version_inherited = true;
                Version::new(0, 0, 0)
            }
            Some(v) => as_version("package.version", v)?,
            None => {
                return Err(Error::parse(
                    "Cargo.toml",
                    "package.version is required (or `workspace = true`)",
                ))
            }
        };
        let mut edition_inherited = false;
        let edition = match pkg_table.get("edition") {
            Some(toml::Value::Table(t)) if t.get("workspace").and_then(|v| v.as_bool()) == Some(true) => {
                edition_inherited = true;
                crate::Edition::E2015
            }
            Some(v) => crate::Edition::parse(&required_str_value("package.edition", v)?)?,
            None => crate::Edition::E2015,
        };
        let resolver = match pkg_table.get("resolver") {
            Some(v) => as_u8("package.resolver", v)?,
            None => edition.default_resolver(),
        };
        let build_script = match pkg_table.get("build") {
            Some(toml::Value::Boolean(false)) => None,
            Some(v) => Some(required_str_value("package.build", v)?),
            None => {
                // The `build.rs` convention.
                if root.join("build.rs").is_file() {
                    Some("build.rs".to_owned())
                } else {
                    None
                }
            }
        };
        let links = pkg_table.get("links").and_then(|v| v.as_str()).map(str::to_owned);
        let rust_version = pkg_table
            .get("rust-version")
            .map(|v| as_version("package.rust-version", v))
            .transpose()?;
        let _ = (version_inherited, edition_inherited);

        // Feature-gated, target-specific and workspace-inherited rows are
        // handled by the workspace pass; here we read the plain tables.
        let mut features = BTreeMap::new();
        if let Some(feat) = table.get("features").and_then(|v| v.as_table()) {
            for (fname, list) in feat {
                let list = list
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect::<Vec<_>>())
                    .ok_or_else(|| {
                        Error::parse(format!("features.{fname}"), "expected an array of strings")
                    })?;
                features.insert(fname.clone(), list);
            }
        }

        let mut m = Manifest {
            name: name.to_owned(),
            version,
            edition,
            resolver,
            rust_version,
            root: root.to_path_buf(),
            build_script,
            links,
            deps: BTreeMap::new(),
            dev_deps: BTreeMap::new(),
            build_deps: BTreeMap::new(),
            features,
            lib: None,
            lib_kind: LibKind::Rlib,
            bins: Vec::new(),
            tests: Vec::new(),
            benches: Vec::new(),
            examples: Vec::new(),
            autobins: true,
            autotests: true,
            autoexamples: true,
            autobenches: true,
        };

        m.read_dep_table(&table, "dependencies", DepKind::Normal)?;
        m.read_dep_table(&table, "dev-dependencies", DepKind::Dev)?;
        m.read_dep_table(&table, "build-dependencies", DepKind::Build)?;
        m.read_dep_table(&table, "dev_dependencies", DepKind::Dev)?;
        m.read_dep_table(&table, "build_dependencies", DepKind::Build)?;

        // Target-specific tables, `[target.<triple>.*]` and
        // `[target.'cfg(...)'.*]`. The host facts decide which cfgs hold.
        let facts = TargetFacts::from_triple(&kiln_core::ids::TargetTriple::host().0);
        if let Some(targets) = table.get("target").and_then(|v| v.as_table()) {
            for (spec, tables) in targets {
                let applies = if let Some(cfg) = spec.strip_prefix("cfg(").and_then(|s| s.strip_suffix(')')) {
                    crate::cfg::parse_cfg(&format!("cfg({cfg})"))?.eval(&facts)?
                } else {
                    spec == &facts.os || spec.contains(&facts.arch)
                };
                if applies {
                    let tables = tables.as_table().ok_or_else(|| {
                        Error::parse(format!("target.{spec}"), "expected a table of tables")
                    })?;
                    for (key, value) in tables {
                        match key.as_str() {
                            "dependencies" => m.read_dep_value_table(spec, value, DepKind::Normal)?,
                            "dev-dependencies" | "dev_dependencies" => {
                                m.read_dep_value_table(spec, value, DepKind::Dev)?
                            }
                            "build-dependencies" | "build_dependencies" => {
                                m.read_dep_value_table(spec, value, DepKind::Build)?
                            }
                            other => {
                                return Err(Error::unsupported(format!(
                                    "target.{spec}.{other}: kiln does not model target-specific `{other}` tables yet"
                                )))
                            }
                        }
                    }
                }
            }
        }

        // `[patch]` and `[replace]` change what resolution means; refusing
        // them beats resolving something else than cargo would.
        for forbidden in ["patch", "replace"] {
            if table.contains_key(forbidden) {
                return Err(Error::unsupported(format!(
                    "[{forbidden}]: kiln does not model {forbidden} sections yet"
                )));
            }
        }

        m.read_lib(&table)?;
        m.read_target_lists(&table)?;
        for (key, value) in pkg_table {
            match key.as_str() {
                "autobins" => m.autobins = value.as_bool().unwrap_or(true),
                "autotests" => m.autotests = value.as_bool().unwrap_or(true),
                "autoexamples" => m.autoexamples = value.as_bool().unwrap_or(true),
                "autobenches" => m.autobenches = value.as_bool().unwrap_or(true),
                // Pure registry/publish metadata: no effect on a build.
                "description" | "readme" | "license" | "license-file" | "keywords" | "categories"
                | "homepage" | "documentation" | "repository" | "authors" | "exclude" | "include"
                | "publish" | "metadata" | "default-run" | "workspace" | "name" | "version"
                | "edition" | "rust-version" | "resolver" | "build" | "links" | "im-a-teapot"
                | "rust-version-lowercase" => {}
                other => {
                    return Err(Error::unsupported(format!(
                        "package.{other}: kiln does not model this manifest key yet; refusing rather than guessing its meaning"
                    )))
                }
            }
        }

        m.discover_by_convention();
        Ok(m)
    }

    fn read_dep_table(&mut self, table: &toml::Table, key: &str, kind: DepKind) -> Result<()> {
        if let Some(deps) = table.get(key).and_then(|v| v.as_table()) {
            for (name, value) in deps {
                // `workspace = true` rows are resolved against
                // [workspace.dependencies] by the workspace pass; here they
                // are skipped, not half-parsed.
                if let toml::Value::Table(t) = value {
                    if t.get("workspace").and_then(|v| v.as_bool()) == Some(true) {
                        continue;
                    }
                }
                let dep = Dependency::parse(name, value, kind, &self.root)?;
                self.insert_dep(kind, dep)?;
            }
        }
        Ok(())
    }

    fn read_dep_value_table(&mut self, spec: &str, value: &toml::Value, kind: DepKind) -> Result<()> {
        let deps = value.as_table().ok_or_else(|| {
            Error::parse(format!("target.{spec}"), "expected a table of dependencies")
        })?;
        for (name, value) in deps {
            let dep = Dependency::parse(name, value, kind, &self.root)?;
            self.insert_dep(kind, dep)?;
        }
        Ok(())
    }

    fn insert_dep(&mut self, kind: DepKind, dep: Dependency) -> Result<()> {
        let map = match kind {
            DepKind::Normal => &mut self.deps,
            DepKind::Dev => &mut self.dev_deps,
            DepKind::Build => &mut self.build_deps,
        };
        if let Some(existing) = map.get_mut(&dep.name) {
            // A target-specific table re-declaring a row: the more
            // restrictive one wins (features union, options from the
            // specific row), mirroring cargo's last-writer for same-name.
            existing.features.extend(dep.features);
            existing.features.sort();
            existing.features.dedup();
            return Ok(());
        }
        map.insert(dep.name.clone(), dep);
        Ok(())
    }

    fn read_lib(&mut self, table: &toml::Table) -> Result<()> {
        let lib = match table.get("lib") {
            Some(v) => v.as_table().ok_or_else(|| Error::parse("[lib]", "expected a table"))?,
            None => return Ok(()),
        };
        let name = lib
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.replace('-', "_"))
            .unwrap_or_else(|| self.name.replace('-', "_"));
        let path = lib
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| "src/lib.rs".to_owned());
        let crate_types: Vec<String> = lib
            .get("crate-type")
            .or_else(|| lib.get("crate_type"))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        let proc_macro_flag = lib.get("proc-macro").and_then(|v| v.as_bool()) == Some(true);
        self.lib_kind = if proc_macro_flag
            || crate_types.iter().any(|c| c == "proc-macro")
        {
            LibKind::ProcMacro
        } else if crate_types.is_empty() || crate_types.iter().any(|c| c == "rlib") {
            LibKind::Rlib
        } else {
            return Err(Error::unsupported(format!(
                "[lib] crate-type = {crate_types:?}: kiln builds rlib and proc-macro libraries; dylib and cdylib libraries are not supported yet"
            )));
        };
        for (key, _) in lib {
            if !matches!(key.as_str(), "name" | "path" | "crate-type" | "crate_type" | "test" | "doctest" | "doc" | "bench" | "plugin" | "harness" | "required-features" | "proc-macro") {
                return Err(Error::unsupported(format!("[lib] {key}: kiln does not model this key yet")));
            }
        }
        self.lib = Some(PackageTarget {
            name,
            path,
            kind: if self.lib_kind == LibKind::ProcMacro {
                kiln_core::unit::TargetKind::ProcMacro
            } else {
                kiln_core::unit::TargetKind::Lib
            },
            required_features: Vec::new(),
        });
        Ok(())
    }

    fn read_target_lists(&mut self, table: &toml::Table) -> Result<()> {
        for (key, dest) in [
            ("bin", 0usize),
            ("test", 1),
            ("bench", 2),
            ("example", 3),
        ] {
            let Some(value) = table.get(key) else { continue };
            let array = value.as_array().ok_or_else(|| {
                Error::parse(format!("[[{key}]]"), "expected an array of tables")
            })?;
            for entry in array {
                let entry = entry.as_table().ok_or_else(|| {
                    Error::parse(format!("[[{key}]]"), "every entry must be a table")
                })?;
                let default_path = match key {
                    "bin" => format!("src/bin/{}.rs", entry.get("name").and_then(|v| v.as_str()).unwrap_or_default()),
                    "test" => format!("tests/{}.rs", entry.get("name").and_then(|v| v.as_str()).unwrap_or_default()),
                    "bench" => format!("benches/{}.rs", entry.get("name").and_then(|v| v.as_str()).unwrap_or_default()),
                    _ => format!("examples/{}.rs", entry.get("name").and_then(|v| v.as_str()).unwrap_or_default()),
                };
                let name = entry
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                    .ok_or_else(|| Error::parse(format!("[[{key}]]"), "name is required"))?;
                let path = entry
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                    .unwrap_or(default_path);
                let required_features = entry
                    .get("required-features")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
                    .unwrap_or_default();
                for (k, _) in entry {
                    if !matches!(k.as_str(), "name" | "path" | "required-features" | "test" | "doc" | "bench" | "harness" | "doctest" | "edition" | "plugin") {
                        return Err(Error::unsupported(format!(
                            "[[{key}]] {k}: kiln does not model this key yet"
                        )));
                    }
                }
                let target = PackageTarget {
                    name,
                    path,
                    kind: match key {
                        "bin" => kiln_core::unit::TargetKind::Bin,
                        "test" => kiln_core::unit::TargetKind::Test,
                        "bench" => kiln_core::unit::TargetKind::Bench,
                        _ => kiln_core::unit::TargetKind::Example,
                    },
                    required_features,
                };
                match dest {
                    0 => self.bins.push(target),
                    1 => self.tests.push(target),
                    2 => self.benches.push(target),
                    _ => self.examples.push(target),
                }
            }
        }
        Ok(())
    }

    /// Discover targets from the filesystem where the manifest left them
    /// implicit: `src/lib.rs`, `src/main.rs`, `src/bin/*.rs`, `tests/*.rs`,
    /// `benches/*.rs`, `examples/*.rs`.
    fn discover_by_convention(&mut self) {
        if self.lib.is_none() && self.root.join("src/lib.rs").is_file() {
            self.lib = Some(PackageTarget {
                name: self.name.replace('-', "_"),
                path: "src/lib.rs".to_owned(),
                kind: kiln_core::unit::TargetKind::Lib,
                required_features: Vec::new(),
            });
        }
        let main_rs = self.root.join("src/main.rs");
        if main_rs.is_file()
            && self.bins.iter().all(|b| b.path != "src/main.rs")
        {
            self.bins.insert(
                0,
                PackageTarget {
                    name: self.name.clone(),
                    path: "src/main.rs".to_owned(),
                    kind: kiln_core::unit::TargetKind::Bin,
                    required_features: Vec::new(),
                },
            );
        }
        let scan = |dir: &str| -> Vec<PackageTarget> {
            let dir = self.root.join(dir);
            let mut out = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("rs") && path.is_file() {
                        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_owned();
                        let rel = path
                            .strip_prefix(&self.root)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .replace('\\', "/");
                        out.push(PackageTarget {
                            name: stem,
                            path: rel,
                            kind: kiln_core::unit::TargetKind::Bin,
                            required_features: Vec::new(),
                        });
                    }
                }
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            out
        };

        if self.autobins {
            for t in scan("src/bin") {
                if !self.bins.iter().any(|x| x.path == t.path) {
                    self.bins.push(PackageTarget {
                        kind: kiln_core::unit::TargetKind::Bin,
                        ..t
                    });
                }
            }
        }
        if self.autotests {
            for t in scan("tests") {
                if !self.tests.iter().any(|x| x.path == t.path) {
                    self.tests.push(PackageTarget { kind: kiln_core::unit::TargetKind::Test, ..t });
                }
            }
        }
        if self.autobenches {
            for t in scan("benches") {
                if !self.benches.iter().any(|x| x.path == t.path) {
                    self.benches.push(PackageTarget { kind: kiln_core::unit::TargetKind::Bench, ..t });
                }
            }
        }
        if self.autoexamples {
            for t in scan("examples") {
                if !self.examples.iter().any(|x| x.path == t.path) {
                    self.examples.push(PackageTarget { kind: kiln_core::unit::TargetKind::Example, ..t });
                }
            }
        }
    }
}

fn required_str<'a>(table: &'a toml::Table, key: &str) -> Result<&'a str> {
    table
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::parse("Cargo.toml", format!("{key} is required and must be a string")))
}

fn required_str_value(key: &str, v: &toml::Value) -> Result<String> {
    v.as_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::parse(key, "expected a string"))
}

fn as_version(key: &str, v: &toml::Value) -> Result<Version> {
    let text = v
        .as_str()
        .ok_or_else(|| Error::parse(key, "expected a version string like \"1.2.3\""))?;
    // Cargo accepts two-part versions (`"1.80"`) where semver wants three;
    // a missing patch is a zero patch.
    let padded = match text.split('.').count() {
        2 => format!("{text}.0"),
        _ => text.to_owned(),
    };
    padded
        .parse()
        .map_err(|e| Error::parse(format!("{key} `{text}`"), e))
}

fn as_u8(key: &str, v: &toml::Value) -> Result<u8> {
    v.as_integer()
        .and_then(|i| u8::try_from(i).ok())
        .ok_or_else(|| Error::parse(key, "expected 1 or 2"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(root: &Path, text: &str) -> Result<Manifest> {
        Manifest::from_str(root, text)
    }

    #[test]
    fn a_full_manifest_parses() {
        let root = std::env::temp_dir().join("kiln-m1");
        std::fs::create_dir_all(root.join("src/bin")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "").ok();
        let m = parse(
            &root,
            r#"
[package]
name = "my-app"
version = "1.2.3"
edition = "2021"
rust-version = "1.80"

[dependencies]
serde = "1"
serde_json = { version = "1.0", features = ["preserve_order"], optional = true }

[dev-dependencies]
pretty_assertions = "1"

[build-dependencies]
cc = "1"

[features]
default = ["serde"]
fast = ["serde_json"]

[[bin]]
name = "server"
path = "src/bin/server.rs"
"#,
        )
        .unwrap();
        assert_eq!(m.name, "my-app");
        assert_eq!(m.version, Version::new(1, 2, 3));
        assert_eq!(m.edition, crate::Edition::E2021);
        assert_eq!(m.resolver, 2);
        assert!(m.deps.contains_key("serde"));
        assert!(m.dev_deps.contains_key("pretty_assertions"));
        assert!(m.build_deps.contains_key("cc"));
        assert!(m.deps["serde_json"].optional);
        assert_eq!(m.features["default"], vec!["serde"]);
        assert_eq!(m.bins.len(), 1);
        assert_eq!(m.bins[0].name, "server");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn conventions_discover_targets() {
        let root = std::env::temp_dir().join("kiln-m2");
        std::fs::create_dir_all(root.join("src/bin")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("src/bin/cli.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("tests/it.rs"), "").unwrap();
        let m = parse(
            &root,
            r#"
[package]
name = "conv"
version = "0.1.0"
"#,
        )
        .unwrap();
        assert!(m.bins.iter().any(|b| b.name == "cli"));
        assert!(m.tests.iter().any(|t| t.name == "it"));
        assert!(m.lib.is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn proc_macro_libs_are_recognised() {
        let m = parse(
            &std::env::temp_dir(),
            r#"
[package]
name = "my-macro"
version = "0.1.0"

[lib]
proc-macro = true
"#,
        )
        .unwrap();
        assert_eq!(m.lib_kind, LibKind::ProcMacro);
        assert_eq!(m.lib.as_ref().unwrap().kind, kiln_core::unit::TargetKind::ProcMacro);
    }

    #[test]
    fn target_specific_deps_follow_cfgs() {
        let m = parse(
            &std::env::temp_dir(),
            r#"
[package]
name = "plat"
version = "0.1.0"

[target.'cfg(unix)'.dependencies]
libc = "0.2"

[target.x86_64-unknown-linux-gnu.dependencies]
openssl = "0.10"
"#,
        )
        .unwrap();
        let is_linux = std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64";
        assert_eq!(m.deps.contains_key("libc"), std::env::consts::OS != "windows");
        assert_eq!(m.deps.contains_key("openssl"), is_linux);
    }

    #[test]
    fn unmodelled_sections_are_refused_by_name() {
        let e = parse(
            &std::env::temp_dir(),
            r#"
[package]
name = "x"
version = "0.1.0"

[patch.crates-io]
serde = { path = "../serde" }
"#,
        )
        .unwrap_err();
        assert!(e.to_string().contains("patch"), "{e}");
    }

    #[test]
    fn dylib_libs_are_refused() {
        let e = parse(
            &std::env::temp_dir(),
            r#"
[package]
name = "x"
version = "0.1.0"

[lib]
crate-type = ["dylib"]
"#,
        )
        .unwrap_err();
        assert!(e.to_string().contains("dylib"), "{e}");
    }
}

//! Build scripts: compile them, run them sandboxed, parse their directives.
//!
//! A build script's output is the one legitimate way arbitrary facts enter a
//! build. Kiln runs each script once, keyed by the script's own unit key and
//! its inputs, caches its parsed output, and feeds it into the fingerprints
//! and argvs of the package's units — exactly the contract `build.rs` has
//! under cargo.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kiln_core::{Error, Result};
use serde::{Deserialize, Serialize};

/// One parsed `cargo:` directive (plus the ones cargo itself defines).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Directive {
    /// `cargo:rustc-cfg=NAME` — a `--cfg` for the package's units.
    RustcCfg(String),
    /// `cargo:rustc-flags=FLAGS` — literal flags (space-separated).
    RustcFlags(String),
    /// `cargo:rustc-link-lib=KIND=NAME` — `-l` for links.
    RustcLinkLib(String),
    /// `cargo:rustc-link-search=PATH` — `-L` for links.
    RustcLinkSearch(String),
    /// `cargo:rustc-env=VAR=VALUE` — env for the units' compilation.
    RustcEnv(String, String),
    /// `cargo:rustc-cdylib-link-arg=ARG` — link args for cdylibs.
    CdylibLinkArg(String),
    /// `cargo:rerun-if-changed=PATH` — an input of the script.
    RerunIfChanged(String),
    /// `cargo:rerun-if-env-changed=VAR` — an env input of the script.
    RerunIfEnvChanged(String),
    /// `cargo:warning=TEXT` — a user-facing warning.
    Warning(String),
    /// Anything else (`cargo:KEY=VALUE`) — metadata for other tools.
    Metadata(String, String),
}

/// A build script's full parsed output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildScriptOutput {
    /// Every directive, in print order.
    pub directives: Vec<Directive>,
    /// The script's stdout beyond directives (blank lines dropped).
    pub stdout: Vec<String>,
}

impl BuildScriptOutput {
    /// Parse a script's raw stdout.
    pub fn parse(stdout: &str) -> Self {
        let mut out = BuildScriptOutput::default();
        for line in stdout.lines() {
            if let Some(rest) = line.strip_prefix("cargo:") {
                if let Some((key, value)) = rest.split_once('=') {
                    let directive = match key {
                        "rustc-cfg" => Directive::RustcCfg(value.to_owned()),
                        "rustc-flags" => Directive::RustcFlags(value.to_owned()),
                        "rustc-link-lib" => Directive::RustcLinkLib(value.to_owned()),
                        "rustc-link-search" => Directive::RustcLinkSearch(value.to_owned()),
                        "rustc-env" => {
                            let (var, val) = value.split_once('=').unwrap_or((value, ""));
                            Directive::RustcEnv(var.to_owned(), val.to_owned())
                        }
                        "rustc-cdylib-link-arg" => Directive::CdylibLinkArg(value.to_owned()),
                        "rerun-if-changed" => Directive::RerunIfChanged(value.to_owned()),
                        "rerun-if-env-changed" => Directive::RerunIfEnvChanged(value.to_owned()),
                        "warning" => Directive::Warning(value.to_owned()),
                        other => Directive::Metadata(other.to_owned(), value.to_owned()),
                    };
                    out.directives.push(directive);
                } else if !rest.is_empty() {
                    out.directives.push(Directive::Metadata(rest.to_owned(), String::new()));
                }
            } else if !line.trim().is_empty() {
                out.stdout.push(line.to_owned());
            }
        }
        out
    }

    /// The `--cfg` flags for the package's units.
    pub fn rustc_cfg_flags(&self) -> Vec<String> {
        let mut flags = Vec::new();
        for d in &self.directives {
            if let Directive::RustcCfg(cfg) = d {
                flags.push("--cfg".to_owned());
                flags.push(cfg.clone());
            }
        }
        flags
    }

    /// Extra literal rustc flags (`rustc-flags` and `link-lib`/`link-search`).
    pub fn rustc_flag_args(&self) -> Vec<String> {
        let mut flags = Vec::new();
        for d in &self.directives {
            match d {
                Directive::RustcFlags(raw) => flags.extend(raw.split_whitespace().map(str::to_owned)),
                Directive::RustcLinkLib(spec) => {
                    flags.push("-l".to_owned());
                    flags.push(spec.clone());
                }
                Directive::RustcLinkSearch(path) => {
                    flags.push("-L".to_owned());
                    flags.push(format!("native={path}"));
                }
                _ => {}
            }
        }
        flags
    }

    /// The environment variables the units' compilation runs under.
    pub fn env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        for d in &self.directives {
            if let Directive::RustcEnv(var, value) = d {
                env.insert(var.clone(), value.clone());
            }
        }
        env
    }

    /// Paths whose change re-runs the script (`rerun-if-changed`; empty
    /// means cargo's default: any change of the package).
    pub fn rerun_if_changed(&self) -> Vec<String> {
        self.directives
            .iter()
            .filter_map(|d| match d {
                Directive::RerunIfChanged(p) => Some(p.clone()),
                _ => None,
            })
            .collect()
    }

    /// Environment variables whose change re-runs the script.
    pub fn rerun_if_env_changed(&self) -> Vec<String> {
        self.directives
            .iter()
            .filter_map(|d| match d {
                Directive::RerunIfEnvChanged(v) => Some(v.clone()),
                _ => None,
            })
            .collect()
    }

    /// Serialised form for fingerprints and the store.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// The OUT_DIR a script's units will see.
    pub fn out_dir(build_dir: &Path) -> PathBuf {
        build_dir.join("out")
    }
}

/// The environment a build script runs under, per cargo's contract.
pub fn script_environment(
    package_root: &Path,
    package_name: &str,
    package_version: &str,
    target_triple: &str,
    host_triple: &str,
    out_dir: &Path,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("CARGO_MANIFEST_DIR".into(), package_root.to_string_lossy().into_owned());
    env.insert("CARGO_PKG_NAME".into(), package_name.to_owned());
    env.insert("CARGO_PKG_VERSION".into(), package_version.to_owned());
    env.insert("CARGO_PKG_VERSION_MAJOR".into(), package_version.split('.').next().unwrap_or("0").to_owned());
    env.insert("OUT_DIR".into(), out_dir.to_string_lossy().into_owned());
    env.insert("TARGET".into(), target_triple.to_owned());
    env.insert("HOST".into(), host_triple.to_owned());
    env.insert("CARGO".into(), "kiln".into());
    env
}

/// Run a compiled build script executable and parse its output.
pub fn run_script(
    exe: &Path,
    package_root: &Path,
    env: &BTreeMap<String, String>,
    sandbox: &crate::sandbox::SandboxPolicy,
) -> Result<BuildScriptOutput> {
    let output = sandbox.run_build_script(exe, package_root, env)?;
    if !output.stderr.is_empty() {
        let text = String::from_utf8_lossy(&output.stderr);
        for line in text.lines() {
            eprintln!("   build-script stderr: {line}");
        }
    }
    if !output.status.success() {
        // The last 15 stderr lines: enough context, never a wall of text.
        let tail: String = String::from_utf8_lossy(&output.stderr)
            .lines()
            .rev()
            .take(15)
            .collect::<Vec<_>>()
            .join("\n");
        return Err(Error::BuildScript {
            package: package_root.display().to_string(),
            message: format!("exit {}:\n{tail}", output.status),
        });
    }
    Ok(BuildScriptOutput::parse(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"cargo:rustc-cfg=feature="native"
cargo:rustc-env=VERSION=1.2.3
cargo:rustc-flags=-l static=z
cargo:rustc-link-search=native=/opt/lib
cargo:rerun-if-changed=build/tables.txt
cargo:rerun-if-env-changed=ZLIB_VERSION
cargo:warning=building without zlib
plain stdout line
cargo:metadata=anything
"#;

    #[test]
    fn directives_parse_and_classify() {
        let out = BuildScriptOutput::parse(SAMPLE);
        assert_eq!(out.directives.len(), 8);
        assert_eq!(out.rustc_cfg_flags(), vec!["--cfg", "feature=\"native\""]);
        assert!(out.rustc_flag_args().contains(&"-l".to_owned()));
        assert!(out.rustc_flag_args().iter().any(|f| f == "static=z"));
        assert!(out.rustc_flag_args().contains(&"native=/opt/lib".to_owned()) || out.rustc_flag_args().contains(&"-L".to_owned()));
        assert_eq!(out.env().get("VERSION").map(String::as_str), Some("1.2.3"));
        assert_eq!(out.rerun_if_changed(), vec!["build/tables.txt"]);
        assert_eq!(out.rerun_if_env_changed(), vec!["ZLIB_VERSION"]);
        assert_eq!(out.stdout, vec!["plain stdout line"]);
        match &out.directives[6] {
            Directive::Warning(w) => assert_eq!(w, "building without zlib"),
            other => panic!("expected warning, got {other:?}"),
        }
        match &out.directives[7] {
            Directive::Metadata(key, value) => {
                assert_eq!(key, "metadata");
                assert_eq!(value, "anything");
            }
            other => panic!("expected metadata, got {other:?}"),
        }
    }

    #[test]
    fn malformed_lines_do_not_crash_the_parse() {
        let out = BuildScriptOutput::parse("cargo:\ncargo:x\ngarbage\n\n");
        assert!(out.stdout.contains(&"garbage".to_owned()));
        assert!(out.directives.is_empty() || out.directives.len() == 1);
    }

    #[test]
    fn output_round_trips_through_bytes() {
        let out = BuildScriptOutput::parse(SAMPLE);
        let bytes = out.to_bytes();
        let back: BuildScriptOutput = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out, back);
    }

    #[test]
    fn script_environment_has_the_contract_vars() {
        let env = script_environment(
            Path::new("/ws/pkg"),
            "pkg",
            "1.2.3",
            "x86_64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
            Path::new("/ws/target/debug/build/pkg-abc/out"),
        );
        assert_eq!(env["CARGO_MANIFEST_DIR"], "/ws/pkg");
        assert_eq!(env["CARGO_PKG_VERSION_MAJOR"], "1");
        assert_eq!(env["TARGET"], "x86_64-unknown-linux-gnu");
        assert_eq!(env["HOST"], "x86_64-unknown-linux-gnu");
        assert!(env["OUT_DIR"].ends_with("out"));
    }
}

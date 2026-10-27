//! The rustc driver: builds the exact argv for one unit and runs it.
//!
//! The argv is cargo's, deliberately: the same `--extern` layout, the same
//! `-Cmetadata` disambiguation, the same `--out-dir` structure. A unit
//! compiled by kiln and a unit compiled by cargo are interchangeable, and
//! the artifacts land in `target/` where every tool expects them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use kiln_core::profile::Profile;
use kiln_core::unit::{CompileMode, TargetKind, Unit};
use kiln_core::{Error, Result};

/// The file names a unit's compilation produces, relative to the out dir.
pub struct UnitOutputs {
    /// The rlib (`deps/libname-hash.rlib`), for libs and proc-macros.
    pub rlib: Option<String>,
    /// The rmeta (`deps/libname-hash.rmeta`), for check-mode units.
    pub rmeta: Option<String>,
    /// The binary (`deps/name-hash`), for executable targets.
    pub bin: Option<String>,
    /// The dep-info file (`deps/name-hash.d`).
    pub dep_info: String,
}

/// The environment a unit compiles under: build script output and the
/// package's own facts.
pub struct UnitEnv {
    /// `cargo:` directives from the package's build script, applied:
    /// rustc flags and cfgs verbatim, env vars into the compile environment.
    pub rustc_flags: Vec<String>,
    /// Environment variables the build script printed (`cargo:rustc-env`).
    pub env: BTreeMap<String, String>,
    /// `OUT_DIR`, when a build script produced one.
    pub out_dir: Option<PathBuf>,
    /// The package's `links = "..."` value.
    pub links: Option<String>,
}

impl UnitEnv {
    /// No build script output.
    pub fn plain() -> Self {
        Self { rustc_flags: Vec::new(), env: BTreeMap::new(), out_dir: None, links: None }
    }
}

/// Build the argv for one unit.
///
/// `dep_externs` maps each dependency's crate name to the file rustc should
/// `--extern` against (its rlib or rmeta), in the dependency's own spelling.
#[allow(clippy::too_many_arguments)]
pub fn rustc_argv(
    unit: &Unit,
    planned_edition: &str,
    root: &Path,
    profile: &Profile,
    out_dir: &Path,
    target_dir: &Path,
    target_triple: Option<&str>,
    dep_externs: &[(String, PathBuf)],
    env: &UnitEnv,
    unit_short: &str,
    rustflags: &[String],
    linker_args: &[String],
) -> Result<Vec<String>> {
    let mut argv: Vec<String> = Vec::with_capacity(64);
    let crate_name = unit.crate_name();

    // The entry file.
    argv.push(root.join(&unit.target_path).to_string_lossy().into_owned());

    argv.push("--crate-name".into());
    argv.push(crate_name.clone());

    // Crate type by target kind and mode.
    let is_check = unit.mode == CompileMode::Check;
    match unit.kind {
        TargetKind::Lib => {
            if is_check {
                // Check mode: metadata only; dependents --extern the rmeta.
                argv.push("--emit=dep-info,metadata".into());
            } else {
                argv.push("--emit=dep-info,metadata,link".into());
                argv.push("--crate-type".into());
                argv.push("rlib".into());
            }
        }
        TargetKind::ProcMacro => {
            argv.push("--emit=dep-info,metadata,link".into());
            argv.push("--crate-type".into());
            argv.push("proc-macro".into());
        }
        TargetKind::BuildScript | TargetKind::Bin | TargetKind::Test | TargetKind::Bench
        | TargetKind::Example | TargetKind::TestHarness => {
            argv.push("--emit=dep-info,metadata,link".into());
            argv.push("--crate-type".into());
            argv.push("bin".into());
        }
    }

    argv.push("-C".into());
    argv.push(format!("metadata={}", short_metadata(unit, planned_edition, unit_short)));

    // Edition.
    argv.push("--edition".into());
    argv.push(planned_edition.to_owned());

    // Profile flags.
    argv.push("-C".into());
    argv.push(format!("opt-level={}", profile.opt_level));
    if profile.debug > 0 {
        argv.push("-C".into());
        argv.push(if profile.debug >= 2 { "debuginfo=2".into() } else { "debuginfo=1".into() });
    }
    if profile.debug_assertions {
        argv.push("-C".into());
        argv.push("debug-assertions=on".into());
    }
    if profile.overflow_checks {
        argv.push("-C".into());
        argv.push("overflow-checks=on".into());
    }
    if let Some(lto) = profile.lto.as_str() {
        argv.push("-C".into());
        argv.push(format!("lto={lto}"));
    }
    if let Some(cu) = profile.codegen_units {
        argv.push("-C".into());
        argv.push(format!("codegen-units={cu}"));
    }
    argv.push("-C".into());
    argv.push(format!("panic={}", profile.panic.as_str()));

    // Incremental is off: the artifact cache subsumes it and stays valid
    // across machines, which incremental directories never are.
    argv.push("-C".into());
    argv.push("incremental=false".into());

    // The fast linker, when one was chosen (final artifacts only).
    if matches!(unit.kind, TargetKind::Bin | TargetKind::Test | TargetKind::Bench | TargetKind::Example | TargetKind::BuildScript) {
        argv.extend(linker_args.iter().cloned());
    }

    // User RUSTFLAGS, verbatim.
    argv.extend(rustflags.iter().cloned());

    // Build script output flags.
    argv.extend(env.rustc_flags.iter().cloned());

    // Test harness units compile with --test.
    if unit.test {
        argv.push("--test".into());
    }

    // cfg(test) is what separates a harness from the lib it re-compiles.
    if unit.test {
        argv.push("--cfg".into());
        argv.push("test".into());
    }

    // Features as cfgs: `feature="serde"` for each enabled feature.
    for feature in unit.features.iter() {
        argv.push("--cfg".into());
        argv.push(format!("feature=\"{feature}\""));
    }

    // Target.
    if let Some(triple) = target_triple {
        argv.push("--target".into());
        argv.push(triple.to_owned());
    }

    // Dependencies: --extern name=path, plus -L for the deps dir.
    for (name, path) in dep_externs {
        argv.push("--extern".into());
        argv.push(format!("{name}={}", path.display()));
    }
    argv.push("-L".into());
    argv.push(format!("dependency={}", out_dir.display()));
    argv.push("-L".into());
    argv.push(format!("dependency={}", target_dir.join("deps").display()));

    // OUT_DIR and friends reach the compiler through the environment
    // (build scripts and `env!` reads).
    let mut env_map = env.env.clone();
    if let Some(out) = &env.out_dir {
        env_map.insert("OUT_DIR".into(), out.to_string_lossy().into_owned());
    }
    if let Some(links) = &env.links {
        env_map.insert(format!("CARGO_PKG_{}", links.to_uppercase().replace('-', "_")), env_map.get("CARGO_MANIFEST_LINKS").cloned().unwrap_or_default());
    }

    // Where the outputs go.
    argv.push("--out-dir".into());
    argv.push(out_dir.display().to_string());

    // Errors point at the sources, not at kiln.
    argv.push("--remap-path-prefix".into());
    argv.push(format!("{}=/rustc/kiln", root.display()));

    Ok(argv)
}

/// The outputs a unit's compilation produces, by name convention.
pub fn unit_outputs(unit: &Unit, unit_short: &str) -> UnitOutputs {
    let crate_name = unit.crate_name();
    let base = format!("{crate_name}-{unit_short}");
    let lib_base = format!("lib{crate_name}-{unit_short}");
    let is_check = unit.mode == CompileMode::Check;
    UnitOutputs {
        rlib: matches!(unit.kind, TargetKind::Lib | TargetKind::ProcMacro)
            .then(|| format!("deps/{lib_base}.rlib"))
            .filter(|_| !is_check),
        rmeta: matches!(unit.kind, TargetKind::Lib | TargetKind::ProcMacro)
            .then(|| format!("deps/{lib_base}.rmeta")),
        bin: matches!(
            unit.kind,
            TargetKind::Bin | TargetKind::Test | TargetKind::Bench | TargetKind::Example | TargetKind::BuildScript | TargetKind::TestHarness
        )
        .then(|| format!("deps/{base}"))
        .filter(|_| !is_check),
        dep_info: format!("deps/{base}.d"),
    }
}

/// The `-Cmetadata` payload: enough to keep same-named crates apart, short
/// enough not to blow up symbol names.
fn short_metadata(unit: &Unit, edition: &str, unit_short: &str) -> String {
    let kind = match unit.kind {
        TargetKind::Lib => "lib",
        TargetKind::ProcMacro => "proc",
        TargetKind::Bin => "bin",
        TargetKind::Test => "test",
        TargetKind::Bench => "bench",
        TargetKind::Example => "ex",
        TargetKind::BuildScript => "build",
        TargetKind::TestHarness => "harness",
    };
    format!("kiln{kind}{}{}", edition, &unit_short[..8.min(unit_short.len())])
}

/// Run one rustc invocation, inheriting stdout/stderr so the user sees
/// diagnostics live, with the unit's environment applied.
pub fn run_rustc(
    rustc: &Path,
    argv: &[String],
    env_map: &BTreeMap<String, String>,
    cwd: &Path,
) -> Result<std::process::Output> {
    let mut cmd = Command::new(rustc);
    cmd.args(argv).current_dir(cwd).env_remove("CARGO");
    for (k, v) in env_map {
        cmd.env(k, v);
    }
    // Diagnostics go to the terminal as they happen.
    cmd.stdout(std::process::Stdio::inherit()).stderr(std::process::Stdio::inherit());
    let output = cmd.output().map_err(|e| Error::io("run", rustc, e))?;
    if !output.status.success() {
        return Err(Error::CommandFailed {
            command: format!("{} {}", rustc.display(), argv.join(" ")),
            status: format!("{}", output.status),
        });
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_core::ids::{PackageId, SourceId};
    use semver::Version;
    use std::collections::BTreeSet;

    fn unit(kind: TargetKind, test: bool) -> Unit {
        Unit {
            pkg: PackageId::registry("my-crate", Version::new(1, 0, 0), SourceId::CRATES_IO),
            target_name: "my-crate".into(),
            target_path: "src/lib.rs".into(),
            kind,
            test,
            mode: CompileMode::Build,
            features: BTreeSet::from(["serde".to_owned()]),
        }
    }

    #[test]
    fn a_lib_argv_is_cargo_shaped() {
        let u = unit(TargetKind::Lib, false);
        let argv = rustc_argv(
            &u,
            "2021",
            Path::new("/ws/my-crate"),
            &Profile::dev(),
            Path::new("/ws/target/debug"),
            Path::new("/ws/target/debug"),
            None,
            &[("dep".into(), PathBuf::from("/ws/target/debug/deps/libdep-abc.rlib"))],
            &UnitEnv::plain(),
            "abcd1234efgh5678",
            &[],
            &[],
        )
        .unwrap();
        let joined = argv.join(" ");
        assert!(argv[0].ends_with("src/lib.rs"), "entry file first: {argv:?}");
        assert!(joined.contains("--crate-name my-crate"));
        assert!(joined.contains("--crate-type rlib"));
        assert!(joined.contains("--edition 2021"));
        assert!(joined.contains("opt-level=0"));
        assert!(joined.contains("--extern dep=/ws/target/debug/deps/libdep-abc.rlib"));
        assert!(joined.contains("feature=\"serde\""));
        assert!(joined.contains("-C metadata=kilnlib2021abcd1234"));
        assert!(joined.contains("incremental=false"));
        assert!(joined.contains("--remap-path-prefix"));
    }

    #[test]
    fn proc_macros_and_bins_and_tests_differ() {
        let pm = unit(TargetKind::ProcMacro, false);
        let argv = rustc_argv(&pm, "2021", Path::new("/ws"), &Profile::dev(), Path::new("/t"), Path::new("/t"), None, &[], &UnitEnv::plain(), "1234567812345678", &[], &[]).unwrap();
        assert!(argv.contains(&"proc-macro".to_string()));

        let bin = unit(TargetKind::Bin, false);
        let argv = rustc_argv(&bin, "2021", Path::new("/ws"), &Profile::dev(), Path::new("/t"), Path::new("/t"), None, &[], &UnitEnv::plain(), "1234567812345678", &[], &[]).unwrap();
        assert!(argv.contains(&"bin".to_string()));

        let harness = unit(TargetKind::TestHarness, true);
        let argv = rustc_argv(&harness, "2021", Path::new("/ws"), &Profile::dev(), Path::new("/t"), Path::new("/t"), None, &[], &UnitEnv::plain(), "1234567812345678", &[], &[]).unwrap();
        assert!(argv.contains(&"--test".to_string()));
        assert!(argv.contains(&"test".to_string()));
    }

    #[test]
    fn check_mode_emits_metadata_only() {
        let mut u = unit(TargetKind::Lib, false);
        u.mode = CompileMode::Check;
        let argv = rustc_argv(&u, "2021", Path::new("/ws"), &Profile::dev(), Path::new("/t"), Path::new("/t"), None, &[], &UnitEnv::plain(), "1234567812345678", &[], &[]).unwrap();
        let joined = argv.join(" ");
        assert!(joined.contains("--emit=dep-info,metadata"));
        assert!(!joined.contains("--crate-type"));
    }

    #[test]
    fn build_script_flags_and_env_flow_through() {
        let u = unit(TargetKind::Lib, false);
        let mut env = UnitEnv::plain();
        env.rustc_flags = vec!["--cfg".into(), "has_feature".into(), "-C".into(), "target-feature=+sse2".into()];
        env.env.insert("SOME_VAR".into(), "value".into());
        env.out_dir = Some(PathBuf::from("/ws/target/debug/build/my-crate-abc/out"));
        let argv = rustc_argv(&u, "2021", Path::new("/ws"), &Profile::dev(), Path::new("/t"), Path::new("/t"), None, &[], &env, "1234567812345678", &[], &[]).unwrap();
        let joined = argv.join(" ");
        assert!(joined.contains("has_feature"));
        assert!(joined.contains("target-feature=+sse2"));
    }

    #[test]
    fn outputs_follow_the_deps_layout() {
        let u = unit(TargetKind::Lib, false);
        let out = unit_outputs(&u, "abcd1234efgh5678");
        assert_eq!(out.rlib.as_deref(), Some("deps/libmy-crate-abcd1234efgh5678.rlib"));
        assert_eq!(out.rmeta.as_deref(), Some("deps/libmy-crate-abcd1234efgh5678.rmeta"));
        let u = unit(TargetKind::Bin, false);
        let out = unit_outputs(&u, "abcd1234efgh5678");
        assert_eq!(out.bin.as_deref(), Some("deps/my-crate-abcd1234efgh5678"));
    }
}

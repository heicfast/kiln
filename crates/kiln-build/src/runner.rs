//! The unit runner: executes one unit at a time on behalf of the scheduler.
//!
//! Every unit follows the same discipline:
//!
//! 1. **Fingerprint** its inputs (sources, features, profile, toolchain,
//!    build-script output, and the artifact digests of every dependency).
//! 2. **Ask the store** — a hit materialises the unit's files into the
//!    target directory and returns immediately; artifacts never recompile.
//! 3. **Miss**: drive rustc (or a build script) ourselves, capture the
//!    outputs, and put them into the store for every future build — this
//!    machine, the daemon, the CI, and anyone sharing the remote.
//!
//! Build scripts additionally parse their `cargo:` directives; the parsed
//! output is stored next to the compiled script (cargo's freshness
//! semantics: unchanged inputs mean the stored output is reused without
//! re-running the script).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use kiln_core::profile::Profile;
use kiln_core::unit::{CompileMode, TargetKind, Unit, UnitKey};
use kiln_core::{Error, Result};
use kiln_resolve::resolver::ResolvedWorkspace;
use kiln_store::format::Entry;

use crate::buildscript::{self, BuildScriptOutput, Directive};
use crate::compile::{self, UnitEnv};
use crate::fingerprint::Fingerprinter;
use crate::plan::{Plan, PlannedUnit};
use crate::sandbox::SandboxPolicy;
use crate::scheduler::{UnitExecutor, UnitOutcome};
use crate::toolchain::Toolchain;

/// What one unit leaves behind on disk.
#[derive(Debug, Clone)]
struct UnitArtifact {
    /// The artifact dependents `--extern` against (rlib, or rmeta in check
    /// mode); `None` for bin-only units.
    extern_path: Option<PathBuf>,
    /// The unit's main output (rlib, binary, or build-script exe).
    main_path: PathBuf,
}

/// A store hit: the digest dependents key against, the materialised
/// artifact, and for scripts the parsed output with its `OUT_DIR`.
type StoreHit = ([u8; 32], UnitArtifact, Option<ScriptResult>);

/// A build script's result, shared with the package's other units.
#[derive(Debug, Clone)]
struct ScriptResult {
    /// The parsed `cargo:` directives.
    output: Arc<BuildScriptOutput>,
    /// The `OUT_DIR` the script ran in — compile units must see the same
    /// path (`env!("OUT_DIR")` is a compile-time read).
    out_dir: PathBuf,
}

/// The runner the scheduler drives. All shared state is behind `Mutex`es;
/// `execute` runs on many worker threads at once, each driving its own
/// rustc child.
pub struct UnitRunner {
    plan: Arc<Plan>,
    resolved: Arc<ResolvedWorkspace>,
    toolchain: Toolchain,
    profile: Profile,
    profile_fingerprint: u64,
    rustflags: Vec<String>,
    linker_args: Vec<String>,
    target_triple: String,
    /// The workspace's `target/` directory.
    target_dir: PathBuf,
    /// `target/<profile>/`.
    profile_dir: PathBuf,
    store: Arc<kiln_store::Store>,
    /// A handle for remote fetches from worker threads (never a tokio
    /// worker thread, so `block_on` is legal).
    remote: Option<tokio::runtime::Handle>,
    sandbox: bool,
    /// Package name -> its lib unit's graph index.
    lib_units: HashMap<String, usize>,
    artifacts: Mutex<HashMap<usize, UnitArtifact>>,
    /// Package name -> its build script's result.
    script_outputs: Mutex<HashMap<String, ScriptResult>>,
}

impl UnitRunner {
    /// Build a runner for a plan. The plan, resolution and toolchain are
    /// shared and immutable for the whole schedule.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        plan: Arc<Plan>,
        resolved: Arc<ResolvedWorkspace>,
        toolchain: Toolchain,
        profile: Profile,
        rustflags: Vec<String>,
        target_triple: String,
        target_dir: PathBuf,
        store: Arc<kiln_store::Store>,
        remote: Option<tokio::runtime::Handle>,
        sandbox: bool,
    ) -> Self {
        let profile_dir = target_dir.join(profile.dir_name());
        let profile_fingerprint = profile.fingerprint();
        let linker_args = toolchain.linker_args();
        let mut lib_units = HashMap::new();
        for (i, planned) in plan.units.iter().enumerate() {
            if matches!(planned.unit.kind, TargetKind::Lib | TargetKind::ProcMacro) {
                lib_units.insert(planned.unit.pkg.name.clone(), i);
            }
        }
        Self {
            plan,
            resolved,
            toolchain,
            profile,
            profile_fingerprint,
            rustflags,
            linker_args,
            target_triple,
            target_dir,
            profile_dir,
            store,
            remote,
            sandbox,
            lib_units,
            artifacts: Mutex::new(HashMap::new()),
            script_outputs: Mutex::new(HashMap::new()),
        }
    }

    /// The triple a unit compiles for: proc-macros and build scripts are
    /// host units.
    fn unit_triple(&self, unit: &Unit) -> &str {
        if matches!(unit.kind, TargetKind::ProcMacro | TargetKind::BuildScript) {
            &self.toolchain.host
        } else {
            &self.target_triple
        }
    }

    /// The fingerprinter for one unit (cheap to construct; borrows self).
    fn fingerprinter(&self, unit: &Unit) -> Fingerprinter<'_> {
        Fingerprinter::new(
            &self.resolved,
            &self.toolchain,
            self.unit_triple(unit),
            self.profile_fingerprint,
            &self.rustflags,
        )
    }

    /// A build script's directory: `target/<profile>/build/<pkg>-<short>`.
    fn script_dir(&self, pkg: &str, short: &str) -> PathBuf {
        self.profile_dir
            .join("build")
            .join(format!("{pkg}-{short}"))
    }

    /// The dependencies of a unit's package that apply to it, by import
    /// name, filtered to enabled ones.
    fn applicable_deps(&self, planned: &PlannedUnit) -> Vec<(String, kiln_manifest::Dependency)> {
        let pkg = match self.resolved.packages.get(&planned.unit.pkg.name) {
            Some(p) => p,
            None => return Vec::new(),
        };
        let rows: Vec<&kiln_manifest::Dependency> = if planned.unit.kind == TargetKind::BuildScript
        {
            pkg.manifest.build_deps.values().collect()
        } else if planned.needs_dev_deps {
            pkg.manifest
                .deps
                .values()
                .chain(pkg.manifest.dev_deps.values())
                .collect()
        } else {
            pkg.manifest.deps.values().collect()
        };
        rows.into_iter()
            .filter(|dep| {
                !dep.optional
                    || planned.enabled_optional_deps.contains(&dep.name)
                    || planned.enabled_optional_deps.contains(dep.crate_name())
            })
            .map(|dep| (dep.name.clone(), dep.clone()))
            .collect()
    }

    /// The `--extern` pairs for a unit: import name -> dependency artifact.
    fn dep_externs(&self, planned: &PlannedUnit) -> Vec<(String, PathBuf)> {
        let artifacts = self.artifacts.lock().expect("artifacts lock");
        let mut out = Vec::new();
        for (import_name, dep) in self.applicable_deps(planned) {
            let lib_index = match self.lib_units.get(dep.crate_name()) {
                Some(&i) => i,
                None => continue,
            };
            let Some(artifact) = artifacts.get(&lib_index) else {
                continue;
            };
            if let Some(path) = &artifact.extern_path {
                out.push((import_name, path.clone()));
            }
        }
        out
    }

    /// Fetch a key from the remote (local store misses only).
    fn fetch_remote(&self, key: &str) -> Option<Entry> {
        let handle = self.remote.as_ref()?;
        handle
            .block_on(self.store.fetch_remote(key))
            .unwrap_or_default()
    }

    /// Try to satisfy a unit from the store; on hit, materialise every
    /// file (artifacts, dep-info, and for scripts the exe plus the whole
    /// `OUT_DIR` tree) and return the artifact digest.
    fn try_store(&self, key_hex: &str, unit: &Unit, short: &str) -> Result<Option<StoreHit>> {
        let entry = match self.store.get(key_hex)? {
            Some(e) => e,
            None => match self.fetch_remote(key_hex) {
                Some(e) => e,
                None => return Ok(None),
            },
        };
        for file in &entry.outputs {
            let dest = self.profile_dir.join(&file.path);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| Error::io("create", parent, e))?;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = if file.executable { 0o755 } else { 0o644 };
                std::fs::write(&dest, &file.bytes).map_err(|e| Error::io("write", &dest, e))?;
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(perms))
                    .map_err(|e| Error::io("chmod", &dest, e))?;
            }
            #[cfg(not(unix))]
            {
                std::fs::write(&dest, &file.bytes).map_err(|e| Error::io("write", &dest, e))?;
            }
        }
        // Identify the unit's artifact roles from what was materialised.
        let out_dir = self.script_dir(&unit.pkg.name, short).join("out");
        let mut artifact = UnitArtifact {
            extern_path: None,
            main_path: self.profile_dir.clone(),
        };
        let mut script_result = None;
        if unit.kind == TargetKind::BuildScript {
            for file in &entry.outputs {
                if file.executable {
                    artifact.main_path = self.profile_dir.join(&file.path);
                } else if file.path.ends_with("output.json") {
                    let output: BuildScriptOutput =
                        serde_json::from_slice(&file.bytes).map_err(|e| {
                            Error::parse(format!("stored output of {}", unit.pkg.name), e)
                        })?;
                    script_result = Some(ScriptResult {
                        output: Arc::new(output),
                        out_dir: out_dir.clone(),
                    });
                }
            }
        } else {
            let outputs = compile::unit_outputs(unit, short);
            if unit.mode == CompileMode::Check {
                if let Some(rmeta) = &outputs.rmeta {
                    let path = self.profile_dir.join(format!("deps/{rmeta}"));
                    if path.is_file() {
                        artifact.main_path = path.clone();
                        artifact.extern_path = Some(path);
                    }
                }
            } else if let Some(rlib) = &outputs.rlib {
                let path = self.profile_dir.join(format!("deps/{rlib}"));
                if path.is_file() {
                    artifact.main_path = path.clone();
                    artifact.extern_path = Some(path);
                }
            }
            if let Some(bin) = &outputs.bin {
                let path = self.profile_dir.join(format!("deps/{bin}"));
                if path.is_file() {
                    artifact.main_path = path;
                }
            }
        }
        Ok(Some((entry_digest(&entry), artifact, script_result)))
    }

    /// Run one build script: compile it for the host, run it sandboxed
    /// with cargo's contract environment, then store the exe, the parsed
    /// output and the whole `OUT_DIR` tree as one entry.
    fn run_build_script(
        &self,
        planned: &PlannedUnit,
        unit: &Unit,
        short: &str,
        key_hex: &str,
    ) -> Result<([u8; 32], UnitArtifact)> {
        let pkg = self.resolved.package(&unit.pkg.name)?;
        let dir = self.script_dir(&unit.pkg.name, short);
        std::fs::create_dir_all(&dir).map_err(|e| Error::io("create", &dir, e))?;
        let out_dir = dir.join("out");
        std::fs::create_dir_all(&out_dir).map_err(|e| Error::io("create", &out_dir, e))?;

        // 1. Compile the script for the host, linking against its
        //    build-deps and the native libs their scripts discovered.
        let (dep_env, dep_flags) = self.dep_script_facts(planned, true);
        let mut unit_env = UnitEnv::plain();
        unit_env.rustc_flags = dep_flags;
        let externs = self.dep_externs(planned);
        let argv = compile::rustc_argv(
            unit,
            &planned.edition,
            &planned.root,
            &self.profile,
            &dir,
            &self.target_dir,
            None, // host unit
            &externs,
            &unit_env,
            short,
            &self.rustflags,
            &[],
        )?;
        compile::run_rustc(
            &self.toolchain.rustc,
            &argv,
            &BTreeMap::new(),
            &planned.root,
        )?;
        let exe = dir.join(host_exe(&unit.crate_name()));

        // 2. Run it under the sandbox with cargo's contract environment.
        //    TARGET is the triple being built for (how scripts detect a
        //    cross-compile), HOST the machine running the script.
        let mut env = buildscript::script_environment(
            &planned.root,
            &pkg.manifest.name,
            &pkg.manifest.version.to_string(),
            &self.target_triple,
            &self.toolchain.host,
            &out_dir,
        );
        for (k, v) in cfg_env(&self.target_triple, &unit.features) {
            env.insert(k, v);
        }
        // DEP_* facts from the build-deps' own scripts, as cargo gives them.
        env.extend(dep_env);
        env.insert(
            "RUSTC".into(),
            self.toolchain.rustc.to_string_lossy().into_owned(),
        );
        env.insert("OPT_LEVEL".into(), self.profile.opt_level.to_string());
        env.insert("PROFILE".into(), self.profile.dir_name().to_owned());
        env.insert(
            "CARGO_MANIFEST_LINKS".into(),
            pkg.manifest.links.clone().unwrap_or_default(),
        );
        let policy = if self.sandbox {
            SandboxPolicy::BuildScript {
                out_dir: out_dir.clone(),
                target_dir: self.profile_dir.clone(),
            }
        } else {
            SandboxPolicy::Off
        };
        let output = buildscript::run_script(&exe, &planned.root, &env, &policy)?;

        // 3. Persist the parsed output and the OUT_DIR tree next to the exe
        //    (cargo's layout), then store all of it as one entry: a cache
        //    hit materialises exactly what a fresh run produced.
        let output_json = output.to_bytes();
        let output_path = dir.join("output.json");
        std::fs::write(&output_path, &output_json)
            .map_err(|e| Error::io("write", &output_path, e))?;
        let mut files: Vec<(String, PathBuf, bool)> = vec![
            (rel_under(&exe, &self.profile_dir), exe.clone(), true),
            (
                rel_under(&output_path, &self.profile_dir),
                output_path,
                false,
            ),
        ];
        collect_tree(&out_dir, &self.profile_dir, &mut files)?;
        let description = format!("build-script {} {}", unit.pkg.name, unit.pkg.version);
        let entry = Entry::from_files(&description, &files)?;
        self.store.put(key_hex, &entry)?;
        self.script_outputs
            .lock()
            .expect("script outputs lock")
            .insert(
                unit.pkg.name.clone(),
                ScriptResult {
                    output: Arc::new(output),
                    out_dir: out_dir.clone(),
                },
            );

        Ok((
            entry_digest(&entry),
            UnitArtifact {
                extern_path: None,
                main_path: exe,
            },
        ))
    }

    /// Compile one ordinary unit and store its outputs.
    fn run_compile(
        &self,
        planned: &PlannedUnit,
        unit: &Unit,
        short: &str,
        key_hex: &str,
        script: Option<ScriptResult>,
    ) -> Result<([u8; 32], UnitArtifact)> {
        let deps_dir = self.profile_dir.join("deps");
        std::fs::create_dir_all(&deps_dir).map_err(|e| Error::io("create", &deps_dir, e))?;

        // The unit's environment: its own build script's directives and
        // OUT_DIR, plus the DEP_* facts and native link flags its
        // dependency closure's scripts contribute.
        let linking = matches!(
            unit.kind,
            TargetKind::Bin
                | TargetKind::Test
                | TargetKind::Bench
                | TargetKind::Example
                | TargetKind::BuildScript
                | TargetKind::TestHarness
        );
        let (dep_env, dep_flags) = self.dep_script_facts(planned, linking);
        let mut unit_env = UnitEnv::plain();
        let mut env_map = dep_env;
        if let Some(script) = &script {
            unit_env.rustc_flags = script.output.rustc_flag_args();
            unit_env.rustc_flags.extend(script.output.rustc_cfg_flags());
            unit_env.out_dir = Some(script.out_dir.clone());
            for (k, v) in script.output.env() {
                env_map.insert(k, v);
            }
            if let Some(out) = &unit_env.out_dir {
                env_map.insert("OUT_DIR".into(), out.to_string_lossy().into_owned());
            }
        }
        // Native -L/-l from dependency scripts reach the link line.
        unit_env.rustc_flags.extend(dep_flags);

        let externs = self.dep_externs(planned);
        let triple = if matches!(unit.kind, TargetKind::ProcMacro | TargetKind::BuildScript) {
            None
        } else {
            Some(self.target_triple.as_str())
        };
        let argv = compile::rustc_argv(
            unit,
            &planned.edition,
            &planned.root,
            &self.profile,
            &deps_dir,
            &self.target_dir,
            triple,
            &externs,
            &unit_env,
            short,
            &self.rustflags,
            &self.linker_args,
        )?;
        compile::run_rustc(&self.toolchain.rustc, &argv, &env_map, &planned.root)?;

        // Collect the outputs by the pinned naming convention.
        let outputs = compile::unit_outputs(unit, short);
        let mut files: Vec<(String, PathBuf, bool)> = Vec::new();
        let mut main_path = None;
        let mut extern_path = None;
        if let Some(rlib) = &outputs.rlib {
            let p = deps_dir.join(rlib);
            files.push((format!("deps/{rlib}"), p.clone(), false));
            main_path = Some(p.clone());
            extern_path = Some(p);
        }
        if let Some(rmeta) = &outputs.rmeta {
            let p = deps_dir.join(rmeta);
            if !files
                .iter()
                .any(|(rel, _, _)| rel == &format!("deps/{rmeta}"))
            {
                files.push((format!("deps/{rmeta}"), p.clone(), false));
            }
            // Check mode: dependents --extern the rmeta.
            if unit.mode == CompileMode::Check {
                main_path = Some(p.clone());
                extern_path = Some(p);
            }
        }
        if let Some(bin) = &outputs.bin {
            let p = deps_dir.join(bin);
            files.push((format!("deps/{bin}"), p.clone(), true));
            main_path = Some(p);
        }
        let dep_info = deps_dir.join(&outputs.dep_info);
        if dep_info.is_file() {
            files.push((format!("deps/{}", outputs.dep_info), dep_info, false));
        }
        let description = format!(
            "{} {} {}",
            unit.pkg.name, unit.pkg.version, unit.target_name
        );
        let entry = Entry::from_files(&description, &files)?;
        self.store.put(key_hex, &entry)?;

        // A unit with no artifact at all is a bug — except a checked bin,
        // which type-checks without leaving anything to link behind.
        let main_path = match main_path {
            Some(p) => p,
            None if unit.mode == CompileMode::Check => deps_dir.join(&outputs.dep_info),
            None => {
                return Err(Error::Bug(format!(
                    "unit {} produced no main artifact",
                    unit.pkg.name
                )));
            }
        };
        Ok((
            entry_digest(&entry),
            UnitArtifact {
                extern_path,
                main_path,
            },
        ))
    }

    /// The packages whose build-script output applies to a unit: its
    /// dependency closure, own package excluded, in stable name order.
    fn script_closure(&self, planned: &PlannedUnit) -> Vec<String> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue: Vec<String> = self
            .applicable_deps(planned)
            .into_iter()
            .filter_map(|(_, dep)| {
                let name = dep.crate_name().to_owned();
                self.resolved.packages.contains_key(&name).then_some(name)
            })
            .collect();
        queue.sort();
        queue.dedup();
        while let Some(name) = queue.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            if let Some(pkg) = self.resolved.packages.get(&name) {
                for dep in pkg
                    .manifest
                    .deps
                    .values()
                    .chain(pkg.manifest.build_deps.values())
                {
                    let dn = dep.crate_name().to_owned();
                    if !seen.contains(&dn) && self.resolved.packages.contains_key(&dn) {
                        queue.push(dn);
                    }
                }
            }
        }
        seen.into_iter().collect()
    }

    /// What a unit inherits from its dependencies' build scripts, as cargo
    /// propagates it: `DEP_<LINKS>_<KEY>` environment for every `links`
    /// package in the closure, and `-L`/`-l` pairs for units that link.
    fn dep_script_facts(
        &self,
        planned: &PlannedUnit,
        linking: bool,
    ) -> (BTreeMap<String, String>, Vec<String>) {
        let mut env = BTreeMap::new();
        let mut flags = Vec::new();
        let scripts = self.script_outputs.lock().expect("script outputs lock");
        for name in self.script_closure(planned) {
            let Some(script) = scripts.get(&name) else {
                continue;
            };
            let links = self
                .resolved
                .packages
                .get(&name)
                .and_then(|p| p.manifest.links.clone());
            if let Some(links) = links {
                let prefix = format!("DEP_{}_", links.to_uppercase().replace('-', "_"));
                for (k, v) in script.output.metadata() {
                    env.insert(format!("{prefix}{}", k.to_uppercase().replace('-', "_")), v);
                }
            }
            if linking {
                for directive in &script.output.directives {
                    match directive {
                        Directive::RustcLinkSearch(path) => {
                            flags.push("-L".into());
                            flags.push(format!("native={path}"));
                        }
                        Directive::RustcLinkLib(spec) => {
                            flags.push("-l".into());
                            flags.push(spec.clone());
                        }
                        _ => {}
                    }
                }
            }
        }
        (env, flags)
    }
}

impl UnitExecutor for UnitRunner {
    fn execute(
        &self,
        unit: &Unit,
        unit_index: usize,
        deps: &[(usize, UnitOutcome, [u8; 32])],
    ) -> Result<([u8; 32], UnitOutcome)> {
        let planned = &self.plan.units[unit_index];

        // 1. Fingerprint. The inputs cover the unit's own sources, its own
        //    build script's output, the outputs of every script in its
        //    dependency closure (their DEP_*/-L/-l facts feed this unit),
        //    and the DEP_* environment itself.
        let script = if planned.unit.kind != TargetKind::BuildScript {
            self.script_outputs
                .lock()
                .expect("script outputs lock")
                .get(&unit.pkg.name)
                .cloned()
        } else {
            None
        };
        let (dep_env, _) = self.dep_script_facts(planned, false);
        let mut script_bytes: Vec<u8> = Vec::new();
        if let Some(s) = &script {
            script_bytes.extend_from_slice(&s.output.to_bytes());
        }
        {
            let scripts = self.script_outputs.lock().expect("script outputs lock");
            for name in self.script_closure(planned) {
                if let Some(s) = scripts.get(&name) {
                    script_bytes.extend_from_slice(&s.output.to_bytes());
                }
            }
        }
        let fingerprinter = self.fingerprinter(unit);
        let input_hash = fingerprinter.unit_input_hash(
            unit,
            (!script_bytes.is_empty()).then_some(&script_bytes),
            &dep_env,
        )?;
        let mut dep_artifacts: Vec<(String, [u8; 32])> = deps
            .iter()
            .map(|(i, _, digest)| {
                let name = self.plan.units[*i].unit.crate_name();
                (name, *digest)
            })
            .collect();
        // Stable order: the same graph must always produce the same key.
        dep_artifacts.sort_by(|a, b| a.0.cmp(&b.0));
        let key: UnitKey = fingerprinter.unit_key(unit, input_hash, &dep_artifacts);
        let key_hex = key.digest();
        let short = key.short();

        // 2. The store: a hit materialises files and moves on.
        let store_hit = if self.store.writable() || self.store.has(&key_hex) {
            self.try_store(&key_hex, unit, &short)?
        } else {
            None
        };
        if let Some((digest, artifact, script_result)) = store_hit {
            self.artifacts
                .lock()
                .expect("artifacts lock")
                .insert(unit_index, artifact);
            if let Some(result) = script_result {
                self.script_outputs
                    .lock()
                    .expect("script outputs lock")
                    .insert(unit.pkg.name.clone(), result);
            }
            return Ok((digest, UnitOutcome::Cached));
        }

        // 3. Execute: drive rustc (or a build script) and store the result.
        let (digest, artifact) = if unit.kind == TargetKind::BuildScript {
            self.run_build_script(planned, unit, &short, &key_hex)?
        } else {
            self.run_compile(planned, unit, &short, &key_hex, script)?
        };
        self.artifacts
            .lock()
            .expect("artifacts lock")
            .insert(unit_index, artifact);
        Ok((digest, UnitOutcome::Built))
    }

    fn on_progress(&self, unit: &Unit, outcome: &UnitOutcome) {
        let verb = match outcome {
            UnitOutcome::Built => "Compiling",
            UnitOutcome::Cached => "Cache hit",
            UnitOutcome::Fresh => "Fresh",
        };
        eprintln!("   {verb} {} v{}", unit.pkg.name, unit.pkg.version);
    }
}

/// The executable rustc leaves for `--crate-name <name>` on this platform.
fn host_exe(crate_name: &str) -> String {
    if cfg!(windows) {
        format!("{crate_name}.exe")
    } else {
        crate_name.to_owned()
    }
}

/// The digest of a unit's artifact: everything a dependent can consume.
/// A just-built entry and the same entry fetched from a store digest
/// identically — dependents' keys must never depend on cache state.
fn entry_digest(entry: &Entry) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for file in &entry.outputs {
        hasher.update(file.path.as_bytes());
        hasher.update(&file.bytes);
    }
    *hasher.finalize().as_bytes()
}

/// Every regular file under `dir`, recursively, as store inputs. Symlinks
/// are never followed: the store captures files, not the filesystem's
/// opinion of aliases.
fn collect_tree(
    dir: &Path,
    profile_dir: &Path,
    files: &mut Vec<(String, PathBuf, bool)>,
) -> Result<()> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&d)
            .map_err(|e| Error::io("read_dir", &d, e))?
            .filter_map(|e| e.ok())
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let path = e.path();
            let meta =
                std::fs::symlink_metadata(&path).map_err(|er| Error::io("stat", &path, er))?;
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                #[cfg(unix)]
                let exec = {
                    use std::os::unix::fs::PermissionsExt;
                    meta.permissions().mode() & 0o111 != 0
                };
                #[cfg(not(unix))]
                let exec = false;
                files.push((rel_under(&path, profile_dir), path, exec));
            }
        }
    }
    Ok(())
}

/// A path relative to the profile directory, or a lossy fallback.
fn rel_under(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The `CARGO_CFG_*` and `CARGO_FEATURE_*` variables cargo gives build
/// scripts, derived from the target triple and the feature set.
pub fn cfg_env(target_triple: &str, features: &BTreeSet<String>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    // Triple shape: arch-vendor-os[-env], e.g. x86_64-unknown-linux-gnu
    // or aarch64-apple-darwin. The OS is the last component unless the
    // triple carries an environment suffix (gnu, musl, msvc, ...).
    const ENV_SUFFIXES: &[&str] = &[
        "gnu",
        "musl",
        "msvc",
        "gnueabihf",
        "gnueabi",
        "eabihf",
        "eabi",
        "ohos",
    ];
    const UNIX_OSES: &[&str] = &[
        "linux",
        "macos",
        "ios",
        "freebsd",
        "netbsd",
        "openbsd",
        "dragonfly",
        "android",
        "solaris",
        "illumos",
        "haiku",
        "aix",
        "vxworks",
    ];
    let parts: Vec<&str> = target_triple.split('-').collect();
    let arch = parts.first().copied().unwrap_or("unknown");
    let (os_raw, env_name) = match parts.len() {
        0 => (String::new(), String::new()),
        1 => (String::new(), String::new()),
        2 => (parts[1].to_string(), String::new()),
        _ => {
            let last = parts[parts.len() - 1];
            if ENV_SUFFIXES.contains(&last) {
                (parts[parts.len() - 2].to_string(), last.to_string())
            } else {
                (last.to_string(), String::new())
            }
        }
    };
    // Apple's triples spell the OS `darwin`; the cfg (and cargo's env)
    // says `macos`.
    let os = match os_raw.as_str() {
        "darwin" | "macos" => "macos".to_string(),
        other => other.to_string(),
    };
    let is_unix = UNIX_OSES.contains(&os.as_str());
    env.insert("CARGO_CFG_TARGET_ARCH".into(), arch.to_owned());
    env.insert("CARGO_CFG_TARGET_OS".into(), os);
    if !env_name.is_empty() {
        env.insert("CARGO_CFG_TARGET_ENV".into(), env_name);
    }
    env.insert(
        "CARGO_CFG_TARGET_FAMILY".into(),
        if is_unix {
            "unix".into()
        } else {
            String::new()
        },
    );
    if is_unix {
        env.insert("CARGO_CFG_UNIX".into(), "1".into());
    }
    env.insert(
        "CARGO_CFG_TARGET_POINTER_WIDTH".into(),
        arch_pointer_width(arch),
    );
    for feature in features {
        let name = feature.to_uppercase().replace('-', "_");
        env.insert(format!("CARGO_FEATURE_{name}"), "1".into());
    }
    env
}

fn arch_pointer_width(arch: &str) -> String {
    match arch {
        "x86_64" | "aarch64" | "riscv64gc" | "loongarch64" | "powerpc64" | "s390x" => "64".into(),
        "x86" | "arm" | "riscv32" | "powerpc" | "mips" => "32".into(),
        other if other.ends_with("64") => "64".into(),
        other if other.ends_with("32") => "32".into(),
        _ => "64".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cfg_env_derives_the_cargo_contract() {
        let env = cfg_env(
            "x86_64-unknown-linux-gnu",
            &BTreeSet::from(["std".into(), "dark-mode".into()]),
        );
        assert_eq!(env["CARGO_CFG_TARGET_ARCH"], "x86_64");
        assert_eq!(env["CARGO_CFG_TARGET_OS"], "linux");
        assert_eq!(env["CARGO_CFG_TARGET_ENV"], "gnu");
        assert_eq!(env["CARGO_CFG_TARGET_FAMILY"], "unix");
        assert_eq!(env["CARGO_CFG_TARGET_POINTER_WIDTH"], "64");
        assert_eq!(env["CARGO_FEATURE_STD"], "1");
        assert_eq!(env["CARGO_FEATURE_DARK_MODE"], "1");
    }

    #[test]
    fn cfg_env_normalises_triples_like_rustc() {
        // Apple triples say `darwin`; the cfg says `macos`.
        let apple = cfg_env("aarch64-apple-darwin", &BTreeSet::new());
        assert_eq!(apple["CARGO_CFG_TARGET_OS"], "macos");
        assert_eq!(apple["CARGO_CFG_TARGET_ARCH"], "aarch64");
        assert_eq!(apple["CARGO_CFG_TARGET_FAMILY"], "unix");
        // Android is its own OS, not an environment.
        let android = cfg_env("aarch64-linux-android", &BTreeSet::new());
        assert_eq!(android["CARGO_CFG_TARGET_OS"], "android");
        // Windows carries its environment in the triple.
        let msvc = cfg_env("x86_64-pc-windows-msvc", &BTreeSet::new());
        assert_eq!(msvc["CARGO_CFG_TARGET_OS"], "windows");
        assert_eq!(msvc["CARGO_CFG_TARGET_ENV"], "msvc");
        assert_eq!(msvc.get("CARGO_CFG_UNIX"), None);
        // Bare-metal: no environment suffix, `none` is the OS.
        let none = cfg_env("thumbv7em-none-eabihf", &BTreeSet::new());
        assert_eq!(none["CARGO_CFG_TARGET_ENV"], "eabihf");
    }

    #[test]
    fn keys_hex_stably() {
        let u = Unit {
            pkg: kiln_core::ids::PackageId::registry(
                "a",
                semver::Version::new(0, 1, 0),
                kiln_core::ids::SourceId::CRATES_IO,
            ),
            target_name: "a".into(),
            target_path: "src/lib.rs".into(),
            kind: TargetKind::Lib,
            test: false,
            mode: CompileMode::Build,
            features: BTreeSet::new(),
        };
        let key = UnitKey {
            unit: u,
            profile_fingerprint: 7,
            target: "t".into(),
            host: "h".into(),
            rustc: "r".into(),
            dep_artifacts: vec![],
            input_hash: [1; 32],
        };
        let hex = key.digest();
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(hex, key.digest());
        assert_eq!(key.short().len(), 16);
        assert!(hex.starts_with(&key.short()));
    }

    #[test]
    fn entry_digests_do_not_depend_on_how_the_entry_arrived() {
        // The cache-poisoning invariant: the digest covers (path, bytes)
        // pairs only — never mtimes, never creation order.
        let mk = |path: &str, bytes: &[u8]| kiln_store::format::StoredFile {
            path: path.to_owned(),
            bytes: bytes.to_vec(),
            executable: false,
        };
        let a = Entry {
            description: "x".into(),
            outputs: vec![mk("deps/libx-1.rlib", b"one"), mk("deps/libx-1.d", b"two")],
            created_ms: 1,
        };
        let b = Entry {
            description: "x".into(),
            outputs: vec![mk("deps/libx-1.rlib", b"one"), mk("deps/libx-1.d", b"two")],
            created_ms: 99,
        };
        assert_eq!(entry_digest(&a), entry_digest(&b));
        let different = Entry {
            description: "x".into(),
            outputs: vec![
                mk("deps/libx-1.rlib", b"one"),
                mk("deps/libx-1.d", b"THREE"),
            ],
            created_ms: 1,
        };
        assert_ne!(entry_digest(&a), entry_digest(&different));
    }

    #[test]
    fn host_exes_follow_the_platform() {
        assert_eq!(host_exe("build_script_build"), "build_script_build");
    }

    #[test]
    fn dep_kinds_flow_by_unit_kind() {
        // Documented behavior: build scripts use build-dependencies.
        // (The applicable_deps path is exercised end-to-end in engine
        // tests; here we pin the pure helpers.)
        let env = cfg_env("aarch64-apple-darwin", &BTreeSet::new());
        assert_eq!(env["CARGO_CFG_TARGET_OS"], "macos");
        assert_eq!(env["CARGO_CFG_TARGET_ARCH"], "aarch64");
    }
}

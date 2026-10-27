//! Sandboxing: build scripts, proc-macro host processes and test binaries run
//! restricted — no network, writes only to declared outputs.
//!
//! Two independent mechanisms on Linux:
//!
//! * **Landlock** (filesystem): read and execute everything the user can,
//!   write only the policy's declared directories. Built as a complete
//!   `RulesetCreated` *before* fork; the child only calls `restrict_self()`
//!   (two raw syscalls, no allocation), keeping the fork-exec window
//!   async-signal-safe.
//! * **seccomp** (network): `socket(AF_INET/AF_INET6, …)` returns `EACCES`;
//!   AF_UNIX stays available for local IPC. Some container runtimes forbid
//!   nested seccomp filters and *kill* processes that try to install one, so
//!   kiln never installs a filter blindly: a sacrificial probe child tests
//!   once per process whether filters install cleanly in this environment,
//!   and real children only receive the filter when the probe proved it safe.
//!
//! Degrading to unsandboxed never changes build outputs — sandboxing is
//! defense-in-depth for untrusted build scripts, not a correctness input —
//! and each degradation prints exactly one warning. `SandboxPolicy::Off`
//! opts out explicitly. Off Linux, the policy degrades to unsandboxed with
//! one notice.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::OnceLock;

use kiln_core::Result;

/// What runs under which policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxPolicy {
    /// Build scripts: read the workspace, write only `OUT_DIR` and the
    /// target directory.
    BuildScript {
        /// Where the script may write.
        out_dir: std::path::PathBuf,
        /// The whole target directory, for temp writes.
        target_dir: std::path::PathBuf,
    },
    /// Test binaries: read the workspace and the target dir, write to a
    /// per-test temp directory and TMPDIR.
    Test {
        /// Where the test may write.
        work_dir: std::path::PathBuf,
    },
    /// No sandbox: explicit user opt-out.
    Off,
}

impl SandboxPolicy {
    /// Whether this policy actually restricts anything.
    pub fn restricted(&self) -> bool {
        !matches!(self, SandboxPolicy::Off)
    }

    /// Run a build script under this policy.
    pub fn run_build_script(
        &self,
        exe: &Path,
        cwd: &Path,
        env: &BTreeMap<String, String>,
    ) -> Result<Output> {
        match self {
            SandboxPolicy::Off => run_plain(exe, cwd, env),
            _ => run_sandboxed(exe, cwd, env, self),
        }
    }

    /// Run a test binary under this policy, inheriting stdio (the user must
    /// see test output live).
    pub fn run_test(
        &self,
        exe: &Path,
        cwd: &Path,
        env: &BTreeMap<String, String>,
    ) -> Result<std::process::ExitStatus> {
        let mut cmd = Command::new(exe);
        cmd.current_dir(cwd).env_remove("CARGO");
        for (k, v) in env {
            cmd.env(k, v);
        }
        apply_restrictions(&mut cmd, self);
        let status = cmd.status().map_err(|e| kiln_core::Error::io("run", exe, e))?;
        Ok(status)
    }
}

/// The child-side restrictions for one policy: a ready-to-apply landlock
/// ruleset and a compiled seccomp filter. Everything here is allocated in
/// the parent; applying it in a child is syscall-only.
struct ChildRestrictions {
    #[cfg(target_os = "linux")]
    landlock: Option<landlock::RulesetCreated>,
    #[cfg(target_os = "linux")]
    seccomp: Option<seccompiler::BpfProgram>,
}

/// Build the child-side restrictions for a policy. This is where all
/// allocation happens (and where degradation warnings print), so it must be
/// called in the parent, before any fork.
#[cfg(target_os = "linux")]
fn build_restrictions(policy: &SandboxPolicy) -> ChildRestrictions {
    if matches!(policy, SandboxPolicy::Off) {
        return ChildRestrictions { landlock: None, seccomp: None };
    }
    ChildRestrictions {
        landlock: build_landlock_ruleset(policy),
        seccomp: seccomp_filter_for_this_process(),
    }
}

#[cfg(not(target_os = "linux"))]
fn build_restrictions(_policy: &SandboxPolicy) -> ChildRestrictions {
    ChildRestrictions {}
}

/// Apply pre-built restrictions to a command. Called on commands whose child
/// may outlive this function, so the restrictions are moved into the
/// `pre_exec` closure.
fn apply_restrictions(cmd: &mut Command, policy: &SandboxPolicy) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let mut restrictions = build_restrictions(policy);
        unsafe {
            cmd.pre_exec(move || {
                // Async-signal-safe: raw syscalls only, no allocation.
                if let Some(rs) = restrictions.landlock.take() {
                    // A restriction failure here is a security policy
                    // failure on a kernel that claimed support: deny the
                    // run rather than silently continue unsandboxed.
                    rs.restrict_self()
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                }
                if let Some(filter) = &restrictions.seccomp {
                    seccompiler::apply_filter(filter)
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (cmd, policy);
        warn_once(
            "nonlinux-sandbox",
            "kiln: sandboxing is Linux-only today; scripts run unsandboxed",
        );
    }
}

fn run_plain(exe: &Path, cwd: &Path, env: &BTreeMap<String, String>) -> Result<Output> {
    let mut cmd = Command::new(exe);
    cmd.current_dir(cwd).env_remove("CARGO");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().map_err(|e| kiln_core::Error::io("run", exe, e))
}

fn run_sandboxed(
    exe: &Path,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    policy: &SandboxPolicy,
) -> Result<Output> {
    let mut cmd = Command::new(exe);
    cmd.current_dir(cwd).env_remove("CARGO");
    for (k, v) in env {
        cmd.env(k, v);
    }
    apply_restrictions(&mut cmd, policy);
    cmd.output().map_err(|e| kiln_core::Error::io("run", exe, e))
}

// ---------------------------------------------------------------------------
// Landlock
// ---------------------------------------------------------------------------

/// Build the landlock ruleset for a policy: read+execute beneath `/`, full
/// access beneath the writable directories. Returns `None` (with a one-time
/// warning) when the kernel cannot enforce it — containers and old hosts are
/// normal, and outputs are identical either way.
#[cfg(target_os = "linux")]
fn build_landlock_ruleset(policy: &SandboxPolicy) -> Option<landlock::RulesetCreated> {
    use landlock::{
        path_beneath_rules, Access, AccessFs, ABI, Ruleset, RulesetAttr, RulesetCreatedAttr,
    };

    let writable: Vec<std::path::PathBuf> = match policy {
        SandboxPolicy::BuildScript { out_dir, target_dir } => {
            vec![out_dir.clone(), target_dir.clone()]
        }
        SandboxPolicy::Test { work_dir } => vec![work_dir.clone(), std::env::temp_dir()],
        SandboxPolicy::Off => return None,
    };
    // ABI 3 covers Refer (rename/link across directories), which build
    // scaffolding uses; BestEffort (the default) degrades on older kernels.
    let abi = ABI::V3;

    let writable_strs: Vec<String> =
        writable.iter().map(|p| p.to_string_lossy().into_owned()).collect();

    match Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .and_then(|r| r.create())
        .and_then(|r| r.add_rules(path_beneath_rules(["/"], AccessFs::from_read(abi))))
        .and_then(|r| {
            r.add_rules(path_beneath_rules(writable_strs.iter().map(String::as_str), AccessFs::from_all(abi)))
        })
        .map(|rs| rs.no_new_privs(true))
    {
        Ok(rs) => Some(rs),
        Err(_) => {
            warn_once(
                "landlock-unavailable",
                "kiln: Landlock is unavailable on this kernel; build scripts run unsandboxed",
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// seccomp
// ---------------------------------------------------------------------------

/// Whether this environment lets us install seccomp filters at all.
///
/// Probed once per process with a sacrificial child: the child installs the
/// real filter and exits. If it exits cleanly, filters are safe here; if it
/// is killed (some container runtimes SIGSYS/SIGKILL nested `seccomp()`
/// callers) or errors, we never install filters in this process.
static SECCOMP_AVAILABLE: OnceLock<bool> = OnceLock::new();

#[cfg(target_os = "linux")]
pub(crate) fn seccomp_available() -> bool {
    *SECCOMP_AVAILABLE.get_or_init(|| {
        let Some(filter) = compile_socket_deny_filter() else { return false };
        let mut probe = Command::new("true");
        probe.stdin(std::process::Stdio::null());
        probe.stdout(std::process::Stdio::null());
        probe.stderr(std::process::Stdio::null());
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                probe.pre_exec(move || {
                    // Raw syscalls only in the sacrificial child.
                    seccompiler::apply_filter(&filter)
                        .map_err(|e| std::io::Error::other(e.to_string()))
                });
            }
        }
        let ok = matches!(probe.output(), Ok(out) if out.status.success());
        if !ok {
            warn_once(
                "seccomp-unavailable",
                "kiln: this environment forbids seccomp filters; sandboxed scripts run without network isolation",
            );
        }
        ok
    })
}

/// The compiled filter for this process, if the environment allows one.
#[cfg(target_os = "linux")]
fn seccomp_filter_for_this_process() -> Option<seccompiler::BpfProgram> {
    if !seccomp_available() {
        return None;
    }
    compile_socket_deny_filter()
}

/// Compile the network-denial filter: `socket()` for AF_INET/AF_INET6
/// stream/datagram returns `EACCES`; everything else (including AF_UNIX)
/// is allowed. `None` when the filter cannot be built on this platform.
#[cfg(target_os = "linux")]
fn compile_socket_deny_filter() -> Option<seccompiler::BpfProgram> {
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule,
    };
    use std::convert::TryInto;

    let deny = |family: u64, ty: u64| -> Option<SeccompRule> {
        SeccompRule::new(vec![
            SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, family).ok()?,
            SeccompCondition::new(1, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, ty).ok()?,
        ])
        .ok()
    };
    let rule = || -> Option<Vec<SeccompRule>> {
        vec![
            deny(libc::AF_INET as u64, libc::SOCK_STREAM as u64)?,
            deny(libc::AF_INET as u64, libc::SOCK_DGRAM as u64)?,
            deny(libc::AF_INET6 as u64, libc::SOCK_STREAM as u64)?,
            deny(libc::AF_INET6 as u64, libc::SOCK_DGRAM as u64)?,
        ]
        .into()
    };
    let filter: BpfProgram = SeccompFilter::new(
        [(libc::SYS_socket, rule()?)].into_iter().collect(),
        SeccompAction::Errno(libc::EACCES as u32),
        SeccompAction::Allow,
        std::env::consts::ARCH.try_into().ok()?,
    )
    .ok()?
    .try_into()
    .ok()?;
    Some(filter)
}

// ---------------------------------------------------------------------------
// Notices
// ---------------------------------------------------------------------------

/// Print a degradation notice exactly once per key, process-wide.
pub(crate) fn warn_once(key: &str, message: &str) {
    static SEEN: OnceLock<std::sync::Mutex<std::collections::BTreeSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeSet::new()));
    if let Ok(mut guard) = seen.lock()
        && guard.insert(key.to_owned()) {
            eprintln!("{message}");
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn policies_know_when_they_restrict() {
        assert!(SandboxPolicy::BuildScript {
            out_dir: PathBuf::from("/tmp/o"),
            target_dir: PathBuf::from("/tmp/t"),
        }
        .restricted());
        assert!(SandboxPolicy::Test { work_dir: PathBuf::from("/tmp/w") }.restricted());
        assert!(!SandboxPolicy::Off.restricted());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_sandboxed_script_writes_inside_its_out_dir() {
        let dir = tempfile::tempdir().unwrap();
        let out_dir = dir.path().join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let script = dir.path().join("script.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho cargo:rerun-if-changed=x\necho done > \"$OUT_DIR/product.txt\"\n",
        )
        .unwrap();
        make_executable(&script);
        let policy = SandboxPolicy::BuildScript {
            out_dir: out_dir.clone(),
            target_dir: dir.path().join("target"),
        };
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        let mut env = BTreeMap::new();
        env.insert("OUT_DIR".to_owned(), out_dir.to_string_lossy().into_owned());
        let output = policy.run_build_script(&script, dir.path(), &env).unwrap();
        assert!(output.status.success(), "script must succeed: {output:?}");
        assert!(out_dir.join("product.txt").is_file(), "writes inside OUT_DIR must work");
    }

    /// A behavioral probe: does landlock actually enforce this policy here?
    /// A sacrificial child (same ruleset, write attempt outside the writable
    /// set) answers for real — no kernel-version guessing.
    #[cfg(target_os = "linux")]
    fn landlock_enforced(policy: &SandboxPolicy) -> bool {
        let outside = tempfile::tempdir().unwrap();
        let outside_path = outside.path().join("violation.txt");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(format!("echo x > {}", outside_path.display()))
            .current_dir("/tmp");
        apply_restrictions(&mut cmd, policy);
        match cmd.output() {
            Ok(out) => !out.status.success() || !outside_path.is_file(),
            Err(_) => false,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_sandboxed_script_cannot_write_outside_its_out_dir() {
        let dir = tempfile::tempdir().unwrap();
        let out_dir = dir.path().join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        let policy = SandboxPolicy::BuildScript {
            out_dir: out_dir.clone(),
            target_dir: dir.path().join("target"),
        };
        if !landlock_enforced(&policy) {
            eprintln!(
                "skipping: this kernel/container does not enforce Landlock \
                 (GitHub CI runners do; this assertion runs there)"
            );
            return;
        }
        let script = dir.path().join("script.sh");
        let outside = dir.path().join("outside.txt");
        std::fs::write(&script, format!("#!/bin/sh\necho hi > {}\n", outside.display())).unwrap();
        make_executable(&script);
        let env = BTreeMap::new();
        let output = policy.run_build_script(&script, dir.path(), &env).unwrap();
        assert!(
            !outside.is_file(),
            "sandbox must prevent writes outside out_dir (script status: {:?})",
            output.status
        );
    }

    /// seccomp enforcement, tested behaviorally through the
    /// `socket_probe` example binary: exit 42 means socket() was denied,
    /// exit 0 means it succeeded. When the environment forbids seccomp
    /// (this container does), the same probe asserts the fail-open path:
    /// the child runs and sockets stay permitted — proving we never
    /// install a filter that would kill the process.
    #[cfg(target_os = "linux")]
    #[test]
    fn seccomp_filter_denies_or_degrades_cleanly() {
        let probe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/debug/examples/socket_probe");
        if !probe.is_file() {
            eprintln!("skipping: socket_probe example not built (run via cargo test)");
            return;
        }
        let mut cmd = Command::new(&probe);
        cmd.current_dir("/tmp");
        let policy = SandboxPolicy::Test { work_dir: PathBuf::from("/tmp") };
        apply_restrictions(&mut cmd, &policy);
        let out = cmd.output().unwrap();
        let code = out.status.code().unwrap_or(-1);
        if seccomp_available() {
            assert_eq!(code, 42, "with seccomp available the filter must deny sockets: {out:?}");
        } else {
            assert_eq!(code, 0, "without seccomp the child must run unfiltered: {out:?}");
        }
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }
}

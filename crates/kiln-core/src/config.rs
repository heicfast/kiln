//! Kiln's configuration: environment variables, `.kiln.toml`, and defaults.
//!
//! Precedence, lowest to highest: built-in defaults, `.kiln.toml` at the
//! workspace root, the `KILN_*` environment. Every knob is a named thing a
//! user can spell in an issue; nothing is configured by folklore.

use std::path::{Path, PathBuf};

use crate::error::Result;

/// How kiln treats its content-addressed store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
    /// Read and write the local store, and sync to the remote if one is set.
    #[default]
    ReadWrite,
    /// Only read: a CI job that must not poison a shared cache.
    ReadOnly,
    /// No store at all: every unit compiles.
    Off,
}

/// The sandbox policy for code kiln runs on the user's behalf.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    /// Build scripts, proc-macros and tests run restricted: no network, and
    /// writes only to their declared outputs. Linux today (Landlock +
    /// seccomp); elsewhere it is an explicit error to ask for it.
    #[default]
    On,
    /// No sandbox. Opt-in, printed on every invocation that uses it.
    Off,
}

/// How the linker for final artifacts is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkerPolicy {
    /// Use the fastest linker installed (`wild`, `mold`, `lld`, in that
    /// order), falling back to the platform default with a note when none of
    /// them is present. Linker choice is fingerprinted, so switching linkers
    /// switches cache keys, never silently reuses an entry.
    #[default]
    Auto,
    /// Use the compiler's default linker.
    Default,
}

/// The effective configuration for one invocation.
#[derive(Debug, Clone)]
pub struct KilnConfig {
    /// The store directory: `~/.cache/kiln/store` by default.
    pub cache_dir: PathBuf,
    /// The remote cache, if any: `http://host:port`, or a `file:///path`.
    pub remote: Option<String>,
    /// An bearer token for the remote, sent as `Authorization: Bearer`.
    pub remote_token: Option<String>,
    /// Cache behaviour.
    pub cache_mode: CacheMode,
    /// Maximum concurrent compile jobs; `None` = logical CPUs.
    pub jobs: Option<usize>,
    /// Sandbox policy for build scripts, proc-macros and tests.
    pub sandbox: SandboxMode,
    /// Linker policy for final artifacts.
    pub linker: LinkerPolicy,
    /// How long the daemon idles before exiting (seconds).
    pub daemon_idle_secs: u64,
}

impl Default for KilnConfig {
    fn default() -> Self {
        Self {
            cache_dir: default_cache_dir(),
            remote: None,
            remote_token: None,
            cache_mode: CacheMode::ReadWrite,
            jobs: None,
            sandbox: SandboxMode::default(),
            linker: LinkerPolicy::default(),
            daemon_idle_secs: 30 * 60,
        }
    }
}

impl KilnConfig {
    /// Load configuration for a workspace rooted at `root`.
    ///
    /// `.kiln.toml` is optional; when present only its known keys are read
    /// and unknown keys are errors, for the same reason as profiles: a knob
    /// kiln does not model is a knob that silently does nothing.
    pub fn load(root: &Path) -> Result<Self> {
        let mut cfg = Self::default();
        let file = root.join(".kiln.toml");
        if file.exists() {
            let text = std::fs::read_to_string(&file)
                .map_err(|e| crate::Error::io("read", &file, e))?;
            let table: toml::Table = toml::from_str(&text)
                .map_err(|e| crate::Error::parse(".kiln.toml", e))?;
            for (key, value) in &table {
                match key.as_str() {
                    "remote" => cfg.remote = value.as_str().map(str::to_owned),
                    "remote-token" => cfg.remote_token = value.as_str().map(str::to_owned),
                    "cache-dir" => {
                        cfg.cache_dir = PathBuf::from(
                            value.as_str().ok_or_else(|| {
                                crate::Error::parse(".kiln.toml", "cache-dir must be a string")
                            })?,
                        )
                    }
                    "jobs" => {
                        cfg.jobs = value.as_integer().and_then(|i| usize::try_from(i).ok());
                    }
                    "sandbox" => {
                        cfg.sandbox = match value.as_str() {
                            Some("on") => SandboxMode::On,
                            Some("off") => SandboxMode::Off,
                            other => {
                                return Err(crate::Error::parse(
                                    ".kiln.toml",
                                    format!("sandbox must be \"on\" or \"off\", got {other:?}"),
                                ))
                            }
                        }
                    }
                    "cache" => {
                        cfg.cache_mode = match value.as_str() {
                            Some("read-write") => CacheMode::ReadWrite,
                            Some("read-only") => CacheMode::ReadOnly,
                            Some("off") => CacheMode::Off,
                            other => {
                                return Err(crate::Error::parse(
                                    ".kiln.toml",
                                    format!("cache must be read-write, read-only or off, got {other:?}"),
                                ))
                            }
                        }
                    }
                    "linker" => {
                        cfg.linker = match value.as_str() {
                            Some("auto") => LinkerPolicy::Auto,
                            Some("default") => LinkerPolicy::Default,
                            other => {
                                return Err(crate::Error::parse(
                                    ".kiln.toml",
                                    format!("linker must be \"auto\" or \"default\", got {other:?}"),
                                ))
                            }
                        }
                    }
                    other => {
                        return Err(crate::Error::unsupported(format!(
                            ".kiln.toml: unknown key `{other}`"
                        )))
                    }
                }
            }
        }
        cfg.apply_env();
        Ok(cfg)
    }

    /// Apply `KILN_*` environment overrides.
    pub fn apply_env(&mut self) {
        if let Ok(dir) = std::env::var("KILN_CACHE") {
            if dir == "off" {
                self.cache_mode = CacheMode::Off;
            } else {
                self.cache_dir = PathBuf::from(dir);
            }
        }
        if let Ok(url) = std::env::var("KILN_REMOTE") {
            if url != "off" {
                self.remote = Some(url);
            } else {
                self.remote = None;
            }
        }
        if let Ok(token) = std::env::var("KILN_REMOTE_TOKEN") {
            self.remote_token = Some(token);
        }
        if let Ok(v) = std::env::var("KILN_CACHE_MODE") {
            self.cache_mode = match v.as_str() {
                "read-write" => CacheMode::ReadWrite,
                "read-only" => CacheMode::ReadOnly,
                _ => CacheMode::Off,
            };
        }
        if let Ok(v) = std::env::var("KILN_JOBS")
            && let Ok(n) = v.parse::<usize>() {
                self.jobs = Some(n);
            }
        if let Ok(v) = std::env::var("KILN_SANDBOX") {
            self.sandbox = if v == "off" { SandboxMode::Off } else { SandboxMode::On };
        }
        if let Ok(v) = std::env::var("KILN_LINKER") {
            self.linker = if v == "default" { LinkerPolicy::Default } else { LinkerPolicy::Auto };
        }
    }

    /// The number of parallel jobs to run.
    pub fn jobs(&self) -> usize {
        self.jobs.unwrap_or_else(|| {
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
        })
    }
}

/// The default store directory, honouring `XDG_CACHE_HOME` on every platform.
pub fn default_cache_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME").ok().filter(|s| !s.is_empty()).map(|h| PathBuf::from(h).join(".cache"))
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("kiln").join("store")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn defaults_are_sane() {
        let cfg = KilnConfig::default();
        assert_eq!(cfg.cache_mode, CacheMode::ReadWrite);
        assert!(cfg.jobs() >= 1);
        assert_eq!(cfg.sandbox, SandboxMode::On);
        assert!(cfg.daemon_idle_secs > 0);
    }

    #[test]
    fn kiln_toml_is_parsed_and_unknown_keys_refused() {
        let dir = std::env::temp_dir().join(format!("kiln-cfg-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".kiln.toml"), "remote = \"http://cache.internal:7381\"\njobs = 8\n").unwrap();
        let cfg = KilnConfig::load(&dir).unwrap();
        assert_eq!(cfg.remote.as_deref(), Some("http://cache.internal:7381"));
        assert_eq!(cfg.jobs, Some(8));

        fs::write(dir.join(".kiln.toml"), "mystery = true\n").unwrap();
        let err = KilnConfig::load(&dir).unwrap_err();
        assert!(err.to_string().contains("unknown key"), "{err}");
        fs::remove_dir_all(&dir).ok();
    }
}

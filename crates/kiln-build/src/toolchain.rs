//! Toolchain discovery: which rustc, which linker, and their exact versions.

use std::path::PathBuf;
use std::process::Command;

use kiln_core::{Error, Result};

/// The linker chosen for final artifacts, in kiln's preference order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkerChoice {
    /// The fastest installed: `wild`, then `mold`, then `lld`.
    Fastest(String),
    /// The compiler's platform default.
    Default,
}

/// A discovered toolchain.
#[derive(Debug, Clone)]
pub struct Toolchain {
    /// The rustc executable.
    pub rustc: PathBuf,
    /// `rustc --version` output, verbatim (version and commit hash).
    pub rustc_version: String,
    /// The host triple.
    pub host: String,
    /// The linker for final artifacts, when a fast one is installed and the
    /// policy allows it.
    pub linker: LinkerChoice,
}

impl Toolchain {
    /// Discover the toolchain: `RUSTC` if set, else `rustc` on PATH, else
    /// rustup's. The linker policy decides between the fastest installed
    /// linker and the platform default.
    pub fn discover(auto_linker: bool) -> Result<Self> {
        let rustc = find_rustc()?;
        let vv = Command::new(&rustc)
            .arg("-vV")
            .output()
            .map_err(|e| Error::io("run", &rustc, e))?;
        if !vv.status.success() {
            return Err(Error::CommandFailed {
                command: format!("{} -vV", rustc.display()),
                status: format!("{}", vv.status),
            });
        }
        let text = String::from_utf8_lossy(&vv.stdout);
        let mut rustc_version = String::new();
        let mut host = String::new();
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("rustc ") {
                rustc_version = v.trim().to_owned();
            }
            if let Some(h) = line.strip_prefix("host: ") {
                host = h.trim().to_owned();
            }
        }
        if rustc_version.is_empty() || host.is_empty() {
            return Err(Error::parse(
                "rustc -vV",
                format!("unexpected output: {text}"),
            ));
        }
        let linker = if auto_linker {
            find_fastest_linker()
        } else {
            LinkerChoice::Default
        };
        Ok(Self { rustc, rustc_version, host, linker })
    }

    /// The `-Clink-arg` prefixes that select the chosen linker, if any.
    pub fn linker_args(&self) -> Vec<String> {
        match &self.linker {
            LinkerChoice::Fastest(name) => vec![
                format!("-Clink-arg=-fuse-ld={name}"),
            ],
            LinkerChoice::Default => Vec::new(),
        }
    }
}

fn find_rustc() -> Result<PathBuf> {
    if let Ok(rustc) = std::env::var("RUSTC")
        && !rustc.is_empty() {
            return Ok(PathBuf::from(rustc));
        }
    if let Some(home) = std::env::var_os("HOME") {
        let via_rustup = PathBuf::from(home).join(".cargo/bin/rustc");
        if via_rustup.is_file() {
            return Ok(via_rustup);
        }
    }
    let found = which("rustc").ok_or_else(|| {
        Error::missing("rustc", "no Rust compiler found: install one with rustup (https://rustup.rs) or set RUSTC")
    })?;
    Ok(found)
}

/// Which of the fast linkers is installed, in preference order. `mold`
/// installs `-fuse-ld=mold`-compatible wrappers; `wild` and `lld` likewise
/// answer to `-fuse-ld=` via their shims.
fn find_fastest_linker() -> LinkerChoice {
    for name in ["wild", "mold", "lld"] {
        if which(&format!("{name}-ld")).is_some() || which(name).is_some() {
            return LinkerChoice::Fastest(name.to_owned());
        }
    }
    LinkerChoice::Default
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_toolchain_is_discovered() {
        // The CI and dev environments always have rustc; this test documents
        // the discovery contract on a machine that has one.
        let tc = Toolchain::discover(false).unwrap();
        assert!(tc.rustc.is_file() || tc.rustc.exists());
        assert!(!tc.rustc_version.is_empty());
        assert!(tc.host.contains('-'), "host triple: {}", tc.host);
        assert_eq!(tc.linker, LinkerChoice::Default);
    }

    #[test]
    fn linker_args_follow_the_choice() {
        let tc = Toolchain { rustc: PathBuf::from("/bin/rustc"), rustc_version: "1.0".into(), host: "x".into(), linker: LinkerChoice::Fastest("mold".into()) };
        assert_eq!(tc.linker_args(), vec!["-Clink-arg=-fuse-ld=mold"]);
        let tc = Toolchain { rustc: PathBuf::from("/bin/rustc"), rustc_version: "1.0".into(), host: "x".into(), linker: LinkerChoice::Default };
        assert!(tc.linker_args().is_empty());
    }
}

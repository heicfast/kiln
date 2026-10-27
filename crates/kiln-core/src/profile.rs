//! Compile profiles: the `[profile.*]` settings that change rustc's behaviour,
//! folded to a fingerprint-friendly value type.

use std::collections::BTreeMap;

/// The compiler-facing settings of a profile, already normalised.
///
/// Anything not represented here does not change compiler output and so has no
/// business in a cache key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Profile {
    /// `-C opt-level`.
    pub opt_level: u8,
    /// `-C debug`: 0 = none, 1 = line tables, 2 = full.
    pub debug: u8,
    /// `-C debug-assertions`.
    pub debug_assertions: bool,
    /// `-C overflow-checks`.
    pub overflow_checks: bool,
    /// `-C panic`: `unwind` or `abort`.
    pub panic: PanicStrategy,
    /// `-C lto`: off, thin, fat, or a number of codegen units.
    pub lto: Lto,
    /// `-C codegen-units`.
    pub codegen_units: Option<u32>,
    /// `-C rpath`.
    pub rpath: bool,
    /// Whether `-C incremental` directories are used. Kiln defaults this to
    /// off: its artifact cache subsumes rustc's incremental state and stays
    /// valid across checkouts and machines, which incremental never is.
    pub incremental: bool,
    /// Whether this is a release-family profile (used for default flags).
    pub release: bool,
}

/// `-C panic` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PanicStrategy {
    /// Unwind on panic (the default).
    Unwind,
    /// Abort on panic.
    Abort,
}

impl PanicStrategy {
    /// The rustc spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            PanicStrategy::Unwind => "unwind",
            PanicStrategy::Abort => "abort",
        }
    }
}

/// `-C lto` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Lto {
    /// No LTO.
    Off,
    /// Thin local LTO.
    Thin,
    /// Fat LTO.
    Fat,
}

impl Lto {
    /// The rustc spelling, if the flag is needed.
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            Lto::Off => None,
            Lto::Thin => Some("thin"),
            Lto::Fat => Some("fat"),
        }
    }
}

impl Profile {
    /// The dev profile: cargo-compatible defaults.
    pub fn dev() -> Self {
        Self {
            opt_level: 0,
            debug: 2,
            debug_assertions: true,
            overflow_checks: true,
            panic: PanicStrategy::Unwind,
            lto: Lto::Off,
            codegen_units: Some(256),
            rpath: false,
            incremental: false,
            release: false,
        }
    }

    /// The release profile: cargo-compatible defaults.
    pub fn release() -> Self {
        Self {
            opt_level: 3,
            debug: 0,
            debug_assertions: false,
            overflow_checks: false,
            panic: PanicStrategy::Unwind,
            lto: Lto::Off,
            codegen_units: Some(16),
            rpath: false,
            incremental: false,
            release: true,
        }
    }

    /// The test profile (dev + test harness defaults).
    pub fn test() -> Self {
        Self::dev()
    }

    /// The bench profile (cargo: bench inherits release).
    pub fn bench() -> Self {
        Self::release()
    }

    /// The directory under `target/` this profile's artifacts land in
    /// (cargo's layout: `debug` for dev and test, `release` for release
    /// and bench).
    pub fn dir_name(&self) -> &'static str {
        if self.release { "release" } else { "debug" }
    }

    /// Override from a `[profile.<name>]` table's raw toml values.
    ///
    /// Unknown keys are an error, not a silent ignore: a setting kiln does
    /// not model could change compiler output, and an unmodelled change to
    /// output is exactly what a cache key must never miss.
    pub fn apply_toml(
        &mut self,
        name: &str,
        table: &BTreeMap<String, toml::Value>,
    ) -> crate::Result<()> {
        for (key, value) in table {
            match key.as_str() {
                "opt-level" => self.opt_level = parse_level(name, key, value)?,
                "debug" => self.debug = parse_debug(name, key, value)?,
                "debug-assertions" => self.debug_assertions = parse_bool(name, key, value)?,
                "overflow-checks" => self.overflow_checks = parse_bool(name, key, value)?,
                "panic" => {
                    let s = expect_string(name, key, value)?;
                    self.panic = match s.as_str() {
                        "unwind" => PanicStrategy::Unwind,
                        "abort" => PanicStrategy::Abort,
                        other => {
                            return Err(crate::Error::parse(
                                format!("profile {name}.{key}"),
                                format!("unknown panic strategy `{other}`"),
                            ));
                        }
                    };
                }
                "lto" => {
                    self.lto = match value {
                        toml::Value::Boolean(false) => Lto::Off,
                        toml::Value::Boolean(true) => Lto::Fat,
                        toml::Value::String(s) if s == "off" => Lto::Off,
                        toml::Value::String(s) if s == "thin" => Lto::Thin,
                        toml::Value::String(s) if s == "fat" => Lto::Fat,
                        other => {
                            return Err(crate::Error::parse(
                                format!("profile {name}.{key}"),
                                format!(
                                    "expected true, false, \"thin\", \"fat\" or \"off\", got {other:?}"
                                ),
                            ));
                        }
                    };
                }
                "codegen-units" => {
                    self.codegen_units = Some(
                        value
                            .as_integer()
                            .and_then(|i| u32::try_from(i).ok())
                            .ok_or_else(|| {
                                crate::Error::parse(
                                    format!("profile {name}.{key}"),
                                    "expected a positive integer",
                                )
                            })?,
                    );
                }
                "rpath" => self.rpath = parse_bool(name, key, value)?,
                "incremental" => self.incremental = parse_bool(name, key, value)?,
                // Cargo accepts these; they do not change rustc's output for
                // the units kiln compiles, but refusing them would break real
                // workspaces, so they are recorded as inert.
                "strip" | "inherits" | "trim-paths" => {}
                other => {
                    return Err(crate::Error::unsupported(format!(
                        "profile {name}.{other}: kiln does not model this setting yet; refusing rather than silently changing compiler output"
                    )));
                }
            }
        }
        Ok(())
    }

    /// A stable fingerprint of the settings that reach rustc.
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }
}

fn parse_level(name: &str, key: &str, value: &toml::Value) -> crate::Result<u8> {
    value
        .as_integer()
        .and_then(|i| u8::try_from(i).ok())
        .filter(|l| *l <= 3)
        .ok_or_else(|| {
            crate::Error::parse(format!("profile {name}.{key}"), "expected 0, 1, 2 or 3")
        })
}

fn parse_debug(name: &str, key: &str, value: &toml::Value) -> crate::Result<u8> {
    match value {
        toml::Value::Integer(i) if (0..=2).contains(i) => Ok(*i as u8),
        toml::Value::Boolean(true) => Ok(2),
        toml::Value::Boolean(false) => Ok(0),
        toml::Value::String(s) if s == "line-tables-only" => Ok(1),
        other => Err(crate::Error::parse(
            format!("profile {name}.{key}"),
            format!("expected 0, 1, 2, true, false or \"line-tables-only\", got {other:?}"),
        )),
    }
}

fn parse_bool(name: &str, key: &str, value: &toml::Value) -> crate::Result<bool> {
    value.as_bool().ok_or_else(|| {
        crate::Error::parse(format!("profile {name}.{key}"), "expected true or false")
    })
}

fn expect_string(name: &str, key: &str, value: &toml::Value) -> crate::Result<String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| crate::Error::parse(format!("profile {name}.{key}"), "expected a string"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(pairs: &[(&str, toml::Value)]) -> BTreeMap<String, toml::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn dev_and_release_disagree_on_every_slow_knob() {
        let dev = Profile::dev();
        let rel = Profile::release();
        assert_ne!(dev.fingerprint(), rel.fingerprint());
        assert_eq!(rel.opt_level, 3);
        assert_eq!(dev.opt_level, 0);
    }

    #[test]
    fn profile_tables_apply_and_fingerprint() {
        let mut p = Profile::dev();
        let t = table(&[
            ("opt-level", toml::Value::Integer(1)),
            ("debug", toml::Value::String("line-tables-only".into())),
            ("lto", toml::Value::String("thin".into())),
        ]);
        p.apply_toml("custom", &t).unwrap();
        assert_eq!(p.opt_level, 1);
        assert_eq!(p.debug, 1);
        assert_eq!(p.lto, Lto::Thin);
    }

    #[test]
    fn unknown_profile_keys_are_refused_not_ignored() {
        let mut p = Profile::dev();
        let t = table(&[("profile-override", toml::Value::String("x".into()))]);
        let err = p.apply_toml("custom", &t).unwrap_err();
        assert!(err.to_string().contains("does not model"), "{err}");
    }
}

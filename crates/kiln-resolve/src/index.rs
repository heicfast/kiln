//! The crates.io sparse index client, with a disk cache.
//!
//! The sparse protocol is one HTTPS GET per crate: the response is a list of
//! JSON objects, one per published version, newest last. Kiln caches each
//! response on disk so an `--offline` build (or a second build) never pays
//! for it again.

use std::path::PathBuf;

use kiln_core::{Error, Result};
use semver::Version;
use serde::{Deserialize, Serialize};

/// One published version of one crate, as the index spells it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    /// The crate name.
    pub name: String,
    /// This version.
    pub vers: Version,
    /// Its dependencies.
    #[serde(default)]
    pub deps: Vec<IndexDependency>,
    /// Its features.
    #[serde(default)]
    pub features: std::collections::BTreeMap<String, Vec<String>>,
    /// The 0.2-era feature additions (`features2`), merged on read.
    #[serde(default)]
    pub features2: std::collections::BTreeMap<String, Vec<String>>,
    /// Whether the version was yanked.
    #[serde(default)]
    pub yanked: bool,
    /// The sha256 of its `.crate` file.
    #[serde(default)]
    pub checksum: Option<String>,
    /// The `links = "..."` value, for build scripts.
    #[serde(default)]
    pub links: Option<String>,
}

/// One dependency row inside an index entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexDependency {
    /// The dependency's name.
    pub name: String,
    /// Its version requirement.
    pub req: String,
    /// The features this publication turns on.
    #[serde(default)]
    pub features: Vec<String>,
    /// Whether it is optional.
    #[serde(default)]
    pub optional: bool,
    /// `normal`, `build` or `dev`; absent = normal.
    #[serde(default)]
    pub kind: Option<String>,
    /// The target cfg it is restricted to, if any.
    #[serde(default)]
    pub target: Option<String>,
    /// The real package when renamed.
    #[serde(default)]
    pub package: Option<String>,
}

impl IndexEntry {
    /// Merged features (`features` plus `features2`).
    pub fn all_features(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        let mut merged = self.features.clone();
        for (k, v) in &self.features2 {
            merged.entry(k.clone()).or_default().extend(v.iter().cloned());
        }
        merged
    }
}

/// The sparse-index client with its disk cache.
pub struct IndexCache {
    http: reqwest::Client,
    index_url: String,
    cache_dir: PathBuf,
    /// `--offline`: serve only from the cache, and say so when missing.
    pub offline: bool,
    /// `--frozen`: the lockfile decides; the network is never asked.
    pub frozen: bool,
}

impl IndexCache {
    /// A client for the default crates.io sparse index.
    pub fn new(offline: bool, frozen: bool) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("kiln/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Network {
                context: "building the registry client".into(),
                source: Box::new(e),
            })?;
        Ok(Self {
            http,
            index_url: "https://index.crates.io".to_owned(),
            cache_dir: crate::registry_cache_root().join("index"),
            offline,
            frozen,
        })
    }

    /// Every published version of `name`, from cache or network.
    pub async fn entries(&self, name: &str) -> Result<Vec<IndexEntry>> {
        let cache_file = self.cache_dir.join(format!("{name}.json"));
        if let Ok(text) = std::fs::read_to_string(&cache_file) {
            if let Some(entries) = parse_entries(&text) {
                return Ok(entries);
            }
        }
        if self.offline || self.frozen {
            return Err(Error::Resolve(format!(
                "crate `{name}` is not in the local registry cache and the network is off (--offline/--frozen)"
            )));
        }
        let path = sparse_path(name);
        let url = format!("{}/{}", self.index_url, path);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Network { context: format!("fetching the index of `{name}`"), source: Box::new(e) })?;
        if !response.status().is_success() {
            return Err(Error::Network {
                context: format!("fetching the index of `{name}`"),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("registry answered {}", response.status()),
                )),
            });
        }
        let text = response
            .text()
            .await
            .map_err(|e| Error::Network { context: format!("reading the index of `{name}`"), source: Box::new(e) })?;
        let entries = parse_entries(&text)
            .ok_or_else(|| Error::parse(format!("the index of `{name}`"), "a line is not valid JSON"))?;
        std::fs::create_dir_all(&self.cache_dir)
            .map_err(|e| Error::io("create", &self.cache_dir, e))?;
        let tmp = cache_file.with_extension("tmp");
        std::fs::write(&tmp, &text).map_err(|e| Error::io("write", &tmp, e))?;
        std::fs::rename(&tmp, &cache_file).map_err(|e| Error::io("rename", &cache_file, e))?;
        Ok(entries)
    }
}

/// The sparse index path of a crate: 1/2/3-letter names get their own
/// directories, longer names are sharded by their first two and four letters.
fn sparse_path(name: &str) -> String {
    match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{name}/{name}"),
        // 4 characters and up: the first two, then the next two, then the name.
        _ => format!("{}/{}/{name}", &name[0..2], &name[2..4]),
    }
}

fn parse_entries(text: &str) -> Option<Vec<IndexEntry>> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line).ok()?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_paths_follow_the_protocol() {
        assert_eq!(sparse_path("a"), "1/a");
        assert_eq!(sparse_path("ab"), "2/ab");
        assert_eq!(sparse_path("abc"), "3/abc/abc");
        assert_eq!(sparse_path("serde"), "se/rd/serde");
        assert_eq!(sparse_path("serde_json"), "se/rd/serde_json");
    }

    #[test]
    fn index_entries_parse() {
        let text = r#"{"name":"tiny","vers":"0.1.0","deps":[],"features":{},"checksum":"aa"}
{"name":"tiny","vers":"0.2.0","deps":[{"name":"serde","req":"^1","optional":true}],"features":{"big":["serde"]},"yanked":true,"checksum":"bb","features2":{"big":["serde/derive"]}}"#;
        let entries = parse_entries(text).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].vers.to_string(), "0.1.0");
        assert!(!entries[0].yanked);
        assert!(entries[1].yanked);
        let merged = entries[1].all_features();
        assert_eq!(merged["big"], vec!["serde", "serde/derive"]);
    }

    #[test]
    fn bad_entries_are_rejected_not_half_parsed() {
        assert!(parse_entries("{\"name\":").is_none());
        assert!(parse_entries("not json at all").is_none());
    }

    #[test]
    fn offline_cache_hits_and_misses() {
        // The cache dir is per-user; a name we never fetched must be an
        // explicit offline error, not a silent empty list.
        let name = format!("kiln-never-fetched-{}", std::process::id());
        let err = std::thread::scope(|s| {
            s.spawn(move || {
                let cache = IndexCache::new(true, false).unwrap();
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                rt.block_on(cache.entries(&name)).unwrap_err()
            })
            .join()
            .expect("test thread panicked")
        });
        assert!(err.to_string().contains("network is off"), "{err}");
    }

}

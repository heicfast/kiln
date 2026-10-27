//! Remotes: an HTTP object store (kiln's own `cache serve`, or any server
//! speaking the same three endpoints), or a plain directory.

use kiln_core::{Error, Result};

/// A connected remote.
pub struct Remote {
    kind: RemoteKind,
    http: reqwest::Client,
    token: Option<String>,
}

/// What a remote address resolved to.
#[derive(Debug, Clone)]
pub enum RemoteKind {
    /// `http://` or `https://`: kiln's protocol over HTTP.
    Http(String),
    /// A local directory (also `file:///...`).
    Dir(std::path::PathBuf),
}

impl Remote {
    /// Connect to `url`: `http(s)://...` or a filesystem path.
    pub fn connect(url: &str, token: Option<String>) -> Result<Self> {
        let kind = if let Some(rest) = url.strip_prefix("file://") {
            RemoteKind::Dir(std::path::PathBuf::from(rest))
        } else if url.starts_with("http://") || url.starts_with("https://") {
            RemoteKind::Http(url.trim_end_matches('/').to_owned())
        } else {
            RemoteKind::Dir(std::path::PathBuf::from(url))
        };
        let http = reqwest::Client::builder()
            .user_agent(concat!("kiln/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Network { context: "building the remote client".into(), source: Box::new(e) })?;
        Ok(Self { kind, http, token })
    }

    fn object_path(dir: &std::path::Path, key: &str) -> std::path::PathBuf {
        dir.join("objects").join(&key[..2]).join(format!("{key}.zst"))
    }

    /// Fetch one object; `None` when the remote does not have it.
    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match &self.kind {
            RemoteKind::Dir(dir) => match std::fs::read(Self::object_path(dir, key)) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(Error::io("read", Self::object_path(dir, key), e)),
            },
            RemoteKind::Http(base) => {
                let url = format!("{base}/objects/{}/{}", &key[..2], key);
                let mut request = self.http.get(&url);
                if let Some(token) = &self.token {
                    request = request.bearer_auth(token);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|e| Error::Network { context: format!("fetching {key}"), source: Box::new(e) })?;
                match response.status() {
                    s if s == reqwest::StatusCode::NOT_FOUND => Ok(None),
                    s if s.is_success() => {
                        let bytes = response
                            .bytes()
                            .await
                            .map_err(|e| Error::Network { context: format!("reading {key}"), source: Box::new(e) })?;
                        Ok(Some(bytes.to_vec()))
                    }
                    s => Err(Error::Network {
                        context: format!("fetching {key}"),
                        source: Box::new(std::io::Error::other(format!("remote answered {s}"))),
                    }),
                }
            }
        }
    }

    /// Store one object.
    pub async fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        match &self.kind {
            RemoteKind::Dir(dir) => {
                let path = Self::object_path(dir, key);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| Error::io("create", parent, e))?;
                }
                let tmp = path.with_extension("tmp");
                std::fs::write(&tmp, bytes).map_err(|e| Error::io("write", &tmp, e))?;
                std::fs::rename(&tmp, &path).map_err(|e| Error::io("rename", &path, e))?;
                Ok(())
            }
            RemoteKind::Http(base) => {
                let url = format!("{base}/objects/{}/{}", &key[..2], key);
                let mut request = self.http.put(&url).body(bytes.to_vec());
                if let Some(token) = &self.token {
                    request = request.bearer_auth(token);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|e| Error::Network { context: format!("pushing {key}"), source: Box::new(e) })?;
                if !response.status().is_success() {
                    return Err(Error::Network {
                        context: format!("pushing {key}"),
                        source: Box::new(std::io::Error::other(format!("remote answered {}", response.status()))),
                    });
                }
                Ok(())
            }
        }
    }

    /// Which of `keys` the remote already has.
    pub async fn has_many(&self, keys: &[String]) -> Result<Vec<String>> {
        match &self.kind {
            RemoteKind::Dir(dir) => Ok(keys
                .iter()
                .filter(|k| Self::object_path(dir, k).is_file())
                .cloned()
                .collect()),
            RemoteKind::Http(base) => {
                let url = format!("{base}/has");
                let body = serde_json::to_vec(&keys).map_err(|e| Error::parse("has list", e))?;
                let mut request = self.http.post(&url).body(body);
                if let Some(token) = &self.token {
                    request = request.bearer_auth(token);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|e| Error::Network { context: "checking the remote".into(), source: Box::new(e) })?;
                if !response.status().is_success() {
                    return Err(Error::Network {
                        context: "checking the remote".into(),
                        source: Box::new(std::io::Error::other(format!("remote answered {}", response.status()))),
                    });
                }
                let present: Vec<String> = response
                    .json()
                    .await
                    .map_err(|e| Error::Network { context: "reading the remote's answer".into(), source: Box::new(e) })?;
                Ok(present)
            }
        }
    }

    /// Every key on the remote.
    pub async fn list(&self) -> Result<Vec<String>> {
        match &self.kind {
            RemoteKind::Dir(dir) => {
                let objects = dir.join("objects");
                let mut out = Vec::new();
                let Ok(shards) = std::fs::read_dir(&objects) else { return Ok(out) };
                for shard in shards.flatten() {
                    if !shard.path().is_dir() {
                        continue;
                    }
                    if let Ok(entries) = std::fs::read_dir(shard.path()) {
                        for entry in entries.flatten() {
                            let name = entry.file_name().to_string_lossy().into_owned();
                            if let Some(key) = name.strip_suffix(".zst") {
                                out.push(key.to_owned());
                            }
                        }
                    }
                }
                out.sort();
                Ok(out)
            }
            RemoteKind::Http(base) => {
                let url = format!("{base}/list");
                let mut request = self.http.get(&url);
                if let Some(token) = &self.token {
                    request = request.bearer_auth(token);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|e| Error::Network { context: "listing the remote".into(), source: Box::new(e) })?;
                if !response.status().is_success() {
                    return Err(Error::Network {
                        context: "listing the remote".into(),
                        source: Box::new(std::io::Error::other(format!("remote answered {}", response.status()))),
                    });
                }
                let keys: Vec<String> = response
                    .json()
                    .await
                    .map_err(|e| Error::Network { context: "reading the remote's list".into(), source: Box::new(e) })?;
                Ok(keys)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn directory_remotes_round_trip() {
        let dir = std::env::temp_dir().join(format!("kiln-remote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let remote = Remote::connect(&dir.to_string_lossy(), None).unwrap();
        assert!(matches!(remote.kind, RemoteKind::Dir(_)));

        let key = "9".repeat(64);
        assert!(remote.get(&key).await.unwrap().is_none());
        assert!(remote.has_many(&[key.clone()]).await.unwrap().is_empty());

        let payload = b"payload-bytes".to_vec();
        remote.put(&key, &payload).await.unwrap();
        assert_eq!(remote.get(&key).await.unwrap().unwrap(), payload);
        assert_eq!(remote.has_many(&[key.clone()]).await.unwrap(), vec![key.clone()]);
        assert_eq!(remote.list().await.unwrap(), vec![key]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

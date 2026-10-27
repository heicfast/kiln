//! Error type shared by every kiln crate.

use std::fmt;

/// The one error type that crosses crate boundaries in kiln.
///
/// Every failure names what failed and why in a sentence a user can act on;
/// internal invariants are `bug:`-prefixed so a violation is unmistakable.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A manifest, lockfile or configuration file could not be parsed.
    #[error("{context}: {source}")]
    Parse {
        /// What was being parsed.
        context: String,
        /// The underlying parse failure.
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A file or directory kiln needed is missing.
    #[error("{path}: {message}")]
    Missing {
        /// The path that is missing.
        path: std::path::PathBuf,
        /// Why kiln needs it.
        message: String,
    },

    /// An I/O failure, with the operation named.
    #[error("{op} {path}: {source}")]
    Io {
        /// The operation, e.g. `write`.
        op: &'static str,
        /// The path involved.
        path: std::path::PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },

    /// A dependency constraint could not be satisfied.
    #[error("dependency resolution failed: {0}")]
    Resolve(String),

    /// A network operation failed (registry, remote cache).
    #[error("{context}: {source}")]
    Network {
        /// What kiln was doing.
        context: String,
        /// The underlying transport error.
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A compiler invocation failed. The full command is in `command`.
    #[error("{command} exited with {status}")]
    CommandFailed {
        /// The command that ran.
        command: String,
        /// How it ended.
        status: String,
    },

    /// A build script failed, with the captured stderr tail.
    #[error("build script of `{package}` failed: {message}")]
    BuildScript {
        /// The package whose script failed.
        package: String,
        /// The failure.
        message: String,
    },

    /// Something kiln refuses to do silently (the no-fallback rule: fail fast,
        /// name what is missing, never take a slower hidden path).
    #[error("{0}")]
    Unsupported(String),

    /// An internal invariant broke. This is always a bug.
    #[error("bug: {0}")]
    Bug(String),

    /// Lock contention with another kiln process, with a hint.
    #[error("lock held by another kiln process: {0}")]
    LockHeld(String),
}

impl Clone for Error {
    fn clone(&self) -> Self {
        match self {
            Error::Parse { context, source } => Error::Parse {
                context: context.clone(),
                source: source.to_string().into(),
            },
            Error::Missing { path, message } => {
                Error::Missing { path: path.clone(), message: message.clone() }
            }
            Error::Io { op, path, source } => Error::Io {
                op,
                path: path.clone(),
                source: std::io::Error::other(source.to_string()),
            },
            Error::Resolve(m) => Error::Resolve(m.clone()),
            Error::Network { context, source } => Error::Network {
                context: context.clone(),
                source: source.to_string().into(),
            },
            Error::CommandFailed { command, status } => Error::CommandFailed {
                command: command.clone(),
                status: status.clone(),
            },
            Error::BuildScript { package, message } => {
                Error::BuildScript { package: package.clone(), message: message.clone() }
            }
            Error::Unsupported(m) => Error::Unsupported(m.clone()),
            Error::Bug(m) => Error::Bug(m.clone()),
            Error::LockHeld(m) => Error::LockHeld(m.clone()),
        }
    }
}

impl Error {
    /// Build a `Parse` error.
    pub fn parse(context: impl Into<String>, source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::Parse { context: context.into(), source: source.into() }
    }

    /// Build an `Io` error for `op` on `path`.
    pub fn io(op: &'static str, path: impl Into<std::path::PathBuf>, source: std::io::Error) -> Self {
        Self::Io { op, path: path.into(), source }
    }

    /// Build a `Missing` error.
    pub fn missing(path: impl Into<std::path::PathBuf>, message: impl fmt::Display) -> Self {
        Self::Missing { path: path.into(), message: message.to_string() }
    }

    /// Build a `Bug` error.
    pub fn bug(message: impl fmt::Display) -> Self {
        Self::Bug(message.to_string())
    }

    /// Build an `Unsupported` error (the no-fallback policy).
    pub fn unsupported(message: impl fmt::Display) -> Self {
        Self::Unsupported(message.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Bug(format!("unexpected bare io error: {e}"))
    }
}

/// Convenience alias used across kiln.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_their_subject() {
        let e = Error::missing("/tmp/x", "workspace root does not exist");
        assert_eq!(e.to_string(), "/tmp/x: workspace root does not exist");
        let e = Error::unsupported("git dependencies");
        assert!(e.to_string().starts_with("git dependencies"));
    }
}

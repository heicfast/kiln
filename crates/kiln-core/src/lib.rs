//! Kiln's shared domain model.
//!
//! Everything in here is pure data and pure logic: no I/O, no clocks, no
//! randomness. The build engine, the daemon and the CLI all speak these types,
//! which is what keeps the daemon's in-memory state and a fresh one-shot build
//! in agreement about what a unit is and what its key covers.

pub mod config;
pub mod error;
pub mod graph;
pub mod ids;
pub mod profile;
pub mod unit;

pub use config::{CacheMode, KilnConfig, LinkerPolicy, SandboxMode};
pub use error::{Error, Result};
pub use graph::UnitGraph;
pub use ids::{PackageId, SourceId, TargetTriple};
pub use profile::Profile;
pub use unit::{CompileMode, TargetKind, Unit, UnitKey};

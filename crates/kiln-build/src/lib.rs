//! Kiln's build engine.
//!
//! The engine owns the whole path from a workspace directory to artifacts:
//! discovery, resolution, the unit graph, fingerprints, the cache, the
//! scheduler and the rustc driver. Nothing here shells out to cargo for a
//! build — kiln drives `rustc` itself, one invocation per unit, exactly the
//! way Bazel drives compilers — which is what makes units cacheable,
//! shareable and schedulable.

pub mod buildscript;
pub mod compile;
pub mod engine;
pub mod fingerprint;
pub mod plan;
pub mod runner;
pub mod sandbox;
pub mod scheduler;
pub mod toolchain;

pub use plan::{Plan, PlanRequest, PlannedUnit, plan_units};
pub use toolchain::{LinkerChoice, Toolchain};

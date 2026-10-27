# kiln

**A content-addressed build system for Rust.** Kiln drives your project's own
`rustc`, keeps every compiled unit in a content-addressed store shared across
checkouts and machines, schedules builds on the critical path, and keeps a
daemon resident so a no-op build is milliseconds.

> Status: under active development (v0.1). See `docs/` for the architecture.

## Why

Compile times are Rust's top complaint (Rust Survey, every year). The causes
are structural:

- **Rebuild cascades**: touch a lib, and every dependent recompiles from
  metadata, per checkout, per machine.
- **Cold caches everywhere**: CI and new checkouts start from zero; sccache
  caches object files only, keyed per compiler invocation, and its keys miss
  on any flag change.
- **No daemon**: every `cargo build` pays process startup, fingerprint
  re-reading and lock contention, even when nothing changed.
- **The linker**: the platform default is slow; fast linkers exist but are
  opt-in and per-machine.

Kiln attacks all four at once: artifact-level content addressing (the unit's
every input: sources, features, profile, toolchain, dependency artifacts),
a Bazel-style remote cache, a resident daemon with a watch loop, and
fast-linker autodiscovery — while **always using the project's own rustc**,
so the code that runs is the code rustc produces, on every platform.

## Layout

| Crate | What it is |
|---|---|
| `kiln-core` | Domain model: packages, units, graphs, profiles, config |
| `kiln-manifest` | Cargo.toml / workspace / Cargo.lock parsing, cfg evaluation |
| `kiln-resolve` | Lock-aware dependency resolution, sparse index client |
| `kiln-store` | Content-addressed artifact store: local, remote, `cache serve` |
| `kiln-build` | The engine: fingerprints, unit graph, scheduler, rustc driver |
| `kiln-daemon` | Resident server: watch mode, memoised fingerprints |
| `kiln-live` | Dylib-based hot reload for `#[kiln::hot]` functions |
| `kiln-cli` | `kiln build/check/test/run/fetch/cache/daemon/live` |

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

MIT or Apache-2.0.

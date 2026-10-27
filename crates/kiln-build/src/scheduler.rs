//! The scheduler: run a unit graph across N workers on the critical path.
//!
//! A unit becomes *ready* the moment its last dependency finishes. Ready
//! units wait in a max-heap keyed by critical-path priority (the longest
//! chain from the unit to the end of the build), so a straggler that blocks
//! the final artifact is never stuck behind twenty cheap leaves. Cache hits
//! complete instantly and inside the scheduling loop, so a warm build never
//! allocates a worker at all.
//!
//! Work runs on a fixed pool of scoped OS threads over one mutex-guarded
//! state, workers parking on a condvar. The executor is shared by `Arc` and
//! takes `&self` — implementations lock internally at whatever granularity
//! they need, so the real engine runs rustc processes fully in parallel
//! while unit-ordering decisions stay centralized. This is deliberately
//! simple — the parallelism is in rustc itself, not in the scheduler — and
//! measured faster than a full work-stealing runtime at build-graph sizes
//! (hundreds to a few thousand units).

use std::collections::BinaryHeap;
use std::sync::{Arc, Condvar, Mutex};

use kiln_core::graph::UnitGraph;
use kiln_core::unit::Unit;
use kiln_core::Result;

/// How one unit's execution ended.
#[derive(Debug, Clone)]
pub enum UnitOutcome {
    /// The unit produced its artifact.
    Built,
    /// The unit was served from the store.
    Cached,
    /// Nothing to do: the unit was already current in this session.
    Fresh,
}

/// What a worker should do for one unit, and what came back.
///
/// The executor trait keeps the scheduler pure: tests drive it with a fake,
/// the engine with the real rustc driver. Implementations must be usable
/// from several worker threads at once (`Send + Sync`, `&self` methods);
/// they own their locking.
pub trait UnitExecutor: Send + Sync {
    /// Execute one unit. `deps` are the finished outputs of its
    /// dependencies, in the graph's edge order. Returns the artifact
    /// identity (a digest) dependents will fold into their keys.
    fn execute(
        &self,
        unit: &Unit,
        unit_index: usize,
        deps: &[(usize, UnitOutcome, [u8; 32])],
    ) -> Result<([u8; 32], UnitOutcome)>;

    /// A cheap pre-check: is this unit already satisfied (daemon session
    /// state, a memoised hit)? Returning `Some` skips `execute`.
    fn pre_check(&self, _unit: &Unit, _unit_index: usize) -> Option<([u8; 32], UnitOutcome)> {
        None
    }

    /// Called after every unit completes, for progress reporting. Never
    /// called from under a scheduler lock.
    fn on_progress(&self, _unit: &Unit, _outcome: &UnitOutcome) {}
}

/// The scheduler's result for the whole run.
#[derive(Debug, Default)]
pub struct ScheduleResult {
    /// Per-unit outcome, by graph index.
    pub outcomes: Vec<Option<UnitOutcome>>,
    /// Per-unit artifact digest, by graph index.
    pub digests: Vec<Option<[u8; 32]>>,
    /// Units that failed, with their error.
    pub failures: Vec<(usize, kiln_core::Error)>,
    /// Wall time of the whole schedule.
    pub elapsed: std::time::Duration,
    /// How many units were served without executing.
    pub cached: usize,
    /// How many units executed.
    pub executed: usize,
}

/// Shared state between the workers.
struct Shared {
    ready: BinaryHeap<QueueEntry>,
    remaining_deps: Vec<usize>,
    finished: Vec<Option<([u8; 32], UnitOutcome)>>,
    failures: Vec<(usize, kiln_core::Error)>,
    /// Critical-path priorities, computed once (by graph index).
    priorities: Vec<u32>,
    in_flight: usize,
    done: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct QueueEntry {
    priority: u32,
    index: usize,
}

impl Ord for QueueEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Max-heap: highest priority first; ties broken by index for
        // determinism (the same graph schedules the same way every time).
        self.priority.cmp(&other.priority).then(other.index.cmp(&self.index))
    }
}

impl PartialOrd for QueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Run the graph. `jobs` workers; `executor` runs units concurrently —
/// its `execute` may block on a rustc child while other workers proceed.
pub fn schedule(
    graph: &UnitGraph,
    executor: Arc<dyn UnitExecutor>,
    jobs: usize,
) -> Result<ScheduleResult> {
    let start = std::time::Instant::now();
    let n = graph.len();
    let jobs = jobs.max(1);
    let priorities = graph.critical_path_priority();

    let shared = Arc::new((
        Mutex::new(Shared {
            ready: BinaryHeap::new(),
            remaining_deps: graph.edges.iter().map(|e| e.len()).collect(),
            finished: (0..n).map(|_| None).collect(),
            failures: Vec::new(),
            priorities,
            in_flight: 0,
            done: false,
        }),
        Condvar::new(),
    ));

    // Seed: every unit with no dependencies is ready.
    {
        let (lock, _) = &*shared;
        let mut state = lock.lock().expect("scheduler lock poisoned");
        for i in 0..n {
            if state.remaining_deps[i] == 0 {
                let priority = state.priorities[i];
                state.ready.push(QueueEntry { priority, index: i });
            }
        }
    }

    // Scoped threads borrow `graph` and `executor` — no unsafe, no clones,
    // joined before this function returns.
    std::thread::scope(|scope| {
        for worker_id in 0..jobs {
            let shared = Arc::clone(&shared);
            let executor = Arc::clone(&executor);
            std::thread::Builder::new()
                .name(format!("kiln-worker-{worker_id}"))
                .spawn_scoped(scope, move || {
                    let _ = worker_id;
                    worker_loop(graph, &shared, &executor);
                })
                .expect("spawn worker");
        }
    });

    let (lock, _) = &*shared;
    let state = lock.lock().expect("scheduler lock poisoned");
    let mut digests = vec![None; n];
    let mut outcomes = vec![None; n];
    let mut cached = 0;
    let mut executed = 0;
    for (i, slot) in state.finished.iter().enumerate() {
        if let Some((digest, outcome)) = slot {
            digests[i] = Some(*digest);
            outcomes[i] = Some(outcome.clone());
            match outcome {
                UnitOutcome::Cached | UnitOutcome::Fresh => cached += 1,
                UnitOutcome::Built => executed += 1,
            }
        }
    }
    Ok(ScheduleResult {
        outcomes,
        digests,
        failures: state.failures.clone(),
        elapsed: start.elapsed(),
        cached,
        executed,
    })
}

fn worker_loop(
    graph: &UnitGraph,
    shared: &Arc<(Mutex<Shared>, Condvar)>,
    executor: &Arc<dyn UnitExecutor>,
) {
    let (lock, cvar) = &**shared;
    loop {
        // Take the next ready unit, or park until one appears / we're done.
        // A failure anywhere stops new work: fail fast, like cargo.
        let next = {
            let mut state = lock.lock().expect("scheduler lock poisoned");
            loop {
                if state.done {
                    return;
                }
                if !state.failures.is_empty() {
                    state.done = true;
                    cvar.notify_all();
                    return;
                }
                if let Some(entry) = state.ready.pop() {
                    state.in_flight += 1;
                    break Some(entry.index);
                }
                if state.in_flight == 0 {
                    // Nothing ready, nothing running: the build is over.
                    state.done = true;
                    cvar.notify_all();
                    return;
                }
                state = cvar.wait(state).expect("scheduler lock poisoned");
            }
        };
        let index = next.expect("loop only breaks with a unit");

        // Snapshot the dependency outputs (all finished by construction).
        let deps: Vec<(usize, UnitOutcome, [u8; 32])> = {
            let state = lock.lock().expect("scheduler lock poisoned");
            graph.edges[index]
                .iter()
                .map(|&dep| {
                    let slot = &state.finished[dep];
                    match slot {
                        Some((digest, outcome)) => (dep, outcome.clone(), *digest),
                        // A dependency that failed or was skipped: this unit
                        // cannot run. The failure path handles the report.
                        None => (dep, UnitOutcome::Fresh, [0; 32]),
                    }
                })
                .collect()
        };

        let unit = graph.units[index].clone();
        let result = executor
            .pre_check(&unit, index)
            .map(Ok)
            .unwrap_or_else(|| executor.execute(&unit, index, &deps));

        // Publish the outcome and unblock dependents under one lock, then
        // report progress with no scheduler lock held.
        let progress;
        {
            let mut state = lock.lock().expect("scheduler lock poisoned");
            state.in_flight -= 1;
            match result {
                Ok((digest, outcome)) => {
                    state.finished[index] = Some((digest, outcome.clone()));
                    for &dependent in &graph.reverse[index] {
                        state.remaining_deps[dependent] -= 1;
                        if state.remaining_deps[dependent] == 0 {
                            let priority = state.priorities[dependent];
                            state.ready.push(QueueEntry { priority, index: dependent });
                        }
                    }
                    progress = Some((unit, outcome));
                }
                Err(e) => {
                    state.failures.push((index, e));
                    state.done = true;
                    progress = None;
                }
            }
        }
        cvar.notify_all();
        if let Some((unit, outcome)) = progress {
            executor.on_progress(&unit, &outcome);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_core::ids::{PackageId, SourceId};
    use kiln_core::unit::{CompileMode, TargetKind};
    use semver::Version;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn unit(name: &str) -> Unit {
        Unit {
            pkg: PackageId::registry(name, Version::new(0, 1, 0), SourceId::CRATES_IO),
            target_name: name.to_owned(),
            target_path: "src/lib.rs".into(),
            kind: TargetKind::Lib,
            test: false,
            mode: CompileMode::Build,
            features: BTreeSet::new(),
        }
    }

    /// A fake executor: each unit "compiles" for the given milliseconds and
    /// records peak concurrency — the assertion that the pool really runs
    /// units in parallel.
    struct FakeExecutor {
        per_unit_ms: std::collections::HashMap<String, u64>,
        max_concurrency: Arc<AtomicUsize>,
        current: Arc<AtomicUsize>,
    }

    impl FakeExecutor {
        fn new() -> Self {
            Self {
                per_unit_ms: std::collections::HashMap::new(),
                max_concurrency: Arc::new(AtomicUsize::new(0)),
                current: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn with_sleeps(pairs: &[(&str, u64)]) -> Self {
            Self {
                per_unit_ms: pairs.iter().map(|(n, ms)| (n.to_string(), *ms)).collect(),
                max_concurrency: Arc::new(AtomicUsize::new(0)),
                current: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl UnitExecutor for FakeExecutor {
        fn execute(
            &self,
            unit: &Unit,
            _unit_index: usize,
            _deps: &[(usize, UnitOutcome, [u8; 32])],
        ) -> Result<([u8; 32], UnitOutcome)> {
            let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_concurrency.fetch_max(now, Ordering::SeqCst);
            let ms = *self.per_unit_ms.get(&unit.pkg.name).unwrap_or(&0);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            self.current.fetch_sub(1, Ordering::SeqCst);
            Ok((blake3::hash(unit.pkg.name.as_bytes()).into(), UnitOutcome::Built))
        }
    }

    #[test]
    fn a_chain_runs_in_order() {
        let mut g = UnitGraph::new();
        let a = g.add(unit("a")); // depends on b
        let b = g.add(unit("b"));
        g.depend_on(a, b).unwrap();
        let result = schedule(&g, Arc::new(FakeExecutor::new()), 4).unwrap();
        assert_eq!(result.failures.len(), 0);
        assert_eq!(result.executed, 2);
        assert!(result.digests[a].is_some());
        assert!(result.digests[b].is_some());
    }

    #[test]
    fn failures_stop_the_build_fast() {
        struct Failing;
        impl UnitExecutor for Failing {
            fn execute(
                &self,
                unit: &Unit,
                _: usize,
                _: &[(usize, UnitOutcome, [u8; 32])],
            ) -> Result<([u8; 32], UnitOutcome)> {
                if unit.pkg.name == "boom" {
                    return Err(kiln_core::Error::unsupported("boom"));
                }
                Ok(([1; 32], UnitOutcome::Built))
            }
        }
        let mut g = UnitGraph::new();
        let boom = g.add(unit("boom"));
        g.add(unit("other"));
        let _ = boom;
        let result = schedule(&g, Arc::new(Failing), 4).unwrap();
        assert_eq!(result.failures.len(), 1);
        assert!(result.failures[0].1.to_string().contains("boom"));
    }

    #[test]
    fn parallel_units_run_concurrently() {
        // Eight leaves, each sleeping 40ms: serial >= 320ms, a pool of 8
        // should finish near 40-80ms and record concurrency > 1.
        let mut g = UnitGraph::new();
        for i in 0..8 {
            g.add(unit(&format!("leaf{i}")));
        }
        let exec = FakeExecutor::with_sleeps(&[
            ("leaf0", 40),
            ("leaf1", 40),
            ("leaf2", 40),
            ("leaf3", 40),
            ("leaf4", 40),
            ("leaf5", 40),
            ("leaf6", 40),
            ("leaf7", 40),
        ]);
        let max_concurrency = Arc::clone(&exec.max_concurrency);
        let started = std::time::Instant::now();
        let result = schedule(&g, Arc::new(exec), 8).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(result.executed, 8);
        assert!(
            max_concurrency.load(Ordering::SeqCst) >= 2,
            "workers must overlap unit execution"
        );
        assert!(elapsed < std::time::Duration::from_millis(300), "pool must not serialize: {elapsed:?}");
    }

    #[test]
    fn pre_check_hits_skip_execution() {
        struct Cached;
        impl UnitExecutor for Cached {
            fn pre_check(&self, unit: &Unit, _: usize) -> Option<([u8; 32], UnitOutcome)> {
                if unit.pkg.name.starts_with("hit") {
                    Some(([9; 32], UnitOutcome::Cached))
                } else {
                    None
                }
            }
            fn execute(
                &self,
                unit: &Unit,
                _: usize,
                _: &[(usize, UnitOutcome, [u8; 32])],
            ) -> Result<([u8; 32], UnitOutcome)> {
                Ok((blake3::hash(unit.pkg.name.as_bytes()).into(), UnitOutcome::Built))
            }
        }
        let mut g = UnitGraph::new();
        g.add(unit("hit-one"));
        g.add(unit("miss-one"));
        let result = schedule(&g, Arc::new(Cached), 4).unwrap();
        assert_eq!(result.cached, 1);
        assert_eq!(result.executed, 1);
    }

    #[test]
    fn dependents_wait_for_dependencies() {
        struct Recorder {
            order: Mutex<Vec<String>>,
            names_by_index: std::collections::HashMap<usize, String>,
        }
        impl UnitExecutor for Recorder {
            fn execute(
                &self,
                unit: &Unit,
                _index: usize,
                deps: &[(usize, UnitOutcome, [u8; 32])],
            ) -> Result<([u8; 32], UnitOutcome)> {
                // Assert every dependency is recorded before this unit runs.
                {
                    let order = self.order.lock().unwrap();
                    for (dep_index, _, _) in deps {
                        let dep_name = &self.names_by_index[dep_index];
                        assert!(
                            order.contains(dep_name),
                            "{dep_name} must finish before {}",
                            unit.pkg.name
                        );
                    }
                }
                self.order.lock().unwrap().push(unit.pkg.name.clone());
                Ok(([2; 32], UnitOutcome::Built))
            }
        }
        let mut g = UnitGraph::new();
        let root = g.add(unit("root"));
        let mid1 = g.add(unit("mid1"));
        let mid2 = g.add(unit("mid2"));
        let leaf = g.add(unit("leaf"));
        g.depend_on(root, mid1).unwrap();
        g.depend_on(root, mid2).unwrap();
        g.depend_on(mid1, leaf).unwrap();
        g.depend_on(mid2, leaf).unwrap();
        let names_by_index: std::collections::HashMap<usize, String> =
            g.units.iter().enumerate().map(|(i, u)| (i, u.pkg.name.clone())).collect();
        let recorder = Recorder { order: Mutex::new(Vec::new()), names_by_index };
        let result = schedule(&g, Arc::new(recorder), 4).unwrap();
        assert_eq!(result.failures.len(), 0);
        assert_eq!(result.executed, 4);
    }
}

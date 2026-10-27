//! The unit dependency graph: topological order and critical-path priorities.
//!
//! Kiln's scheduler runs units as soon as their dependencies are done. When
//! several units are ready at once it prefers the one on the longest path to
//! the end of the build, which is the classical list-scheduling heuristic for
//! minimising makespan on a fixed number of workers.

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::unit::Unit;

/// A build graph over units, keyed by unit index.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct UnitGraph {
    /// All units, in insertion order.
    pub units: Vec<Unit>,
    /// `edges[i]` holds the indices of unit `i`'s dependencies.
    pub edges: Vec<Vec<usize>>,
    /// `reverse[i]` holds the indices of the units that depend on unit `i`.
    pub reverse: Vec<Vec<usize>>,
}

impl UnitGraph {
    /// An empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a unit and get its index. Dependencies are declared afterwards.
    pub fn add(&mut self, unit: Unit) -> usize {
        self.units.push(unit);
        self.edges.push(Vec::new());
        self.reverse.push(Vec::new());
        self.units.len() - 1
    }

    /// Declare that `dependent` needs `dependency` (both are indices).
    ///
    /// A dependency listed twice is kept once; an edge from a unit to itself
    /// is a cycle and therefore a bug.
    pub fn depend_on(&mut self, dependent: usize, dependency: usize) -> Result<()> {
        if dependent == dependency {
            return Err(Error::bug(format!(
                "unit `{}` depends on itself",
                self.units[dependent].crate_name()
            )));
        }
        if !self.edges[dependent].contains(&dependency) {
            self.edges[dependent].push(dependency);
            self.reverse[dependency].push(dependent);
        }
        Ok(())
    }

    /// The number of units in the graph.
    pub fn len(&self) -> usize {
        self.units.len()
    }

    /// Whether the graph has no units.
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// Topological order (dependencies first), or the cycle that blocks it.
    ///
    /// Ties are broken by index, which makes the order deterministic: the
    /// same graph always yields the same schedule.
    pub fn topo_order(&self) -> Result<Vec<usize>> {
        // Kahn's algorithm over `edges`: a unit's indegree is the number of
        // its not-yet-emitted dependencies, so the units that start ready are
        // the ones with nothing to wait for.
        let mut indegree: Vec<usize> = self.edges.iter().map(Vec::len).collect();
        let mut ready: std::collections::BinaryHeap<std::cmp::Reverse<usize>> =
            (0..self.units.len())
                .filter(|&i| indegree[i] == 0)
                .map(std::cmp::Reverse)
                .collect();
        let mut order = Vec::with_capacity(self.units.len());
        while let Some(std::cmp::Reverse(i)) = ready.pop() {
            order.push(i);
            for &dependent in &self.reverse[i] {
                indegree[dependent] -= 1;
                if indegree[dependent] == 0 {
                    ready.push(std::cmp::Reverse(dependent));
                }
            }
        }
        if order.len() != self.units.len() {
            let mut cycle: Vec<String> = (0..self.units.len())
                .filter(|i| indegree[*i] > 0)
                .map(|i| self.units[i].crate_name())
                .collect();
            cycle.sort();
            cycle.dedup();
            return Err(Error::Resolve(format!(
                "dependency cycle between: {}",
                cycle.join(", ")
            )));
        }
        Ok(order)
    }

    /// For every unit, the number of units that (transitively) depend on it.
    ///
    /// Used to decide cache-worthiness: an artifact many units consume is
    /// worth keeping far more than a leaf nothing else reads.
    pub fn dependents_count(&self) -> Vec<usize> {
        // Count transitive dependents with a reverse Kahn pass over the
        // condensed topological order.
        let order = self
            .topo_order()
            .unwrap_or_else(|_| (0..self.units.len()).collect());
        let mut counts = vec![0usize; self.units.len()];
        for &i in order.iter().rev() {
            for &dep in &self.edges[i] {
                counts[dep] += 1 + counts[i];
            }
        }
        counts
    }

    /// Critical-path length from each unit to the end of the build.
    ///
    /// `priority[i] = 1 + max(priority[j] for j in reverse[i])`, computed in
    /// reverse topological order; a unit with no dependents scores 1. The
    /// scheduler pops the highest score first: keeping the longest chain busy
    /// is what stops a 2-core machine from idling one core behind a straggler.
    pub fn critical_path_priority(&self) -> Vec<u32> {
        let order = self
            .topo_order()
            .unwrap_or_else(|_| (0..self.units.len()).collect());
        let mut prio = vec![1u32; self.units.len()];
        for &i in order.iter().rev() {
            let mut best = 0u32;
            for &j in &self.reverse[i] {
                best = best.max(prio[j]);
            }
            prio[i] = 1 + best;
        }
        prio
    }

    /// Units whose package matches `filter` (a crate name), plus everything
    /// downstream of them: what a rebuild after an edit has to consider.
    ///
    /// Returns a set of indices in ascending order.
    pub fn affected_downstream(&self, filter: &str) -> BTreeMap<usize, ()> {
        let mut affected = BTreeMap::new();
        let mut stack: Vec<usize> = (0..self.units.len())
            .filter(|&i| self.units[i].pkg.name == filter)
            .collect();
        // Also mark units of the same package even when the filter only
        // matched some of its units, so a `-p` selection stays whole.
        let mut names: Vec<String> = stack
            .iter()
            .map(|&i| self.units[i].pkg.name.clone())
            .collect();
        names.sort();
        names.dedup();
        let extra: Vec<usize> = (0..self.units.len())
            .filter(|&i| names.contains(&self.units[i].pkg.name))
            .collect();
        stack.extend(extra);
        while let Some(i) = stack.pop() {
            if affected.insert(i, ()).is_none() {
                stack.extend(self.reverse[i].iter().copied());
            }
        }
        affected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{PackageId, SourceId};
    use crate::unit::{CompileMode, TargetKind};
    use semver::Version;
    use std::collections::BTreeSet;

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

    #[test]
    fn topo_orders_dependencies_first() {
        let mut g = UnitGraph::new();
        let a = g.add(unit("a")); // a bin that depends on b and c
        let b = g.add(unit("b"));
        let c = g.add(unit("c")); // c depends on b
        g.depend_on(a, b).unwrap();
        g.depend_on(a, c).unwrap();
        g.depend_on(c, b).unwrap();
        let order = g.topo_order().unwrap();
        let pos = |i: usize| order.iter().position(|&x| x == i).unwrap();
        assert!(pos(b) < pos(c));
        assert!(pos(c) < pos(a));
        assert!(pos(b) < pos(a));
    }

    #[test]
    fn cycles_are_reported_not_looped() {
        let mut g = UnitGraph::new();
        let a = g.add(unit("a"));
        let b = g.add(unit("b"));
        g.depend_on(a, b).unwrap();
        g.depend_on(b, a).unwrap();
        let err = g.topo_order().unwrap_err();
        assert!(err.to_string().contains("cycle"), "{err}");
    }

    #[test]
    fn critical_path_prefers_the_long_chain() {
        // chain a -> b -> c, short branch a -> d. `c` sits at the head of the
        // longest remaining chain, so it carries the highest priority.
        let mut g = UnitGraph::new();
        let a = g.add(unit("a"));
        let b = g.add(unit("b"));
        let c = g.add(unit("c"));
        let d = g.add(unit("d"));
        g.depend_on(a, b).unwrap();
        g.depend_on(b, c).unwrap();
        g.depend_on(a, d).unwrap();
        let prio = g.critical_path_priority();
        assert_eq!(prio[a], 1); // nothing depends on the final artifact
        assert_eq!(prio[b], 2); // unblocks a
        assert_eq!(prio[d], 2); // unblocks a
        assert_eq!(prio[c], 3); // unblocks b, which unblocks a
    }

    #[test]
    fn dependents_counts_are_transitive() {
        let mut g = UnitGraph::new();
        let a = g.add(unit("a"));
        let b = g.add(unit("b"));
        let c = g.add(unit("c"));
        g.depend_on(a, b).unwrap();
        g.depend_on(b, c).unwrap();
        let counts = g.dependents_count();
        assert_eq!(counts[c], 2); // b and a
        assert_eq!(counts[b], 1); // a
        assert_eq!(counts[a], 0);
    }

    #[test]
    fn affected_downstream_includes_whole_packages() {
        let mut g = UnitGraph::new();
        let mut lib = unit("app");
        lib.kind = TargetKind::Lib;
        let mut bin = unit("app");
        bin.kind = TargetKind::Bin;
        bin.target_name = "app".into();
        let dep = g.add(unit("dep"));
        let l = g.add(lib);
        let b = g.add(bin);
        g.depend_on(b, l).unwrap();
        g.depend_on(l, dep).unwrap();
        let affected = g.affected_downstream("dep");
        assert!(affected.contains_key(&dep));
        assert!(affected.contains_key(&l));
        assert!(affected.contains_key(&b));
    }
}

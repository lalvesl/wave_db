//! Schema migration system: forward/backward, chains, and lazy application.
//!
//! WaveDB supports two migration types:
//! - **Type 1** — field-level transform (one STRUCT version to the next).
//! - **Type 2** — compose/decompose (merge or split multiple STRUCTs).
//!
//! Migrations are registered in a `MigrationRegistry` which can resolve
//! multi-hop chains (e.g. v1→v4 via v1→v2→v3→v4) at startup time.

use std::collections::HashMap;

/// A reference to a specific struct version: `(STRUCT_ID, version)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VersionRef {
    /// The struct family ID.
    pub struct_id: u32,
    /// The schema version.
    pub version: u8,
}

impl VersionRef {
    /// Construct a new version reference.
    pub const fn new(struct_id: u32, version: u8) -> Self {
        Self { struct_id, version }
    }
}

/// A single migration step — either forward or backward.
#[derive(Debug, Clone)]
pub struct MigrationStep {
    /// Source version.
    pub from: VersionRef,
    /// Target version.
    pub to: VersionRef,
    /// Whether this is a rollback (backward) migration.
    pub is_rollback: bool,
}

/// A planned migration chain: a sequence of steps from source to target.
#[derive(Debug, Clone)]
pub struct MigrationPlan {
    /// Ordered list of steps to execute.
    pub steps: Vec<MigrationStep>,
}

impl MigrationPlan {
    /// Number of steps in the plan.
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Whether this plan is empty.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// Registry of all known migrations.
///
/// Builds a directed graph of version transitions and resolves multi-hop
/// chains via BFS.
#[derive(Debug)]
pub struct MigrationRegistry {
    /// Forward edges: from → [(to, has_rollback)]
    forward: HashMap<VersionRef, Vec<(VersionRef, bool)>>,
    /// Backward edges: to → [(from, _)]
    backward: HashMap<VersionRef, Vec<(VersionRef, bool)>>,
}

impl MigrationRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            forward: HashMap::new(),
            backward: HashMap::new(),
        }
    }

    /// Register a simple forward migration (Type 1: field-level transform).
    pub fn register_forward(&mut self, from: VersionRef, to: VersionRef) {
        self.forward.entry(from).or_default().push((to, false));
    }

    /// Register a forward migration with rollback capability.
    pub fn register_with_rollback(&mut self, from: VersionRef, to: VersionRef) {
        self.forward.entry(from).or_default().push((to, true));
        self.backward.entry(to).or_default().push((from, true));
    }

    /// Register a compose migration (Type 2: merge multiple sources into one target).
    pub fn register_compose(
        &mut self,
        sources: &[VersionRef],
        target: VersionRef,
        has_rollback: bool,
    ) {
        for &src in sources {
            self.forward
                .entry(src)
                .or_default()
                .push((target, has_rollback));
            if has_rollback {
                self.backward.entry(target).or_default().push((src, true));
            }
        }
    }

    /// Resolve a migration plan from `from` to `to`.
    ///
    /// Uses BFS on the forward graph. Returns `Err` if no path exists.
    pub fn resolve(&self, from: VersionRef, to: VersionRef) -> crate::Result<MigrationPlan> {
        if from == to {
            return Ok(MigrationPlan { steps: Vec::new() });
        }

        // BFS forward
        let mut visited: HashMap<VersionRef, VersionRef> = HashMap::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(from);
        visited.insert(from, from);

        while let Some(current) = queue.pop_front() {
            if current == to {
                // Reconstruct path
                let mut path = Vec::new();
                let mut cursor = to;
                while cursor != from {
                    let prev = visited[&cursor];
                    path.push(MigrationStep {
                        from: prev,
                        to: cursor,
                        is_rollback: false,
                    });
                    cursor = prev;
                }
                path.reverse();
                return Ok(MigrationPlan { steps: path });
            }

            if let Some(neighbors) = self.forward.get(&current) {
                for &(next, _has_rollback) in neighbors {
                    if let std::collections::hash_map::Entry::Vacant(e) = visited.entry(next) {
                        e.insert(current);
                        queue.push_back(next);
                    }
                }
            }
        }

        Err(crate::Error::NoMigrationPath {
            from: from.version,
            to: to.version,
        })
    }

    /// Check if a rollback path exists from `from` to `to`.
    pub fn can_rollback(&self, from: VersionRef, to: VersionRef) -> bool {
        if from == to {
            return true;
        }

        let mut visited: std::collections::HashSet<VersionRef> = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(from);
        visited.insert(from);

        while let Some(current) = queue.pop_front() {
            if current == to {
                return true;
            }
            if let Some(neighbors) = self.backward.get(&current) {
                for &(next, _) in neighbors {
                    if visited.insert(next) {
                        queue.push_back(next);
                    }
                }
            }
        }
        false
    }

    /// Number of registered migrations (edges).
    pub fn migration_count(&self) -> usize {
        self.forward.values().map(Vec::len).sum()
    }
}

impl Default for MigrationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: u32 = 7;

    fn v(version: u8) -> VersionRef {
        VersionRef::new(SID, version)
    }

    #[test]
    fn type1_forward_roundtrip() {
        let mut reg = MigrationRegistry::new();
        reg.register_with_rollback(v(1), v(2));

        let plan = reg.resolve(v(1), v(2)).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan.steps[0].from, v(1));
        assert_eq!(plan.steps[0].to, v(2));
    }

    #[test]
    fn type1_rollback_exists() {
        let mut reg = MigrationRegistry::new();
        reg.register_with_rollback(v(1), v(2));
        assert!(reg.can_rollback(v(2), v(1)));
    }

    #[test]
    fn type2_compose_migration() {
        let mut reg = MigrationRegistry::new();
        let order = VersionRef::new(10, 1);
        let order_item = VersionRef::new(11, 1);
        let summary = VersionRef::new(12, 1);

        reg.register_compose(&[order, order_item], summary, true);

        // Both sources can reach the target
        assert!(reg.resolve(order, summary).is_ok());
        assert!(reg.resolve(order_item, summary).is_ok());

        // Rollback from summary to each source
        assert!(reg.can_rollback(summary, order));
        assert!(reg.can_rollback(summary, order_item));
    }

    #[test]
    fn chain_resolution() {
        let mut reg = MigrationRegistry::new();
        reg.register_forward(v(1), v(2));
        reg.register_forward(v(2), v(3));
        reg.register_forward(v(3), v(4));

        let plan = reg.resolve(v(1), v(4)).unwrap();
        assert_eq!(plan.len(), 3, "should be a 3-step chain");
        assert_eq!(plan.steps[0].from, v(1));
        assert_eq!(plan.steps[0].to, v(2));
        assert_eq!(plan.steps[1].from, v(2));
        assert_eq!(plan.steps[1].to, v(3));
        assert_eq!(plan.steps[2].from, v(3));
        assert_eq!(plan.steps[2].to, v(4));
    }

    #[test]
    fn no_path_returns_error() {
        let reg = MigrationRegistry::new();
        let result = reg.resolve(v(1), v(5));
        assert!(result.is_err());
    }

    #[test]
    fn identity_returns_empty_plan() {
        let reg = MigrationRegistry::new();
        let plan = reg.resolve(v(1), v(1)).unwrap();
        assert!(plan.is_empty());
    }

    #[test]
    fn coexistence() {
        // v1 and v2 can coexist; lazy migration runs only on v1 reads
        let mut reg = MigrationRegistry::new();
        reg.register_with_rollback(v(1), v(2));

        // v1→v2 migration is available
        assert!(reg.resolve(v(1), v(2)).is_ok());
        // v2→v1 rollback is available
        assert!(reg.can_rollback(v(2), v(1)));
        // v2→v2 is a no-op
        assert!(reg.resolve(v(2), v(2)).unwrap().is_empty());
    }
}

//! Schema migration system: forward/backward, chains, and lazy application.
//!
//! WaveDB supports two migration types:
//! - **Type 1** — field-level transform (one STRUCT version to the next).
//! - **Type 2** — compose/decompose (merge or split multiple STRUCTs).
//!
//! Migrations are registered in a `MigrationRegistry` which can resolve
//! multi-hop chains (e.g. v1→v4 via v1→v2→v3→v4) at startup time.
//!
//! ## Automatic registration via `#[wave_db]`
//!
//! When `migrate_from` (and optionally `migrate_rollback`) are declared on a
//! struct, the macro generates:
//!
//! - `Struct::register_migration(&mut MigrationRegistry)` — call once at
//!   startup to wire the version edge and type-erased fn pointers into the
//!   registry.
//! - `Struct::__erased_migrate_from(&[u8]) -> Result<Vec<u8>>` — wire-format
//!   round-trip wrapper around the user-supplied forward fn.
//! - `Struct::__erased_migrate_rollback(&[u8]) -> Result<Vec<u8>>` — same for
//!   the optional rollback fn.
//!
//! Both structs (the old and new version) must implement [`crate::Wire`] for
//! the erased wrappers to compile (`#[wave_db]` generates the impl).

use std::collections::HashMap;

/// A type-erased migration function.
///
/// Takes the wire-serialized bytes of the source version and returns the
/// wire-serialized bytes of the target version.
pub type ErasedMigrateFn = fn(&[u8]) -> crate::Result<Vec<u8>>;

/// A registered migration entry holding version refs and type-erased fn pointers.
///
/// Created by `#[wave_db(migrate_from = …)]` and inserted into the
/// `MigrationRegistry` via `register_with_entry`.
#[derive(Debug, Clone)]
pub struct MigrationEntry {
    /// Source version.
    pub from: VersionRef,
    /// Target version.
    pub to: VersionRef,
    /// Forward migration fn (old bytes → new bytes).
    pub migrate_fn: ErasedMigrateFn,
    /// Optional rollback fn (new bytes → old bytes). `None` means forward-only.
    pub rollback_fn: Option<ErasedMigrateFn>,
}

/// Serialize a value with the wire format for use inside type-erased
/// migration fns.
///
/// Generated erased wrappers call this; it is also available for hand-written
/// Type 2 migration glue code.
pub fn serialize_for_migration<T: crate::Wire>(
    val: &T,
) -> crate::Result<Vec<u8>> {
    crate::wire::to_wire(val)
        .map_err(|e| crate::Error::MigrationSer(e.to_string()))
}

/// Deserialize a value with the wire format for use inside type-erased
/// migration fns.
///
/// Generated erased wrappers call this; it is also available for hand-written
/// Type 2 migration glue code.
pub fn deserialize_for_migration<T: crate::Wire>(
    bytes: &[u8],
) -> crate::Result<T> {
    crate::wire::from_wire(bytes)
        .map_err(|e| crate::Error::MigrationDe(e.to_string()))
}

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
/// chains via BFS.  When migrations are registered with `register_with_entry`,
/// the registry also stores type-erased fn pointers so that `migrate_chain`
/// and `rollback_chain` can execute the full chain on raw wire bytes.
#[derive(Debug)]
pub struct MigrationRegistry {
    /// Forward edges: from → [(to, has_rollback)]
    forward: HashMap<VersionRef, Vec<(VersionRef, bool)>>,
    /// Backward edges: to → [(from, _)]
    backward: HashMap<VersionRef, Vec<(VersionRef, bool)>>,
    /// Stored entries with fn pointers (only for macro-registered migrations).
    entries: Vec<MigrationEntry>,
}

impl MigrationRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            forward: HashMap::new(),
            backward: HashMap::new(),
            entries: Vec::new(),
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

    /// Register a rollback edge only — from a future version back to an older one.
    ///
    /// Used by `#[wave_db(migrate_rollback = FutureType)]`: the older struct
    /// owns the "I receive a rollback from `Future`" declaration, so each
    /// struct registers its own rollback edge during its `register_migration`
    /// call.  The forward edge is registered separately by the newer struct
    /// via `register_forward`.
    pub fn register_rollback(
        &mut self,
        future: VersionRef,
        target: VersionRef,
    ) {
        self.backward
            .entry(future)
            .or_default()
            .push((target, true));
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

    /// Register a migration entry generated by `#[wave_db(migrate_from = …)]`.
    ///
    /// Stores the graph edge **and** the type-erased fn pointers so that
    /// `migrate_chain` / `rollback_chain` can execute the full chain.
    pub fn register_with_entry(&mut self, entry: MigrationEntry) {
        let has_rollback = entry.rollback_fn.is_some();
        if has_rollback {
            self.register_with_rollback(entry.from, entry.to);
        } else {
            self.register_forward(entry.from, entry.to);
        }
        self.entries.push(entry);
    }

    /// Execute a chained forward migration from `from` to `to` on raw bytes.
    ///
    /// Resolves the full chain (e.g. v1→v4 via v1→v2→v3→v4) and applies each
    /// type-erased migration fn in order.  Each step must have been registered
    /// via `register_with_entry`; calling this on hand-registered forward/rollback
    /// edges (which carry no fn pointer) returns `NoMigrationPath`.
    ///
    /// Used by the engine for lazy migration: when a record is read with
    /// `struct_version` behind the current compiled version, `migrate_chain` is
    /// called to upgrade the bytes in memory before returning the result.
    pub fn migrate_chain(
        &self,
        from: VersionRef,
        to: VersionRef,
        bytes: &[u8],
    ) -> crate::Result<Vec<u8>> {
        let plan = self.resolve(from, to)?;
        let mut current = bytes.to_vec();
        for step in &plan.steps {
            let entry = self.find_entry(step.from, step.to)?;
            current = (entry.migrate_fn)(&current)?;
        }
        Ok(current)
    }

    /// Execute a chained rollback from `from` (newer) to `to` (older) on raw bytes.
    ///
    /// Finds the forward path `to → from`, reverses it, and applies each
    /// registered rollback fn in sequence.  Returns `NoMigrationPath` if any
    /// step in the chain has no rollback fn registered.
    ///
    /// Used by the engine for rolling back during mixed-version cluster deployments:
    /// a Quick-Node still on an older build calls `rollback_chain` when it receives
    /// bytes for a version it does not yet understand.
    pub fn rollback_chain(
        &self,
        from: VersionRef,
        to: VersionRef,
        bytes: &[u8],
    ) -> crate::Result<Vec<u8>> {
        // Find the forward path (old → new), then walk it in reverse using rollback fns.
        let forward_plan = self.resolve(to, from)?;
        let mut current = bytes.to_vec();
        for step in forward_plan.steps.iter().rev() {
            let entry = self.find_entry(step.from, step.to)?;
            let rollback =
                entry.rollback_fn.ok_or(crate::Error::NoMigrationPath {
                    from: step.to.version,
                    to: step.from.version,
                })?;
            current = rollback(&current)?;
        }
        Ok(current)
    }

    /// Resolve a migration plan from `from` to `to`.
    ///
    /// Uses BFS on the forward graph. Returns `Err` if no path exists.
    pub fn resolve(
        &self,
        from: VersionRef,
        to: VersionRef,
    ) -> crate::Result<MigrationPlan> {
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
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        visited.entry(next)
                    {
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

        let mut visited: std::collections::HashSet<VersionRef> =
            std::collections::HashSet::new();
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

    fn find_entry(
        &self,
        from: VersionRef,
        to: VersionRef,
    ) -> crate::Result<&MigrationEntry> {
        self.entries
            .iter()
            .find(|e| e.from == from && e.to == to)
            .ok_or(crate::Error::NoMigrationPath {
                from: from.version,
                to: to.version,
            })
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

        // Both sources can independently reach the target.
        assert!(reg.resolve(order, summary).is_ok());
        assert!(reg.resolve(order_item, summary).is_ok());

        // Rollback from summary to each source.
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
        let mut reg = MigrationRegistry::new();
        reg.register_with_rollback(v(1), v(2));

        assert!(reg.resolve(v(1), v(2)).is_ok());
        assert!(reg.can_rollback(v(2), v(1)));
        assert!(reg.resolve(v(2), v(2)).unwrap().is_empty());
    }

    #[test]
    fn migrate_chain_with_entry() {
        // Uses erased fn pointers registered via register_with_entry.
        #[allow(clippy::unnecessary_wraps)]
        fn fwd(b: &[u8]) -> crate::Result<Vec<u8>> {
            let mut out = b.to_vec();
            out.push(0xFF); // marker
            Ok(out)
        }
        #[allow(clippy::unnecessary_wraps)]
        fn bwd(b: &[u8]) -> crate::Result<Vec<u8>> {
            let mut out = b.to_vec();
            out.pop(); // strip marker
            Ok(out)
        }

        let mut reg = MigrationRegistry::new();
        reg.register_with_entry(MigrationEntry {
            from: v(1),
            to: v(2),
            migrate_fn: fwd,
            rollback_fn: Some(bwd),
        });

        let input = vec![0x01, 0x02];
        let migrated = reg.migrate_chain(v(1), v(2), &input).unwrap();
        assert_eq!(migrated, vec![0x01, 0x02, 0xFF]);

        let rolled_back = reg.rollback_chain(v(2), v(1), &migrated).unwrap();
        assert_eq!(rolled_back, input);
    }

    #[test]
    fn migrate_chain_multi_hop() {
        #[allow(clippy::unnecessary_wraps)]
        fn add_a(b: &[u8]) -> crate::Result<Vec<u8>> {
            let mut v = b.to_vec();
            v.push(b'a');
            Ok(v)
        }
        #[allow(clippy::unnecessary_wraps)]
        fn add_b(b: &[u8]) -> crate::Result<Vec<u8>> {
            let mut v = b.to_vec();
            v.push(b'b');
            Ok(v)
        }
        #[allow(clippy::unnecessary_wraps)]
        fn rm_b(b: &[u8]) -> crate::Result<Vec<u8>> {
            let mut v = b.to_vec();
            v.pop();
            Ok(v)
        }
        #[allow(clippy::unnecessary_wraps)]
        fn rm_a(b: &[u8]) -> crate::Result<Vec<u8>> {
            let mut v = b.to_vec();
            v.pop();
            Ok(v)
        }

        let mut reg = MigrationRegistry::new();
        reg.register_with_entry(MigrationEntry {
            from: v(1),
            to: v(2),
            migrate_fn: add_a,
            rollback_fn: Some(rm_a),
        });
        reg.register_with_entry(MigrationEntry {
            from: v(2),
            to: v(3),
            migrate_fn: add_b,
            rollback_fn: Some(rm_b),
        });

        let input = vec![0xDE];
        // v1 → v3: should append 'a' then 'b'
        let migrated = reg.migrate_chain(v(1), v(3), &input).unwrap();
        assert_eq!(migrated, vec![0xDE, b'a', b'b']);

        // v3 → v1: should strip 'b' then 'a'
        let rolled_back = reg.rollback_chain(v(3), v(1), &migrated).unwrap();
        assert_eq!(rolled_back, input);
    }
}

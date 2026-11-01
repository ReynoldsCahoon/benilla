# benilla's `bevy_ecs` fork — what differs from upstream, and how to check

Upstream: [`bevyengine/bevy`](https://github.com/bevyengine/bevy) (`bevy_ecs`), MIT OR Apache-2.0,
version `0.18.1`, copied from the cargo registry as published and wired in through
`[patch.crates-io]` in the workspace root.

## The patch — a single-threaded schedule drops its stored access once built

`System::initialize` returns each system's `FilteredAccessSet`, and the schedule keeps it beside
the system for good (`SystemWithAccess::access`, `ConditionWithAccess::access`). Only two readers
exist: a build's conflict check (`Systems::get_conflicting_systems`, cached in
`Schedule::conflicting_systems`) and the multi-threaded executor's `init`. Each set holds one
`FilteredAccess` per parameter, every bitset sized to the app's component count, and benilla's
script call host is one parameter of ~420 accesses held by ~219 systems: ~100 MB of heap that the
single-threaded executor never reads.

`Schedule::initialize` (`src/schedule/schedule.rs`) now calls `SystemSchedule::drop_access`
(`src/schedule/executor/mod.rs`) after initializing a `SingleThreaded` executor, which empties
every system's and condition's stored access. A later rebuild still runs: systems added since keep
their access, and the old ones only stop reporting ambiguities, which no single-threaded run acts
on. `set_executor_kind` to `MultiThreaded` after the drop panics, since that executor's conflict
table would be built from empty sets; benilla picks its executors before a schedule first runs
(`WOW_MT_UPDATE` and friends). Ambiguity tests build fresh schedules, whose first build has the
full access.

## The patch — archetypes and tables keep their component maps compact

Every `Archetype` (`components`) and every `Table` (`columns`) holds an `ImmutableSparseSet`
keyed by `ComponentId`. Upstream its sparse half is a dense `Option<NonMaxUsize>` array as long as
the highest `ComponentId` in it: 8 bytes per component type the app registers, twice per
archetype, whatever the archetype holds. benilla registers thousands of component types and makes
archetypes all session from transient markers: on the iPad these arrays were 70 MB at 30 minutes
of questing and 105 MB at 85 (`SparseSet::into_immutable` from `Archetypes::get_id_or_insert` and
`Tables::get_id_or_insert`).

`ImmutableSparseArray` (`src/storage/sparse_set.rs`) now holds only its present entries, as
`(index, value)` pairs sorted by index, and finds one by binary search: a lookup is O(log n) in
the archetype's own component count (tens), not O(1). The mutable `SparseArray` is unchanged.
Test: `storage::sparse_set::tests::an_immutable_sparse_set_finds_what_was_inserted_and_keeps_only_its_entries`.
Measured (Mac, the quest bot 600 s, heap sampler at 570 s, stock vs patched build): tables,
`Archetypes` and sparse sets together 66.3 MB over 1837 archetypes (36 KB each) vs 29.3 MB over
1953 (15 KB each).

Check: `diff -r <registry>/bevy_ecs-0.18.1 third_party/bevy_ecs` shows only those lines and this
file.

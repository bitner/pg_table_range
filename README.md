# *** Experimental - very AI driven at this point in time. ***

# pg_table_range: PostgreSQL data-range partition pruning

A PostgreSQL 16+ extension that prunes partitions at planning time from a compact
per-partition summary of each column's **actual data** — its min/max range for scalar
columns, or its covering **extent** for range types and PostGIS geometry. This works on
columns that are *not* the partition key, which native PostgreSQL partition pruning
cannot eliminate. Pruning is conservative: a partition is removed only when its summary
provably cannot contain a matching row, so results are always identical to running
without it.

## Quick Start

Summaries are built and maintained through a custom index access method, so pruning
follows the normal index lifecycle (`pg_dump`/restore, `REINDEX`, `DROP INDEX`).

```sql
CREATE EXTENSION pg_table_range;

-- Summarize one or more columns of a partitioned (or plain) table.
CREATE INDEX events_tr ON events USING table_range (val, created_at);

-- Queries now prune partitions whose summary cannot match the predicate.
-- Verify with EXPLAIN: non-matching partitions disappear from the plan.
EXPLAIN (COSTS OFF) SELECT * FROM events WHERE val >= 250;

-- Inserts maintain the summary automatically; REINDEX only re-tightens after many
-- deletes. DROP INDEX removes the summary with the index.
REINDEX INDEX events_tr;
DROP INDEX events_tr;
```

The index is never used for scans — it exists only to build and own the summaries — so
it adds no scan-time overhead and is never chosen by the planner for data access.

### Supported column types (no setup, including PostGIS)

`CREATE INDEX … USING table_range` works on any **btree-comparable** type and any
**range** type out of the box. The required operator classes are provisioned
automatically by mirroring the types that already have a btree/range operator class — and
that mirror re-runs whenever an extension is installed, so **PostGIS geometry works the
moment you `CREATE EXTENSION postgis`, with no extra step**:

```sql
CREATE EXTENSION postgis;                              -- geometry opclass auto-registers
CREATE INDEX places_tr ON places USING table_range (geom);
EXPLAIN (COSTS OFF) SELECT * FROM places WHERE geom && ST_MakeEnvelope(0,0,10,10);
```

## How it works

- **Summaries live in the index.** Like BRIN, each leaf partition's summary is stored in
  that partition's index — one record per indexed column on the index's **metapage**, not
  in any side table. It holds the `has_nulls` / `all_nulls` flags plus either the column's
  btree `min`/`max` (scalar columns) or a single covering **extent** — a covering range
  for range types (`range_merge(range_agg(col))`) or the bounding box for PostGIS geometry
  (`ST_Extent(col)`).
- **Planning.** For each partition the planner builds, a `set_rel_pathlist_hook` reads the
  summary from that partition's index (cached for the plan) and evaluates the partition's
  restriction clauses against it, calling `mark_dummy_rel` on any partition that provably
  cannot match — eliminating it before child paths are generated.
- **Typed comparisons.** Min/max vs. constant comparisons use each column type's own
  btree compare function, so **any btree-comparable type works**: `bigint` / `int` /
  `smallint`, `numeric`, `real` / `double precision`, `text` / `varchar`, `date`,
  `time`, `timestamp`, `timestamptz`, `uuid`, `boolean`, `oid`, etc. Any conversion
  problem degrades safely to "keep".
- **Overlap (`&&`).** For range types and PostGIS geometry, an `&&` (overlaps) predicate
  is pruned by testing the constant against the partition's stored extent with
  PostgreSQL's own `&&` operator — so a partition is eliminated when its extent cannot
  overlap the query.
- **Incremental maintenance (no REINDEX).** `aminsert` widens the summary in place as
  rows are inserted — the same way BRIN maintains its ranges. Because the summary only
  ever needs to be over-inclusive, these updates need no MVCC: an insert within the
  existing range writes nothing; one that extends it grows the min/max/extent. Pruning
  therefore stays correct **and** active across inserts without any rebuild. Deletes only
  shrink a partition's true range, leaving the summary conservatively wide (still safe);
  `VACUUM`/`REINDEX` can re-tighten it for selectivity. `DROP INDEX` removes the summary
  with the index's storage — there is no side table to clean up.

## Performance

The benefit is at **execution**: a selective predicate on a non-key column scans only the
matching partition instead of every partition. On 100 partitions × 30k rows = 3M rows
(`bench/planning_benchmark.sql`, PostgreSQL 18, warm):

| | Total query time (plan + exec) |
|---|---|
| pruning off (scans all 100 partitions) | ~125 ms |
| pruning on  (scans 1 partition)        | ~18 ms  |

Pruning is **not** a free planning-time win: it adds a small per-plan overhead (loading
summaries once, then evaluating each partition — single-digit to low-tens of ms on
hundreds of partitions). It pays off when the partitions it eliminates are large enough
that avoiding their scan outweighs that overhead — so it helps most on **large
partitions with a selective non-key predicate**, and can be a slight net cost on tiny
partitions. Use `table_range.enable_pruning` to measure both ways on your workload.

Summaries are loaded **once per plan** (not per partition); the
`e2e_per_plan_cache_loads_once_regardless_of_partitions` test asserts exactly one
catalog load for a 64-partition query.

## Supported predicates

Everything not listed is conservatively **kept** (never mispruned):

- Comparisons `col < c`, `<=`, `=`, `>=`, `>` (either operand order), and `BETWEEN`
  (the planner expands it into two comparisons).
- `col IS NULL` / `col IS NOT NULL`.
- `col IN (c1, c2, …)` / `col = ANY(<const array>)` — pruned when no listed value falls
  in the partition's range.
- `col && const` (overlaps) for range types and PostGIS `geometry` — pruned when the
  partition's extent cannot overlap the constant.
- Boolean structure composes: `AND` prunes if **any** child proves non-overlap, `OR`
  prunes only if **every** branch does (nested arbitrarily, across any columns).
- Kept (correct, not yet pruned): `NOT IN` / `<> ALL`, `NOT (...)`, function-wrapped
  columns, and parameters in prepared statements until the plan inlines constants.

## Configuration

- `table_range.enable_pruning` (default `on`) — master switch.
- `table_range.log_pruning_debug` (default `off`) — log each prune decision.

## Storage

There is no catalog table — each partition's summary lives on its `table_range` index's
metapage (block 0), written by `ambuild` and updated in place by `aminsert`, like BRIN.

## Project layout

| File | Responsibility |
|------|----------------|
| `src/lib.rs` | GUCs, `_PG_init`, test wiring |
| `src/index_storage.rs` | per-index summary on the metapage: page I/O (Generic WAL) + (de)serialization |
| `src/summary_build.rs` | build a leaf's summary by scanning its data (used by `ambuild`) |
| `src/prune_hook.rs` | planner + pathlist hooks, per-plan cache, typed in-memory evaluation |
| `src/index_am.rs` | `table_range` index AM: build, incremental `aminsert` widening, opclass provisioning |
| `src/e2e_tests.rs`, `src/index_am_tests.rs` | end-to-end tests |

## Building and testing

```sh
cargo pgrx test pg18      # run the end-to-end test suite (PostgreSQL 18)
cargo pgrx run  pg18      # open psql with the extension installed
```

Supported targets: PostgreSQL 16, 17, 18. The test suite is entirely end-to-end —
it builds real partitioned tables, asserts `EXPLAIN` shows the expected partition
elimination, and verifies results are identical with pruning on and off (the
no-false-negative guarantee), including insert/delete/drop correctness paths. The
PostGIS geometry test skips automatically where PostGIS is not installed; CI installs
PostGIS so it runs there, and overlap pruning is also covered on every target by the
range-type tests, which exercise the same code path.

## Limitations

- `NOT IN` / `<> ALL`, `NOT (...)`, expression predicates, and parameterized
  prepared-statement plans are kept rather than pruned.
- Inserts keep summaries current incrementally, but deletes only relax them (the summary
  can stay wider than the live data until a `VACUUM`/`REINDEX` re-tightens it) — always
  correct, just potentially less selective.

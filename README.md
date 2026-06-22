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

-- Recompute after heavy churn; or drop the summaries entirely.
REINDEX INDEX events_tr;
DROP INDEX events_tr;   -- removes the summaries it built
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

- **Summaries.** For each leaf partition and indexed column, one row in
  `table_range_summary` records the `has_nulls` / `all_nulls` flags plus either the
  column's btree `min`/`max` (scalar columns) or a single covering **extent** — a covering
  range for range types (`range_merge(range_agg(col))`) or the bounding box for PostGIS
  geometry (`ST_Extent(col)`).
- **Planning.** A `planner_hook` loads all non-stale summaries once per top-level plan
  (a single query, cached for the duration of planning). A `set_rel_pathlist_hook` then
  evaluates each partition's restriction clauses against its cached summary and calls
  `mark_dummy_rel` on any partition that provably cannot match — eliminating it before
  child paths are generated. Wide partition trees therefore do not pay a per-partition
  lookup.
- **Typed comparisons.** Min/max vs. constant comparisons use each column type's own
  btree compare function, so **any btree-comparable type works**: `bigint` / `int` /
  `smallint`, `numeric`, `real` / `double precision`, `text` / `varchar`, `date`,
  `time`, `timestamp`, `timestamptz`, `uuid`, `boolean`, `oid`, etc. Any conversion
  problem degrades safely to "keep".
- **Overlap (`&&`).** For range types and PostGIS geometry, an `&&` (overlaps) predicate
  is pruned by testing the constant against the partition's stored extent with
  PostgreSQL's own `&&` operator — so a partition is eliminated when its extent cannot
  overlap the query.
- **Automatic correctness.** An insert that extends a partition marks its summary
  *stale* (via the index's `aminsert`), and stale summaries are never used for pruning —
  so a change can never cause a missing row. Deletes only shrink a partition's true
  range, so the summary stays conservatively wide and remains safe. `REINDEX` recomputes
  and re-enables pruning after churn, and a `sql_drop` event trigger removes a dropped
  index's (or table's) summaries.

## Performance

The win is at **planning time** on wide trees, where the planner would otherwise build
paths for every partition. On a 1000-partition table queried by a non-key column
(`bench/planning_benchmark.sql`, PostgreSQL 18):

| | Planning time | Result |
|---|---|---|
| pruning off | ~210 ms | 50 rows |
| pruning on  | ~100 ms | 50 rows |

Pruning removes ~110 ms of child-path planning here. Note the absolute numbers are higher
than they could be: because summaries are owned by a real index, PostgreSQL loads index
metadata for every partition during planning (a flat overhead, ~85 ms on this bare
1000-partition table — proportionally smaller when partitions already carry indexes).

Summaries themselves are loaded **once per plan** (not per partition); the
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

## Catalog

- `table_range_summary` — one summary row per (index, leaf partition, column):
  `index_oid`, `relid`, `attnum`, `kind` (`minmax` or `overlap`), `type_name`,
  `min_summary`, `max_summary`, `has_nulls`, `all_nulls`, `stale`, `tuple_version`.

## Project layout

| File | Responsibility |
|------|----------------|
| `src/lib.rs` | GUCs, `_PG_init`, catalog/bootstrap SQL, test wiring |
| `src/summary_build.rs` | SPI summary build (scalar min/max + range/geometry extent) |
| `src/prune_hook.rs` | planner + pathlist hooks, per-plan cache, typed in-memory evaluation |
| `src/index_am.rs` | `table_range` index access method + automatic operator-class provisioning |
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
- Summaries are exact at build time; an insert that extends a partition marks it stale
  (not pruned, but still correct) until the next `REINDEX`.

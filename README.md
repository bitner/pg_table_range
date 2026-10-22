# *** Experimental - very AI driven at this point in time. ***

# pg_table_range: PostgreSQL data-range partition pruning

[![PGXN version](https://badge.fury.io/pg/pg_table_range.svg)](https://pgxn.org/dist/pg_table_range/)
[![CI](https://github.com/bitner/pg_table_range/actions/workflows/ci.yml/badge.svg)](https://github.com/bitner/pg_table_range/actions/workflows/ci.yml)

A PostgreSQL 16+ extension that prunes partitions at planning time from a compact
per-partition summary of each column's **actual data** — its min/max range for scalar
columns, or its covering **extent** for range types and PostGIS geometry. This works on
columns that are *not* the partition key, which native PostgreSQL partition pruning
cannot eliminate. Pruning is conservative: a partition is removed only when its summary
provably cannot contain a matching row, so results are always identical to running
without it.

> ### ⚠️ Many partitions? Raise `max_locks_per_transaction` first
>
> Pruning (and the index build) on a non-key column requires PostgreSQL to lock **every**
> partition in one transaction. On the **default `max_locks_per_transaction = 64`** this
> exhausts the lock table at roughly **a few thousand partitions**, with:
>
> ```
> ERROR:  out of shared memory
> HINT:   You might need to increase "max_locks_per_transaction".
> ```
>
> If you have thousands of partitions, **raise `max_locks_per_transaction` (it requires a
> restart) before creating the index or querying** — see
> [Scaling and partition count](#scaling-and-partition-count) for sizing. This is a
> PostgreSQL limit on wide non-key access, not specific to this extension.

## Installation

This is a [pgrx](https://github.com/pgcentralfoundation/pgrx) (Rust) extension. Supported:
PostgreSQL **16, 17, 18**. Pick whichever route fits; after installing, run
`CREATE EXTENSION pg_table_range;` in each database that should use it.

### Docker (PostgreSQL 18 + PostGIS, batteries included)

```sh
docker run -e POSTGRES_PASSWORD=secret ghcr.io/bitner/pg_table_range:18
```

The image extends the official `postgres:18` with PostGIS and pg_table_range, and enables
both extensions in the default database on first start. Tags: `:18`, `:vX.Y.Z`, `:latest`.

### Debian / Ubuntu package (no Rust toolchain needed)

Prebuilt `.deb`s for PostgreSQL 16/17/18 (Debian **bookworm**, for the
[apt.postgresql.org / PGDG](https://wiki.postgresql.org/wiki/Apt) packages) are attached to
each [GitHub release](https://github.com/bitner/pg_table_range/releases):

```sh
sudo apt install ./postgresql-18-pg-table-range_<version>-1_amd64.deb
```

### From source with cargo-pgrx

Requires a Rust toolchain and `cargo-pgrx` matching the pinned pgrx version (**0.18.1**).
The bundled `Makefile` wraps cargo-pgrx (it writes a minimal pgrx `config.toml` for you, so
no `cargo pgrx init`/`initdb` is needed):

```sh
cargo install cargo-pgrx --version 0.18.1 --locked
make install PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config   # major auto-detected from pg_config
```

Or drive cargo-pgrx directly — register your PostgreSQL once, then install (use
`--no-default-features --features pg16|pg17` for those majors):

```sh
cargo pgrx init --pg18 /usr/lib/postgresql/18/bin/pg_config
cargo pgrx install --release
```

When upgrading, follow the steps for each version in [CHANGELOG.md](CHANGELOG.md).

The distribution is also on [PGXN](https://pgxn.org/dist/pg_table_range/): `pgxn download
pg_table_range`, unzip, then `make install` (or the cargo-pgrx commands above). `pgxn
install` runs the bundled `Makefile`, so it works too — but still needs cargo-pgrx present,
since this is a Rust extension rather than a C/PGXS one.

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

-- Inserts maintain the summary automatically. After deletes (and VACUUM),
-- table_range_summarize re-tightens it without blocking writes.
SELECT table_range_summarize('events_tr'::regclass);
-- DROP INDEX removes the summary with the index.
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
  for range types (via `range_merge`) or the bounding box for PostGIS geometry (via
  `ST_Extent`). `CREATE INDEX` / `REINDEX` compute it with the table AM's index-build
  scan (as BRIN does), which also covers rows that older snapshots can still see.
- **Planning.** For each partition the planner builds, a `set_rel_pathlist_hook` reads the
  summary from that partition's index and evaluates the partition's restriction clauses
  against it, calling `mark_dummy_rel` on any partition that provably cannot match —
  eliminating it from the plan. Deserialized summaries are cached for
  the life of the backend (kept coherent by a relcache-invalidation callback), so warm
  plans skip the per-partition page read; see [Performance](#performance).
- **Typed comparisons.** Min/max are compared with the query's constant as given, using
  the btree operator family's compare function for the operator's (column, constant)
  type pair — so cross-type predicates such as `bigint_col = 5` or
  `float8_col > 1.5::float4` compare exactly. **Any btree-comparable type works**:
  `bigint` / `int` / `smallint`, `numeric`, `real` / `double precision`, `text` /
  `varchar`, `date`, `time`, `timestamp`, `timestamptz`, `uuid`, `boolean`, enums, `oid`,
  etc. Text-like columns prune only when the query compares under the collation the
  summary was built with (the index column's collation). Any conversion problem degrades
  safely to "keep".
- **Overlap (`&&`).** For range types and PostGIS geometry, an `&&` (overlaps) predicate
  is pruned by calling the clause's own `&&` operator on the partition's stored extent
  and the constant — so a partition is eliminated when its extent cannot overlap the
  query. Only an `&&` implemented in `pg_catalog` or in the column type's own schema is
  trusted.
- **Incremental maintenance (no REINDEX).** `aminsert` widens the summary in place as
  rows are inserted — the same way BRIN maintains its ranges. Because the summary only
  ever needs to be over-inclusive, these updates need no MVCC: an insert within the
  existing range writes nothing; one that extends it grows the min/max/extent, as a
  compare-and-swap on the metapage so concurrent inserts never overwrite each other's
  widening. A widening invalidates cached copies of the summary and cached plans on the
  partition in every backend. If a value cannot be represented (for example it is too
  large for the metapage), that column becomes *unbounded* — it no longer prunes by
  value, but is never too narrow. Pruning therefore stays correct **and** active across
  inserts without any rebuild. Deletes only shrink a partition's true range, leaving the
  summary conservatively wide (still safe); see *Re-summarizing* for re-tightening it.
  `DROP INDEX` removes the summary with the index's storage — there is no side table to
  clean up.
- **Re-summarizing.** `table_range_summarize` rebuilds summaries from the data without
  blocking inserts (like BRIN summarization). It covers every row version still in the
  table, so it re-tightens bounds once `VACUUM` has removed deleted rows. `VACUUM` itself
  rebuilds any summary with an unbounded column (e.g. once an oversized value has been
  deleted) or one left mid-rebuild. `REINDEX` also rebuilds summaries, but blocks writes.

## Performance

**table_range trades a small amount of planning time for a large execution win.** A
selective predicate on a non-key column scans only the matching partition instead of every
partition. The planner pays a little to evaluate each partition's summary, but that cost is
small and — warm — close to free (see the cache note below).

The numbers below are reproducible with `bench/benchmark.sql` (`cargo pgrx run pg18
--release`, then `\i bench/benchmark.sql`); they are medians of the warm `EXPLAIN
(ANALYZE)` planning and execution times over three runs of the script, on PostgreSQL 18.4
with a release build of the extension. Absolute numbers depend on hardware.

**Faster execution.** 300 partitions × 8,000 rows (2.4M rows), `WHERE nk = <value in one
partition>`:

| | Planning | Execution | Total |
|---|---|---|---|
| pruning **off** (scans all 300 partitions) | ~1.5 ms | ~55 ms | ~57 ms |
| pruning **on** (scans 1 partition) | ~1.0 ms | ~0.2 ms | **~1.2 ms** |

Execution is ~275× faster, total time drops ~45×, and planning is no slower (pruned
partitions drop out before the parent's paths are built). The win grows with how much data the eliminated partitions hold; measure your
workload with `table_range.enable_pruning`.

**Honest comparison to native pruning.** When a predicate is on the *partition key*,
PostgreSQL prunes natively — and that path is in a different league, because it eliminates
partitions from a sorted bound array *before* they are ever locked or opened. The table
below uses two identical columns on the same table: `pk` (the range partition key, pruned
natively) and `nk` (the same values, not the key, pruned by table_range):

| Same `=` predicate, 2,000 partitions | Planning | Execution |
|---|---|---|
| native pruning — column **is** the partition key | **~0.05 ms** | ~0.02 ms |
| table_range — column is **not** the partition key | ~15 ms | ~0.02 ms |
| no pruning — scans all 2,000 partitions | ~15 ms | ~17 ms |

Native pruning is *hundreds of times* cheaper to plan and is effectively constant in the
partition count. table_range cannot match that (see
[Scaling](#scaling-and-partition-count)): its job is the case native pruning **can't** do
— eliminating partitions by a non-key column. table_range's own cost is small next to the
no-pruning baseline (0–3 ms across runs, under 1.5 µs per partition): nearly all of the
~15 ms is PostgreSQL expanding 2,000 partitions.

**Comparison to `CHECK` constraint exclusion.** The built-in way to prune on a non-key
column is to put a data-range `CHECK (col BETWEEN lo AND hi)` on each partition and let the
planner's constraint exclusion refute it. That is the most direct apples-to-apples
baseline. Same table, 2,000 partitions, same `nk = <value>` predicate:

| Same `=` predicate, 2,000 partitions | Planning | Execution | Scans |
|---|---|---|---|
| `CHECK` constraint exclusion (`constraint_exclusion=on`) | ~17.6 ms | ~0.04 ms | 1 partition |
| table_range pruning | ~14.7 ms | ~0.04 ms | 1 partition |
| no pruning | ~14.2 ms | ~15.6 ms | all 2,000 |

Both are O(partitions) and give the **identical execution win**, and **table_range plans
faster than constraint exclusion.** (Constraint
exclusion re-parses each partition's `CHECK` expression on every plan; table_range serves
warm plans from a cached summary, see below.) On top of matching the speed, table_range
avoids everything `CHECK` constraints make you give up:

- **No manual management** — `CREATE INDEX` builds and owns the ranges; you don't compute
  and attach a constraint per partition and keep it correct.
- **No enforcement / no blocked inserts** — a real `CHECK` *rejects* out-of-range rows;
  table_range's summary simply widens to cover new data, so inserts never fail.
- **Incremental maintenance** — changing a `CHECK` means `DROP`/`ADD CONSTRAINT` with a
  full-partition revalidation scan; table_range widens in place in `aminsert`, no rescan.

**Why the per-partition cost is small.**

1. *Per-plan compilation.* An operator's strategy and compare function, and a type's input
   function, are the same for every partition, so they are resolved once per plan (cached
   `FmgrInfo`s).
2. *Backend summary cache.* Each index's deserialized summary is cached for the life of the
   backend, so warm plans skip the per-partition index open and metapage read. `aminsert`
   only ever *widens* a summary, and when it does it invalidates the cached copy (and
   cached plans on that partition) everywhere — so a cached summary is never narrower than
   reality. A cold first plan still reads each page.

**Insert cost.** 100,000 rows in one `INSERT … SELECT` into one partition:

| | Time | WAL |
|---|---|---|
| no index | ~45–52 ms | 7.9 MB |
| table_range, values inside the summary's bounds | ~88 ms | 7.9 MB |
| table_range, ascending values (every row widens the summary) | ~280 ms | 13.3 MB |

Within a statement each partition's summary is decoded once, so a row inside the bounds
costs a page read and a comparison (~0.4 µs) and writes nothing. For PostGIS geometry the
comparison is a bounding-box containment check (~4 µs per covered row). A row that widens the
summary costs ~2.3 µs and ~55 bytes of WAL. Every transaction that widens a summary also
invalidates cached plans on that partition (see [Limitations](#limitations)).

## Scaling and partition count

table_range's planning cost is **O(number of partitions)**: PostgreSQL builds a planner
node for every partition of the table for a non-key predicate, and table_range evaluates
each one's summary. This is fundamentally different from native partition pruning, which
is ~O(log n) because it prunes on the partition key before expansion. There is no public
planner hook that prunes a non-key column before expansion, so this O(n) cost is inherent.

Two practical consequences and how to handle them:

- **Lock table exhaustion (the hard wall).** Two operations lock *every* partition (and its
  indexes) in a single transaction:
  - **`CREATE INDEX … USING table_range`**, which builds one child index per partition — so
    on too many partitions the index can't even be *built* (it fails and rolls back, leaving
    no summary, so queries then scan everything);
  - **any query on a non-key column**, whose planning expands and locks all partitions.

  On the default **`max_locks_per_transaction = 64`** the lock table holds only ~6,400 locks
  (with default `max_connections = 100`), so at roughly **a few thousand partitions** — where
  `2 × partitions` exceeds that — you get:

  ```
  ERROR:  out of shared memory
  HINT:   You might need to increase "max_locks_per_transaction".
  ```

  This is a PostgreSQL limit on wide non-key access, not specific to this extension — native
  key pruning avoids it by never locking pruned partitions.

  **How to fix it.** Raise `max_locks_per_transaction`. It is a postmaster-level setting, so
  it **requires a restart**:

  ```sql
  ALTER SYSTEM SET max_locks_per_transaction = 4096;   -- then restart PostgreSQL
  ```

  Sizing: the lock table holds about `max_locks_per_transaction × (max_connections +
  max_prepared_transactions)` locks, and one statement over *N* partitions needs roughly
  `2 × N` of them (a heap + an index lock per partition). Pick a value so that product
  comfortably exceeds `2 × N` for your largest partitioned table, with headroom for
  concurrency. With default `max_connections`, a few thousand (e.g. `4096`) covers tens of
  thousands of partitions; each lock slot costs only a few hundred bytes of shared memory.
- **Planning time grows with partition count.** Even below the lock wall, planning scales
  linearly, dominated by PostgreSQL's own partition expansion (~15 ms for 2,000 partitions
  in the benchmark above); table_range's evaluation adds little on top. **Mitigations:**
  prefer **fewer, larger partitions** (table_range's
  sweet spot — the execution win is biggest there anyway); use **prepared statements** so a
  plan is reused across executions; and where you can, **align the hot filter column with
  the partition key** so native pruning handles it.

In short, table_range targets **hundreds to a few thousand sizeable partitions with a
selective non-key predicate**. For tens of thousands of partitions, non-key pruning is not
something an extension can make sub-linear today; that would require pre-expansion pruning
support in PostgreSQL core.

## Supported predicates

Everything not listed is conservatively **kept** (never mispruned):

- Comparisons `col < c`, `<=`, `=`, `>=`, `>` (either operand order), and `BETWEEN`
  (the planner expands it into two comparisons), including cross-type operators of the
  column's btree operator family. Comparisons under a collation other than the index
  column's (e.g. `col < 'x' COLLATE "C"` on an `en_US` column) are kept.
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

`SELECT table_range_summarize('index'::regclass)` rebuilds every summary of a table_range
index (each leaf of a partitioned index) from the data and returns how many it rebuilt.
It takes `ShareUpdateExclusiveLock` on each partition in turn, so inserts continue; it
requires owning the table and, like `VACUUM`, runs index expressions as the table owner.

## Storage

There is no catalog table — each partition's summary lives on its `table_range` index's
metapage (block 0), written by `ambuild` and updated in place by `aminsert`, like BRIN.
Bounds are stored as each type's text form, written and read with the type I/O settings
that affect it (`DateStyle`, `IntervalStyle`, `extra_float_digits`) pinned to
round-trip-exact values. The on-page format is versioned: format 2, or format 3 (format 2
plus a header flag) while a rebuild is in progress. A summary in any other format is never
used for pruning until `REINDEX` rebuilds it.

## Project layout

| File | Responsibility |
|------|----------------|
| `src/lib.rs` | GUCs, `_PG_init`, test wiring |
| `src/index_storage.rs` | per-index summary on the metapage: page I/O (Generic WAL) + (de)serialization |
| `src/summary_build.rs` | build a leaf's summary by scanning its data (used by `ambuild`) |
| `src/prune_hook.rs` | planner + pathlist hooks, per-plan compilation cache, typed in-memory evaluation |
| `src/summarize.rs` | rebuilds summaries (VACUUM, `table_range_summarize`) without blocking inserts |
| `src/summary_cache.rs` | backend-lifetime per-index summary cache + relcache-invalidation coherence |
| `src/stable_io.rs` | pins the GUCs that change a type's text form while summaries are written or read |
| `src/index_am.rs` | `table_range` index AM: build, incremental `aminsert` widening, opclass provisioning |
| `src/e2e_tests.rs`, `src/index_am_tests.rs` | end-to-end tests |
| `tests/multisession.sh` | tests that need several concurrent backends (run against a live server) |
| `tests/standby.sh` | summary changes reaching queries on a hot standby (creates its own primary and standby) |
| `tests/upgrade.sh` | `ALTER EXTENSION UPDATE` from the previous release (builds it from its git tag) |
| `sql/` | extension upgrade scripts |

## Building and testing

```sh
cargo pgrx test pg18      # run the end-to-end test suite (PostgreSQL 18)
cargo pgrx run  pg18      # open psql with the extension installed

# multi-backend tests, against a running server with the extension installed
cargo pgrx install && cargo pgrx start pg18
PGHOST=localhost PGPORT=28818 tests/multisession.sh

# hot standby and upgrade tests, each on its own throwaway cluster
PATH="$(dirname "$(cargo pgrx info pg-config pg18)"):$PATH" tests/standby.sh
tests/upgrade.sh "$(cargo pgrx info pg-config pg18)"
```

Supported targets: PostgreSQL 16, 17, 18. The test suite is entirely end-to-end —
it builds real partitioned tables, asserts `EXPLAIN` shows the expected partition
elimination, and verifies results are identical with pruning on and off (the
no-false-negative guarantee), including insert/delete/drop correctness paths. The
PostGIS geometry test skips automatically where PostGIS is not installed; CI installs
PostGIS so it runs there, and overlap pruning is also covered on every target by the
range-type tests, which exercise the same code path.

## Releasing (maintainers)

`.github/workflows/release.yml` runs on any `v*` tag and produces everything:

| Job | Output |
|-----|--------|
| `debian` | `.deb` packages for PostgreSQL 16/17/18 (Debian bookworm / PGDG), built in a `debian:bookworm` container with `packaging/build-deb.sh` |
| `docker` | `ghcr.io/<owner>/pg_table_range:18`, `:vX.Y.Z`, `:latest` (PostgreSQL 18 + PostGIS + the extension) |
| `github-release` | attaches the `.deb`s to the GitHub Release |
| `pgxn` | validates `META.json`, bundles the source, and uploads it to [PGXN](https://pgxn.org) |

`workflow_dispatch` runs `debian` + `docker` as a **dry run** (no pushes), to validate
packaging without cutting a release.

One-time setup: add two repository secrets, `PGXN_USERNAME` and `PGXN_PASSWORD`, from a
[PGXN Manager](https://manager.pgxn.org) account. (The Docker push uses the built-in
`GITHUB_TOKEN`; ensure the repo allows GitHub Actions to publish packages.)

To cut a release:

1. Bump the version in **`Cargo.toml`** and **`META.json`** (keep them identical; the
   `.control` file's `default_version` is filled from `Cargo.toml` at build time).
2. Commit, then tag and push — the tag without its leading `v` must equal the
   `META.json` version:

   ```sh
   git tag v0.1.0 && git push origin v0.1.0
   ```

`META.json` describes the PGXN distribution; `PLAN.md` and `.github/` are excluded from
the published tarball via `.gitattributes`.

## Limitations

- **Lock-table wall on many partitions.** Both `CREATE INDEX … USING table_range` and
  queries on a non-key column lock every partition at once, so on the default
  `max_locks_per_transaction = 64` they fail (`out of shared memory`) at roughly a few
  thousand partitions. **Raise `max_locks_per_transaction` (needs a restart)** — see
  [Scaling](#scaling-and-partition-count) for sizing. Native partition-key pruning does not
  have this limit; table_range is for the cases native pruning cannot handle.
- Pruning is a **planning-time cost / execution-time win** tradeoff: on small partitions
  the per-plan overhead can exceed the scan it saves. Measure with
  `table_range.enable_pruning`.
- `NOT IN` / `<> ALL`, `NOT (...)`, expression predicates, and parameterized
  prepared-statement plans are kept rather than pruned.
- Partial indexes (`CREATE INDEX … USING table_range … WHERE …`) are rejected, and
  expression columns in a table_range index are not summarized.
- Inserts keep summaries current incrementally, but deletes only relax them (the summary
  can stay wider than the live data until `VACUUM` plus `table_range_summarize`, or a
  `REINDEX`, re-tightens it) — always correct, just potentially less selective.
- A `table_range_summarize` that errors or is cancelled leaves the summary marked
  mid-rebuild, so that partition is not pruned until the next `VACUUM` or
  `table_range_summarize`. `VACUUM (PARALLEL …)` and `VACUUM (INDEX_CLEANUP OFF)` skip
  rebuilding summaries; run `table_range_summarize` instead.
- A column can appear only once in a table_range index.
- Every transaction that widens a summary invalidates cached plans on that partition in
  every backend (a widening's WAL is small, but plans are replanned). Append-only
  workloads whose values keep growing (e.g. timestamps) widen on most inserts.
- Pruning is decided when a query is planned. Under `READ COMMITTED`, an insert that
  widens a summary and commits after a query is planned but before it starts executing
  can be missed by that one execution (cached plans are replanned once the widening
  commits). A query planned and run in one `REPEATABLE READ` / `SERIALIZABLE` snapshot is
  not affected.
- Pruning engages only while the index is **valid** (`indisvalid`); the planner ignores
  invalid indexes, so anything that invalidates a table_range index silently disables its
  pruning until rebuilt.

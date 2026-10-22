# Changelog

## 0.2.0

Summaries written by 0.1.2 are read as they are, so no `REINDEX` is needed. To upgrade,
install 0.2.0, **restart PostgreSQL** (a backend still running 0.1.2 cannot read a summary
that is being rebuilt, and would skip maintaining it), then run
`ALTER EXTENSION pg_table_range UPDATE;`.

- Widening a summary on insert writes a small WAL record (the changed bytes) instead of a
  full page image: ~55 bytes instead of ~8.4 KB per widening row.
- Inserts decode each partition's summary once per statement: a row inside the bounds
  costs ~0.4 µs instead of ~14 µs (PostGIS geometry: ~4 µs instead of ~70 µs), and a
  widening row ~2.3 µs.
- New `table_range_summarize(index)` rebuilds summaries from the data without blocking
  inserts, re-tightening bounds once `VACUUM` has removed deleted rows. It runs index
  expressions as the table owner. `VACUUM` rebuilds summaries that have an unbounded
  column (e.g. after an oversized value is deleted) or were left mid-rebuild.
- Fixed: an insert that widened a summary and then rolled back left the summary widened
  without telling other backends. A later insert of a value in that range then changed
  nothing, so other backends could keep pruning with their cached summary and miss the
  committed row. This also affected 0.1.2. Widening now notifies other backends, and
  hot standbys, immediately.
- A column can no longer appear twice in a table_range index (e.g. under two
  collations); both would have shared one summary.
- Summaries are written in on-page format 2 as before, and in format 3 (with a header
  flag) while a rebuild is in progress.

## 0.1.2

A correctness release. 0.1.1 could return wrong results (rows missing from query
results) and could crash the server.

Summaries built by 0.1.1 are not trusted: 0.1.2 never prunes with them, so each
table_range index keeps all partitions (results correct, just unpruned) until it is
rebuilt. To upgrade:

1. Install 0.1.2 and **restart PostgreSQL**, so no backend still runs 0.1.1 code (a
   0.1.1 backend cannot read 0.1.2 summaries and would skip widening them).
2. `ALTER EXTENSION pg_table_range UPDATE;` (warns how many indexes need rebuilding).
3. `REINDEX INDEX` each table_range index (on a partitioned table, the parent index).

### Crashes fixed

- Any error raised while planning a query (e.g. `SELECT 1/0`) in a backend that had
  loaded the extension aborted the backend, restarting the whole cluster.
- `REINDEX` of a table_range index crashed the server the same way.

### Wrong results fixed

- Prepared statements and PL/pgSQL functions kept using plans that pruned a partition
  after an insert had widened its summary.
- A second widening inside one transaction was invisible to that transaction's later
  queries; after a rolled-back savepoint, a later widening was never seen by other
  backends.
- Concurrent inserts could overwrite each other's widening.
- A widening that failed (e.g. a value too large for the metapage) was silently skipped.
- Partial table_range indexes ignored inserts outside their predicate. They are now
  rejected.
- `CREATE INDEX` inside a `REPEATABLE READ` transaction missed rows committed after the
  transaction's snapshot, and builds omitted rows still visible to older snapshots.
- Summary bounds depended on `DateStyle` and `extra_float_digits` at write and read time.
- Cross-type comparisons (e.g. `float8_col > 1.5::float4`) re-parsed the constant as the
  column's type.
- Comparisons under a different collation than the summary's (e.g. `COLLATE "C"` on an
  `en_US` column) were pruned with the wrong ordering.

### Other fixes

- `CREATE INDEX` / `REINDEX` set the table's `reltuples` to 0, so the planner estimated
  one row per partition until the next `ANALYZE`.
- Indexes including a `uuid` or `boolean` column silently built no summary at all (those
  types have no `min()`/`max()`). Builds no longer use SQL aggregates.
- On PostgreSQL 17+, geometry summaries could not be built (index builds run with a
  restricted `search_path`). All SQL the extension runs is now schema-qualified, which
  also stops a caller's `search_path` from substituting functions used to maintain
  summaries.
- Updates that change only table_range-indexed columns can be HOT updates again
  (`amsummarizing`).
- The README no longer claims `VACUUM` re-tightens summaries; `REINDEX` does.

### Format

Summaries are stored in on-page format version 2. Version-1 summaries are ignored.

# Changelog

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

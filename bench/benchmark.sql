-- Reproducible benchmarks for pg_table_range.
--
--   cargo pgrx run pg18
--   \i bench/benchmark.sql
--
-- Reports planning and execution time separately (warm) via EXPLAIN (ANALYZE), for:
--   1. Plan-vs-execution tradeoff: a selective non-key predicate, pruning on vs. off.
--   2. Honest comparison to native partition pruning: two identical columns, one the
--      partition key (pruned natively) and one not (pruned by table_range).
--
-- Note: cargo pgrx run reuses its database. If you have previously loaded older versions
-- of this extension, stale event triggers can interfere; this script drops the extension
-- and recreates it for a clean baseline.

\set ECHO none
\pset pager off

DROP EXTENSION IF EXISTS pg_table_range CASCADE;
-- Clear any event triggers left behind by older builds (no-ops on a clean DB).
DROP EVENT TRIGGER IF EXISTS table_range_hide_indexes_trg;
DROP EVENT TRIGGER IF EXISTS table_range_drop_trg;
DROP EVENT TRIGGER IF EXISTS table_range_opclass_sync_trg;
DROP FUNCTION IF EXISTS table_range_hide_indexes() CASCADE;
CREATE EXTENSION pg_table_range;

-- ====================================================================================
-- 1. Plan-vs-execution tradeoff: 300 partitions x 8,000 rows, predicate hits 1 partition.
--    `region` is the partition key; `val` is a disjoint band per partition (non-key).
-- ====================================================================================
DROP TABLE IF EXISTS bench_a CASCADE;
CREATE TABLE bench_a (region int, val bigint, pad text) PARTITION BY LIST (region);
SELECT format('CREATE TABLE bench_a_%s PARTITION OF bench_a FOR VALUES IN (%s);', g, g)
FROM generate_series(1, 300) g \gexec
INSERT INTO bench_a
SELECT g, g * 1000000 + s, repeat('x', 40)
FROM generate_series(1, 300) g, generate_series(0, 7999) s;
VACUUM ANALYZE bench_a;
CREATE INDEX bench_a_tr ON bench_a USING table_range (val);

SET table_range.enable_pruning = on;
SELECT count(*) FROM bench_a WHERE val = 150004000;  -- warm
\echo '==== A: pruning ON (scans 1 of 300 partitions) ===='
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT count(*) FROM bench_a WHERE val = 150004000;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT count(*) FROM bench_a WHERE val = 150004000;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT count(*) FROM bench_a WHERE val = 150004000;

SET table_range.enable_pruning = off;
SELECT count(*) FROM bench_a WHERE val = 150004000;  -- warm
\echo '==== A: pruning OFF (scans all 300 partitions) ===='
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT count(*) FROM bench_a WHERE val = 150004000;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT count(*) FROM bench_a WHERE val = 150004000;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT count(*) FROM bench_a WHERE val = 150004000;
DROP TABLE bench_a CASCADE;

-- ====================================================================================
-- 2. table_range vs. native pruning: 2,000 partitions, two identical columns.
--    `pk` is the RANGE partition key (native pruning); `nk` holds the same values but is
--    NOT the key (table_range pruning). Same `=` predicate, so the only difference is
--    which mechanism prunes.
-- ====================================================================================
DROP TABLE IF EXISTS bench_b CASCADE;
CREATE TABLE bench_b (pk bigint, nk bigint) PARTITION BY RANGE (pk);
SELECT format('CREATE TABLE bench_b_%s PARTITION OF bench_b FOR VALUES FROM (%s) TO (%s);',
              g, g * 1000, (g + 1) * 1000)
FROM generate_series(1, 2000) g \gexec
INSERT INTO bench_b
SELECT v, v FROM (SELECT g * 1000 + s AS v
                  FROM generate_series(1, 2000) g, generate_series(0, 49) s) q;
VACUUM ANALYZE bench_b;
CREATE INDEX bench_b_tr ON bench_b USING table_range (nk);

SET table_range.enable_pruning = off;
SELECT count(*) FROM bench_b WHERE pk = 1000500;  -- warm
\echo '==== B: NATIVE pruning (predicate on the partition key) ===='
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT count(*) FROM bench_b WHERE pk = 1000500;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT count(*) FROM bench_b WHERE pk = 1000500;

SET table_range.enable_pruning = on;
SELECT count(*) FROM bench_b WHERE nk = 1000500;  -- warm
\echo '==== B: table_range pruning (same predicate, non-key column) ===='
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT count(*) FROM bench_b WHERE nk = 1000500;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT count(*) FROM bench_b WHERE nk = 1000500;

SET table_range.enable_pruning = off;
SELECT count(*) FROM bench_b WHERE nk = 1000500;  -- warm
\echo '==== B: NO pruning (non-key column, scans all 2,000 partitions) ===='
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT count(*) FROM bench_b WHERE nk = 1000500;
DROP TABLE bench_b CASCADE;

-- ====================================================================================
-- 3. (Optional) The lock-table wall. At ~10,000 partitions a non-key predicate exhausts
--    the lock table on default settings:
--      ERROR: out of shared memory
--      HINT:  You might need to increase "max_locks_per_transaction".
--    Raise max_locks_per_transaction (e.g. to 4000) and restart to push the wall out.
--    Native key pruning is unaffected because it never locks pruned partitions.
-- ====================================================================================

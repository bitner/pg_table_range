-- Reproducible benchmarks for pg_table_range.
--
--   cargo pgrx run pg18 --release
--   \i bench/benchmark.sql
--
-- Reports planning and execution time separately (warm) via EXPLAIN (ANALYZE), for:
--   1. Plan-vs-execution tradeoff: a selective non-key predicate, pruning on vs. off.
--   2. Honest comparison to native partition pruning: two identical columns, one the
--      partition key (pruned natively) and one not (pruned by table_range).
--   3. Comparison to CHECK constraint exclusion.
--   4. Insert cost (time and WAL), with and without a table_range index.
--
-- The script drops and recreates the extension for a clean baseline.

\set ECHO none
\pset pager off

DROP EXTENSION IF EXISTS pg_table_range CASCADE;
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
-- 3. table_range vs. CHECK constraint exclusion: the built-in way to prune on a non-key
--    column is a data-range CHECK on each partition. Same table, same predicate; we toggle
--    constraint_exclusion vs. table_range.enable_pruning to compare the two mechanisms.
-- ====================================================================================
DROP TABLE IF EXISTS bench_c CASCADE;
CREATE TABLE bench_c (region int, nk bigint) PARTITION BY LIST (region);
-- Each partition gets a data-driven CHECK on the non-key column nk.
SELECT format(
  'CREATE TABLE bench_c_%s PARTITION OF bench_c FOR VALUES IN (%s); '
  'ALTER TABLE bench_c_%s ADD CONSTRAINT bench_c_%s_nk CHECK (nk >= %s AND nk <= %s);',
  g, g, g, g, g * 1000, g * 1000 + 999)
FROM generate_series(1, 2000) g \gexec
INSERT INTO bench_c
SELECT g, g * 1000 + s FROM generate_series(1, 2000) g, generate_series(0, 49) s;
VACUUM ANALYZE bench_c;
CREATE INDEX bench_c_tr ON bench_c USING table_range (nk);

SET constraint_exclusion = on;
SET table_range.enable_pruning = off;
SELECT count(*) FROM bench_c WHERE nk = 1000025;  -- warm
\echo '==== C: CHECK constraint exclusion (table_range off) ===='
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT count(*) FROM bench_c WHERE nk = 1000025;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT count(*) FROM bench_c WHERE nk = 1000025;

SET constraint_exclusion = off;
SET table_range.enable_pruning = on;
SELECT count(*) FROM bench_c WHERE nk = 1000025;  -- warm
\echo '==== C: table_range pruning (CHECK exclusion off) ===='
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT count(*) FROM bench_c WHERE nk = 1000025;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT count(*) FROM bench_c WHERE nk = 1000025;

SET constraint_exclusion = off;
SET table_range.enable_pruning = off;
SELECT count(*) FROM bench_c WHERE nk = 1000025;  -- warm
\echo '==== C: no pruning (both off, scans all 2,000 partitions) ===='
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT count(*) FROM bench_c WHERE nk = 1000025;
DROP TABLE bench_c CASCADE;

-- ====================================================================================
-- 4. Insert cost: 100,000 rows into one partition, with and without a table_range index.
--    Ascending rows extend the summary on every row (like time-series appends); in-range
--    rows fall inside the existing bounds and write nothing to the index.
-- ====================================================================================
DROP TABLE IF EXISTS bench_d, bench_d_plain CASCADE;
CREATE TABLE bench_d (k int, n bigint, ts timestamptz) PARTITION BY LIST (k);
CREATE TABLE bench_d_1 PARTITION OF bench_d FOR VALUES IN (1);
CREATE TABLE bench_d_plain (LIKE bench_d) PARTITION BY LIST (k);
CREATE TABLE bench_d_plain_1 PARTITION OF bench_d_plain FOR VALUES IN (1);
INSERT INTO bench_d VALUES (1, 0, '2024-01-01'), (1, 1000000, '2025-01-01');
INSERT INTO bench_d_plain VALUES (1, 0, '2024-01-01'), (1, 1000000, '2025-01-01');
CREATE INDEX bench_d_tr ON bench_d USING table_range (n, ts);
CHECKPOINT;

\echo '==== D: 100k ascending rows, no index ===='
EXPLAIN (ANALYZE, WAL, COSTS OFF, TIMING OFF)
INSERT INTO bench_d_plain
SELECT 1, 1000000 + g, '2025-01-01'::timestamptz + g * interval '1 s'
FROM generate_series(1, 100000) g;
\echo '==== D: 100k ascending rows, table_range (widens every row) ===='
EXPLAIN (ANALYZE, WAL, COSTS OFF, TIMING OFF)
INSERT INTO bench_d
SELECT 1, 1000000 + g, '2025-01-01'::timestamptz + g * interval '1 s'
FROM generate_series(1, 100000) g;
\echo '==== D: 100k in-range rows, no index ===='
EXPLAIN (ANALYZE, WAL, COSTS OFF, TIMING OFF)
INSERT INTO bench_d_plain SELECT 1, g, '2024-06-01' FROM generate_series(1, 100000) g;
\echo '==== D: 100k in-range rows, table_range ===='
EXPLAIN (ANALYZE, WAL, COSTS OFF, TIMING OFF)
INSERT INTO bench_d SELECT 1, g, '2024-06-01' FROM generate_series(1, 100000) g;
DROP TABLE bench_d, bench_d_plain CASCADE;

-- ====================================================================================
-- 5. (Optional) The lock-table wall. At a few thousand partitions (once 2 x partitions
--    exceeds ~6,400 locks) a non-key predicate exhausts the lock table on default settings:
--      ERROR: out of shared memory
--      HINT:  You might need to increase "max_locks_per_transaction".
--    Raise max_locks_per_transaction (e.g. to 4096) and restart to push the wall out.
--    Native key pruning is unaffected because it never locks pruned partitions.
-- ====================================================================================

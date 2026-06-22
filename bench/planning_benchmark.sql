-- Benchmark for table_range pruning.
--
-- Measures end-to-end query time (planning + execution, warm) for a selective predicate
-- on a NON-partition-key column, with table_range pruning on vs. off. Native PostgreSQL
-- cannot prune on a non-key column, so without pruning the query scans every partition.
--
--   cargo pgrx run pg18
--   \i bench/planning_benchmark.sql
--
-- Pruning trades a small per-plan overhead (loading summaries + evaluating each
-- partition) for skipping the scan of non-matching partitions, so it wins when the
-- partitions it eliminates are large enough to outweigh that overhead.

\set part_count 100
\set rows_per_part 30000

DROP TABLE IF EXISTS bench_events CASCADE;
CREATE TABLE bench_events (region int, val bigint, pad text) PARTITION BY LIST (region);

SELECT format(
    'CREATE TABLE bench_events_%s PARTITION OF bench_events FOR VALUES IN (%s);', g, g)
FROM generate_series(1, :part_count) g \gexec

-- region is the partition key; val is a disjoint band per partition (the queried,
-- non-key column).
INSERT INTO bench_events
SELECT g, g * 1000000 + s, repeat('x', 50)
FROM generate_series(1, :part_count) g, generate_series(0, :rows_per_part - 1) s;

VACUUM ANALYZE bench_events;
CREATE INDEX bench_events_tr ON bench_events USING table_range (val);

\timing on

-- Warm the relation cache first so the numbers reflect steady state, not first-touch
-- partition-metadata loading (which both modes pay equally).
SET table_range.enable_pruning = on;
SELECT count(*) FROM bench_events WHERE val = 50000000;

\echo '==================== pruning ON (warm) ===================='
SELECT count(*) FROM bench_events WHERE val = 50000000;
SELECT count(*) FROM bench_events WHERE val = 50000000;

SET table_range.enable_pruning = off;
SELECT count(*) FROM bench_events WHERE val = 50000000;

\echo '==================== pruning OFF (warm) ===================='
SELECT count(*) FROM bench_events WHERE val = 50000000;
SELECT count(*) FROM bench_events WHERE val = 50000000;

DROP TABLE bench_events CASCADE;

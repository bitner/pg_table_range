-- Planning-time benchmark for table_range pruning.
--
-- Builds a wide partition tree where the queried column is NOT the partition key, so
-- native PostgreSQL pruning cannot help, and compares planning time + plan size with
-- table_range pruning off vs. on. Run with:
--
--   cargo pgrx run pg18
--   \i bench/planning_benchmark.sql
--
-- Look at the "Planning Time" line and the number of child plans in each EXPLAIN.

\set part_count 1000

DROP TABLE IF EXISTS bench_events CASCADE;
CREATE TABLE bench_events (region int, val bigint) PARTITION BY LIST (region);

-- One partition per region; each holds a disjoint 1000-wide band of `val`.
SELECT format(
    'CREATE TABLE bench_events_%s PARTITION OF bench_events FOR VALUES IN (%s);',
    g, g
)
FROM generate_series(1, :part_count) g \gexec

INSERT INTO bench_events
SELECT g, (g * 1000) + s
FROM generate_series(1, :part_count) g,
     generate_series(0, 49) s;

ANALYZE bench_events;

CREATE INDEX bench_events_tr ON bench_events USING table_range (val);

\echo '==================== pruning OFF ===================='
SET table_range.enable_pruning = off;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY ON)
SELECT * FROM bench_events WHERE val BETWEEN 500000 AND 500049;

\echo '==================== pruning ON  ===================='
SET table_range.enable_pruning = on;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY ON)
SELECT * FROM bench_events WHERE val BETWEEN 500000 AND 500049;

\echo '==================== correctness check (must match) ===================='
SET table_range.enable_pruning = off;
SELECT count(*) AS off_count FROM bench_events WHERE val BETWEEN 500000 AND 500049;
SET table_range.enable_pruning = on;
SELECT count(*) AS on_count FROM bench_events WHERE val BETWEEN 500000 AND 500049;

DROP TABLE bench_events CASCADE;

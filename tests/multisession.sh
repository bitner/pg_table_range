#!/usr/bin/env bash
# Tests that need more than one backend, which the single-transaction #[pg_test]
# harness cannot express: cross-backend invalidation, snapshots taken before an index
# build, and concurrent summary updates.
#
# Runs against a server that already has the extension installed. Connection settings
# come from the usual libpq environment (PGHOST, PGPORT, PGUSER); psql is taken from
# PATH. A scratch database is created and dropped.
#
#     PGHOST=localhost PGPORT=28818 tests/multisession.sh

set -euo pipefail

DB=table_range_multisession
PSQL=(psql -X -q -v ON_ERROR_STOP=1 -At)
failures=0

psql -X -q -d postgres -c "SET client_min_messages = warning" \
    -c "DROP DATABASE IF EXISTS $DB" -c "CREATE DATABASE $DB"
trap 'psql -X -q -d postgres -c "DROP DATABASE IF EXISTS $DB" >/dev/null 2>&1 || true' EXIT

q() { "${PSQL[@]}" -d "$DB" "$@"; }

# Compare a count with pruning on and off; record a failure on mismatch.
check() {
    local label=$1 from_where=$2 on off
    on=$(q -c "SET table_range.enable_pruning = on" -c "SELECT count(*) FROM $from_where")
    off=$(q -c "SET table_range.enable_pruning = off" -c "SELECT count(*) FROM $from_where")
    if [[ "$on" == "$off" ]]; then
        echo "ok   $label (rows=$on)"
    else
        echo "FAIL $label: pruning on=$on off=$off"
        failures=$((failures + 1))
    fi
}

q -c "CREATE EXTENSION pg_table_range"

# 1. A widening in a subtransaction that is rolled back, followed by another widening
#    in the parent, must still reach other backends that cached the older summary.
q <<'SQL'
CREATE TABLE sp (k int, n bigint) PARTITION BY LIST (k);
CREATE TABLE sp_1 PARTITION OF sp FOR VALUES IN (1);
INSERT INTO sp VALUES (1, 1);
CREATE INDEX sp_tr ON sp USING table_range (n);
SQL
# A long-lived backend that caches the summary before, and queries after, the widening.
coproc WATCHER { "${PSQL[@]}" -d "$DB"; }
echo "SELECT count(*) FROM sp WHERE n = 600;" >&"${WATCHER[1]}"
read -r -u "${WATCHER[0]}" before
q <<'SQL'
BEGIN;
SAVEPOINT s;
INSERT INTO sp VALUES (1, 500);
ROLLBACK TO s;
INSERT INTO sp VALUES (1, 600);
COMMIT;
SQL
echo "SELECT count(*) FROM sp WHERE n = 600;" >&"${WATCHER[1]}"
read -r -u "${WATCHER[0]}" after
exec {WATCHER[1]}>&-
wait "$WATCHER_PID" || true
if [[ "$before" == 0 && "$after" == 1 ]]; then
    echo "ok   other backend sees widening after savepoint rollback"
else
    echo "FAIL other backend after savepoint rollback: before=$before after=$after (want 0, 1)"
    failures=$((failures + 1))
fi

# 2. CREATE INDEX inside a REPEATABLE READ transaction must cover rows committed after
#    that transaction's snapshot was taken.
q <<'SQL'
CREATE TABLE rr (k int, n bigint) PARTITION BY LIST (k);
CREATE TABLE rr_1 PARTITION OF rr FOR VALUES IN (1);
INSERT INTO rr VALUES (1, 1);
SQL
coproc BUILDER { "${PSQL[@]}" -d "$DB"; }
printf '%s\n' "BEGIN ISOLATION LEVEL REPEATABLE READ;" "SELECT 1;" >&"${BUILDER[1]}"
read -r -u "${BUILDER[0]}" _
q -c "INSERT INTO rr VALUES (1, 777)"
printf '%s\n' "CREATE INDEX rr_tr ON rr USING table_range (n);" "COMMIT;" "SELECT 'built';" >&"${BUILDER[1]}"
read -r -u "${BUILDER[0]}" _
exec {BUILDER[1]}>&-
wait "$BUILDER_PID" || true
check "index built under REPEATABLE READ covers later commit" "rr WHERE n = 777"

# 3. Concurrent widenings must not overwrite each other. A range type over a domain
#    with a slow CHECK holds each insert between reading and writing the summary, so
#    the writers overlap deterministically.
q <<'SQL'
CREATE FUNCTION slow(int) RETURNS bool LANGUAGE sql AS 'SELECT pg_sleep(0.3) IS NOT NULL';
CREATE DOMAIN slowint AS int CHECK (slow(VALUE));
CREATE TYPE slowrange AS RANGE (subtype = slowint);
DO $$ BEGIN PERFORM table_range_sync_opclasses(); END $$;
CREATE TABLE cc (k int, r slowrange) PARTITION BY LIST (k);
CREATE TABLE cc_1 PARTITION OF cc FOR VALUES IN (1);
INSERT INTO cc VALUES (1, '[0,1)');
CREATE INDEX cc_tr ON cc USING table_range (r);
SQL
pids=()
retries=$(mktemp)
for i in 1 2 3; do
    for v in "[$((i * 100)),$((i * 100 + 1)))" "[-$((i * 100)),-$((i * 100 - 1)))"; do
        q -c "SET client_min_messages = debug1" -c "INSERT INTO cc VALUES (1, '$v')" 2>>"$retries" &
        pids+=($!)
    done
done
wait "${pids[@]}"
for v in 100 -100 200 -200 300 -300; do
    check "concurrent widening to $v kept" "cc WHERE r && '[$v,$((v + 1)))'::slowrange"
done
if grep -q "summary changed concurrently" "$retries"; then
    echo "ok   the writers raced (at least one compare-and-swap retried)"
else
    echo "FAIL the writers never raced, so the race was not tested"
    failures=$((failures + 1))
fi
rm -f "$retries"

# Assert how many partitions a query scans with pruning on.
expect_scans() {
    local label=$1 from_where=$2 want=$3 got
    got=$(q -c "SET table_range.enable_pruning = on" \
        -c "EXPLAIN (COSTS OFF) SELECT * FROM $from_where" |
        grep "Scan on" | grep -vc "Bitmap Index Scan" || true)
    if [[ "$got" == "$want" ]]; then
        echo "ok   $label (scans $got)"
    else
        echo "FAIL $label: scans $got partitions, want $want"
        failures=$((failures + 1))
    fi
}

# 4. A bulk load keeps the partition pruning. A column made unbounded by an oversized
#    value is restored by VACUUM once that row is deleted, and table_range_summarize
#    re-tightens bounds once deleted rows are vacuumed away. Autovacuum is off on these
#    tables so its snapshots cannot keep deleted rows visible.
q <<'SQL'
CREATE TABLE vb (k int, n bigint, s text) PARTITION BY LIST (k);
CREATE TABLE vb_1 PARTITION OF vb FOR VALUES IN (1) WITH (autovacuum_enabled = false);
CREATE TABLE vb_2 PARTITION OF vb FOR VALUES IN (2) WITH (autovacuum_enabled = false);
INSERT INTO vb VALUES (1, 0, 'm'), (2, 1000, 'm');
CREATE INDEX vb_tr ON vb USING table_range (n, s);
INSERT INTO vb SELECT 1, g, 'm' FROM generate_series(1, 200) g;
SQL
expect_scans "bulk-loaded partition still prunes" "vb WHERE n = 5000" 0
q -c "INSERT INTO vb VALUES (1, 50, 'z' || repeat('x', 9000))"
expect_scans "oversized value keeps its partition" "vb WHERE s = 'zz'" 1
q -c "DELETE FROM vb WHERE length(s) > 100"
q -c "VACUUM vb"
expect_scans "VACUUM restores the bounds once it is deleted" "vb WHERE s = 'zz'" 0
q -c "DELETE FROM vb WHERE k = 1 AND n > 100"
q -c "VACUUM vb"
q -c "SELECT table_range_summarize('vb_tr'::regclass)" >/dev/null
expect_scans "table_range_summarize re-tightens after deletes" "vb WHERE n = 150" 0
check "rows after re-summarizing" "vb WHERE n BETWEEN 0 AND 200"

# 5. Rows inserted while a re-summarization scans the heap must be covered, including
#    rows that land on pages the scan has already passed. The scan evaluates gate() for
#    every row (an expression column, not summarized) and blocks at n = 600 while another
#    backend holds an advisory lock. Space freed on the first page routes the inserts
#    behind the scan.
q <<'SQL'
CREATE FUNCTION gate(bigint) RETURNS bigint LANGUAGE plpgsql IMMUTABLE AS $$
BEGIN
    IF $1 = 600 THEN
        PERFORM pg_advisory_lock_shared(4242);
        PERFORM pg_advisory_unlock_shared(4242);
    END IF;
    RETURN $1;
END $$;
CREATE TABLE rs (k int, n bigint) PARTITION BY LIST (k);
CREATE TABLE rs_1 PARTITION OF rs FOR VALUES IN (1) WITH (autovacuum_enabled = false);
INSERT INTO rs SELECT 1, g FROM generate_series(1, 1000) g;
CREATE INDEX rs_tr ON rs USING table_range (n, (gate(n)));
DELETE FROM rs WHERE n <= 150;
VACUUM rs;
SQL
coproc GATE { "${PSQL[@]}" -d "$DB"; }
printf '%s\n' "SELECT pg_advisory_lock(4242);" "SELECT 'held';" >&"${GATE[1]}"
read -r -u "${GATE[0]}" _; read -r -u "${GATE[0]}" _
q -c "SELECT table_range_summarize('rs_tr'::regclass)" >/dev/null &
summarizer=$!
for _ in $(seq 100); do
    waiting=$(q -c "SELECT count(*) FROM pg_stat_activity
                    WHERE query LIKE 'SELECT table_range_summarize%' AND wait_event = 'advisory'")
    [[ "$waiting" == 1 ]] && break
    sleep 0.1
done
# One statement: the free space map rotates where each new search starts.
q -c "INSERT INTO rs VALUES (1, 50000), (1, -50000)"
behind=$(q -c "SELECT bool_and((ctid::text::point)[0] < (SELECT (ctid::text::point)[0] FROM rs WHERE n = 600))
               FROM rs WHERE n IN (50000, -50000)")
if [[ "$waiting" == 1 && "$behind" == t ]]; then
    echo "ok   inserts landed behind the paused re-summarization scan"
else
    echo "FAIL precondition: scan paused=$waiting, inserts behind it=$behind"
    failures=$((failures + 1))
fi
expect_scans "a summary being rebuilt never prunes" "rs WHERE n = 999999" 1
printf '%s\n' "SELECT pg_advisory_unlock(4242);" >&"${GATE[1]}"
exec {GATE[1]}>&-
wait "$summarizer"
check "insert above the range during re-summarization" "rs WHERE n = 50000"
check "insert below the range during re-summarization" "rs WHERE n = -50000"
expect_scans "re-summarized partition prunes" "rs WHERE n = 999999" 0

# 6. Re-summarizing keeps row versions older snapshots can still see, including ones a
#    HOT update replaced.
q <<'SQL'
CREATE TABLE ho (k int, x int, pad int) PARTITION BY LIST (k);
CREATE TABLE ho_1 PARTITION OF ho FOR VALUES IN (1) WITH (autovacuum_enabled = false);
INSERT INTO ho VALUES (1, 5, 0);
CREATE INDEX ho_tr ON ho USING table_range (x);
SQL
coproc OLD { "${PSQL[@]}" -d "$DB"; }
printf '%s\n' "BEGIN ISOLATION LEVEL REPEATABLE READ;" "SELECT count(*) FROM ho WHERE x = 5;" >&"${OLD[1]}"
read -r -u "${OLD[0]}" _
q -c "UPDATE ho SET x = 100"
q -c "SELECT table_range_summarize('ho_tr'::regclass)" >/dev/null
printf '%s\n' "SELECT count(*) FROM ho WHERE x = 5;" "COMMIT;" >&"${OLD[1]}"
read -r -u "${OLD[0]}" seen
exec {OLD[1]}>&-
wait "$OLD_PID" || true
if [[ "$seen" == 1 ]]; then
    echo "ok   older snapshot still finds the row after a HOT update and re-summarize"
else
    echo "FAIL older snapshot after HOT update and re-summarize: rows=$seen, want 1"
    failures=$((failures + 1))
fi

# 7. A widening whose transaction rolls back still reaches other backends: the page stays
#    widened, so a later insert of the same value writes nothing and sends nothing.
q <<'SQL'
CREATE TABLE rb (k int, x int) PARTITION BY LIST (k);
CREATE TABLE rb_1 PARTITION OF rb FOR VALUES IN (1);
INSERT INTO rb SELECT 1, g FROM generate_series(0, 100) g;
CREATE INDEX rb_tr ON rb USING table_range (x);
SQL
coproc WATCH { "${PSQL[@]}" -d "$DB"; }
echo "SELECT count(*) FROM rb WHERE x = 500;" >&"${WATCH[1]}"
read -r -u "${WATCH[0]}" _
q -c "BEGIN" -c "INSERT INTO rb VALUES (1, 500)" -c "ROLLBACK"
q -c "INSERT INTO rb VALUES (1, 500)"
echo "SELECT count(*) FROM rb WHERE x = 500;" >&"${WATCH[1]}"
read -r -u "${WATCH[0]}" seen
exec {WATCH[1]}>&-
wait "$WATCH_PID" || true
if [[ "$seen" == 1 ]]; then
    echo "ok   other backend sees a value first widened by a rolled-back insert"
else
    echo "FAIL rolled-back widening: other backend rows=$seen, want 1"
    failures=$((failures + 1))
fi

# 8. VACUUM rebuilds an unbounded summary only when it runs index cleanup outside parallel
#    mode: VACUUM (PARALLEL n) and INDEX_CLEANUP OFF leave it for a later VACUUM or
#    table_range_summarize. Two btree indexes make VACUUM (PARALLEL 2) actually parallel.
q <<'SQL'
CREATE TABLE pv (k int, n bigint, s text) PARTITION BY LIST (k);
CREATE TABLE pv_1 PARTITION OF pv FOR VALUES IN (1) WITH (autovacuum_enabled = false);
INSERT INTO pv SELECT 1, g, 'm' FROM generate_series(1, 20000) g;
CREATE INDEX pv_n ON pv_1 (n);
CREATE INDEX pv_s ON pv_1 (s);
CREATE INDEX pv_tr ON pv USING table_range (s);
INSERT INTO pv VALUES (1, 0, 'z' || repeat('x', 9000));
DELETE FROM pv WHERE length(s) > 100;
SQL
verbose=$(q -c "SET min_parallel_index_scan_size = 0" -c "VACUUM (PARALLEL 2, VERBOSE) pv" 2>&1)
if grep -q "parallel vacuum worker" <<<"$verbose"; then
    expect_scans "VACUUM (PARALLEL 2) leaves the summary unbounded" "pv WHERE s = 'zz'" 1
else
    echo "FAIL VACUUM (PARALLEL 2) did not run in parallel, so the case was not tested"
    failures=$((failures + 1))
fi
q -c "VACUUM (INDEX_CLEANUP OFF) pv"
expect_scans "VACUUM (INDEX_CLEANUP OFF) leaves the summary unbounded" "pv WHERE s = 'zz'" 1
q -c "VACUUM pv"
expect_scans "a non-parallel VACUUM rebuilds it" "pv WHERE s = 'zz'" 0

if ((failures > 0)); then
    echo "$failures multi-session check(s) failed"
    exit 1
fi
echo "all multi-session checks passed"

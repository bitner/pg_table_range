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
for i in 1 2 3; do
    q -c "INSERT INTO cc VALUES (1, '[$((i * 100)),$((i * 100 + 1)))')" &
    pids+=($!)
    q -c "INSERT INTO cc VALUES (1, '[-$((i * 100)),-$((i * 100 - 1)))')" &
    pids+=($!)
done
wait "${pids[@]}"
for v in 100 -100 200 -200 300 -300; do
    check "concurrent widening to $v kept" "cc WHERE r && '[$v,$((v + 1)))'::slowrange"
done

# Assert how many partitions a query scans with pruning on.
expect_scans() {
    local label=$1 from_where=$2 want=$3 got
    got=$(q -c "SET table_range.enable_pruning = on" \
        -c "EXPLAIN (COSTS OFF) SELECT * FROM $from_where" | grep -c "Scan on" || true)
    if [[ "$got" == "$want" ]]; then
        echo "ok   $label (scans $got)"
    else
        echo "FAIL $label: scans $got partitions, want $want"
        failures=$((failures + 1))
    fi
}

# 4. A bulk load keeps the partition pruning. A column made unbounded by an oversized
#    value is restored by VACUUM once that row is deleted, and table_range_summarize
#    re-tightens bounds after committed deletes.
q <<'SQL'
CREATE TABLE vb (k int, n bigint, s text) PARTITION BY LIST (k);
CREATE TABLE vb_1 PARTITION OF vb FOR VALUES IN (1);
CREATE TABLE vb_2 PARTITION OF vb FOR VALUES IN (2);
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
q -c "SELECT table_range_summarize('vb_tr'::regclass)" >/dev/null
expect_scans "table_range_summarize re-tightens after deletes" "vb WHERE n = 150" 0
check "rows after re-summarizing" "vb WHERE n BETWEEN 0 AND 200"

# 5. Rows inserted while a re-summarization scans the heap must be covered, including
#    rows that land on pages the scan has already passed. The expression column is not
#    summarized; it only makes every scan slow (~2 ms per row). Freed space on the first
#    pages routes the concurrent inserts behind the scan.
q <<'SQL'
CREATE FUNCTION slow_id(bigint) RETURNS bigint LANGUAGE plpgsql IMMUTABLE
    AS $$ BEGIN PERFORM pg_sleep(0.002); RETURN $1; END $$;
CREATE TABLE rs (k int, n bigint) PARTITION BY LIST (k);
CREATE TABLE rs_1 PARTITION OF rs FOR VALUES IN (1);
INSERT INTO rs SELECT 1, g FROM generate_series(1, 1000) g;
CREATE INDEX rs_tr ON rs USING table_range (n, (slow_id(n)));
DELETE FROM rs WHERE n <= 220;
VACUUM rs;
INSERT INTO rs SELECT 1, g FROM generate_series(2000, 2100) g;
SQL
q -c "SELECT table_range_summarize('rs_tr'::regclass)" >/dev/null &
summarizer=$!
sleep 0.8
q -c "INSERT INTO rs VALUES (1, 50000)"
q -c "INSERT INTO rs VALUES (1, -50000)"
wait "$summarizer"
check "insert above the range during re-summarization" "rs WHERE n = 50000"
check "insert below the range during re-summarization" "rs WHERE n = -50000"
expect_scans "re-summarized partition prunes" "rs WHERE n = 999999" 0

if ((failures > 0)); then
    echo "$failures multi-session check(s) failed"
    exit 1
fi
echo "all multi-session checks passed"

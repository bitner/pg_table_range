#!/usr/bin/env bash
# Hot standby tests: summary changes on a primary must reach queries on a streaming
# standby. Creates a throwaway primary and standby in a temporary directory, using the
# PostgreSQL binaries on PATH (with the extension installed).
#
#     PATH="$(dirname "$(cargo pgrx info pg-config pg18)"):$PATH" tests/standby.sh

set -euo pipefail

DIR=$(mktemp -d "${TMPDIR:-/tmp}/trsb.XXXXXX")
PRIMARY_PORT=${PRIMARY_PORT:-54401}
STANDBY_PORT=${STANDBY_PORT:-54402}
failures=0

cleanup() {
    pg_ctl -D "$DIR/standby" stop -m immediate >/dev/null 2>&1 || true
    pg_ctl -D "$DIR/primary" stop -m immediate >/dev/null 2>&1 || true
    rm -rf "$DIR"
}
trap cleanup EXIT

PSQL=(psql -X -q -v ON_ERROR_STOP=1 -At -h "$DIR" -U postgres -d postgres)
p() { "${PSQL[@]}" -p "$PRIMARY_PORT" "$@"; }
s() { "${PSQL[@]}" -p "$STANDBY_PORT" "$@"; }

initdb -D "$DIR/primary" -U postgres -A trust >/dev/null
cat >>"$DIR/primary/postgresql.conf" <<EOF
port = $PRIMARY_PORT
listen_addresses = ''
unix_socket_directories = '$DIR'
wal_level = replica
autovacuum = off
EOF
pg_ctl -D "$DIR/primary" -l "$DIR/primary.log" start >/dev/null
p -c "CREATE EXTENSION pg_table_range"
pg_basebackup -h "$DIR" -p "$PRIMARY_PORT" -U postgres -D "$DIR/standby" -R -X stream
cat >>"$DIR/standby/postgresql.conf" <<EOF
port = $STANDBY_PORT
hot_standby_feedback = off
max_standby_streaming_delay = 0
EOF
pg_ctl -D "$DIR/standby" -l "$DIR/standby.log" start >/dev/null

# Wait until the standby has replayed everything the primary has logged so far, by waiting
# for a marker record. (The insert position itself can sit past a page header no record
# ends at. A transaction without an xid, like table_range_summarize, does not flush WAL at
# commit; the WAL writer does shortly after.)
caught_up() {
    local lsn
    lsn=$(p -c "SELECT pg_logical_emit_message(false, 'table_range', '')")
    for _ in $(seq 100); do
        [[ $(s -c "SELECT pg_last_wal_replay_lsn() >= '$lsn'") == t ]] && return
        sleep 0.1
    done
    echo "standby did not reach $lsn (replayed $(s -c "SELECT pg_last_wal_replay_lsn()"))" >&2
    tail -n 20 "$DIR/standby.log" >&2
    exit 1
}

expect() {
    local label=$1 got=$2 want=$3
    if [[ "$got" == "$want" ]]; then
        echo "ok   $label"
    else
        echo "FAIL $label: got $got, want $want"
        failures=$((failures + 1))
    fi
}

p <<'SQL'
CREATE TABLE t (k int, x int) PARTITION BY LIST (k);
CREATE TABLE t_1 PARTITION OF t FOR VALUES IN (1);
INSERT INTO t SELECT 1, g FROM generate_series(0, 100) g;
CREATE INDEX t_tr ON t USING table_range (x);
SQL
caught_up

# One long-lived standby backend, so its cached summaries and plans are what is tested.
coproc READER { "${PSQL[@]}" -p "$STANDBY_PORT"; }
ask() {
    echo "$1" >&"${READER[1]}"
    read -r -t 10 -u "${READER[0]}" answer
    echo "$answer"
}
ask "PREPARE q500 AS SELECT count(*) FROM t WHERE x = 500;" >/dev/null || true
expect "standby prunes before the widening" "$(ask "EXECUTE q500;")" 0
expect "same (cached)" "$(ask "SELECT count(*) FROM t WHERE x = 600;")" 0

# 1. A committed widening reaches the standby's cached summary and cached plan.
p -c "INSERT INTO t VALUES (1, 500)"
caught_up
expect "committed widening reaches a cached plan on the standby" "$(ask "EXECUTE q500;")" 1

# 2. A widening whose transaction rolls back still reaches the standby, so a later
#    covered insert (which writes nothing) is found there.
p -c "BEGIN" -c "INSERT INTO t VALUES (1, 600)" -c "ROLLBACK"
p -c "INSERT INTO t VALUES (1, 600)"
caught_up
expect "rolled-back widening reaches the standby" "$(ask "SELECT count(*) FROM t WHERE x = 600;")" 1
exec {READER[1]}>&-
wait "${READER_PID:-}" || true

# 3. Re-summarizing without VACUUM keeps deleted rows that a standby snapshot still sees.
coproc OLD { "${PSQL[@]}" -p "$STANDBY_PORT" -v ON_ERROR_STOP=0; }
printf '%s\n' "BEGIN ISOLATION LEVEL REPEATABLE READ;" "SELECT count(*) FROM t WHERE x = 600;" >&"${OLD[1]}"
read -r -t 10 -u "${OLD[0]}" _
# Deleting the top of the range (500 and 600) is what lets a rebuild narrow the bounds.
p -c "DELETE FROM t WHERE x > 100" -c "SELECT table_range_summarize('t_tr'::regclass)" >/dev/null
caught_up
echo "SELECT count(*) FROM t WHERE x = 600;" >&"${OLD[1]}"
read -r -t 10 -u "${OLD[0]}" seen || seen=none
expect "standby snapshot keeps a deleted row across re-summarizing" "$seen" 1

# 4. Once VACUUM removes the row, the old standby snapshot is cancelled by the recovery
#    conflict before the narrowed summary is replayed; it never silently loses the row.
p -c "VACUUM t" -c "SELECT table_range_summarize('t_tr'::regclass)" >/dev/null
caught_up
{ echo "SELECT count(*) FROM t WHERE x = 600;" >&"${OLD[1]}"; } 2>/dev/null || true
read -r -t 10 -u "${OLD[0]}" seen || seen=cancelled
if [[ "$seen" == 1 || "$seen" == cancelled ]]; then
    echo "ok   after VACUUM the standby snapshot sees the row or is cancelled ($seen)"
else
    echo "FAIL after VACUUM the standby snapshot got $seen rows"
    failures=$((failures + 1))
fi
# The coprocess may already be gone (terminated by the recovery conflict).
[[ -n "${OLD[1]:-}" ]] && exec {OLD[1]}>&-
wait "${OLD_PID:-}" 2>/dev/null || true
expect "narrowed summary prunes on the standby" \
    "$(s -c "EXPLAIN (COSTS OFF) SELECT * FROM t WHERE x = 600" | grep -c "Scan on" || true)" 0

if ((failures > 0)); then
    tail -n 20 "$DIR/standby.log"
    echo "$failures standby check(s) failed"
    exit 1
fi
echo "all standby checks passed"

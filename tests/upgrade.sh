#!/usr/bin/env bash
# Upgrade test: indexes built by an earlier release stay correct when this build's library
# is loaded, both before and after ALTER EXTENSION UPDATE; VACUUM leaves them all pruning;
# and the updated extension matches a fresh install. Builds OLD_REF from a git worktree, installs it into
# the PostgreSQL that PG_CONFIG names, creates a throwaway cluster, then installs this
# checkout over it and restarts.
#
#     tests/upgrade.sh "$(cargo pgrx info pg-config pg18)" [OLD_REF]

set -euo pipefail

PG_CONFIG=$1
OLD_REF=${2:-v0.1.2}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
MAJOR=$("$PG_CONFIG" --version | sed -E 's/^PostgreSQL ([0-9]+).*/\1/')
PATH="$("$PG_CONFIG" --bindir):$PATH"
DIR=$(mktemp -d "${TMPDIR:-/tmp}/trup.XXXXXX")
PORT=${PORT:-54403}
OLD_SRC="$DIR/old"
failures=0

cleanup() {
    pg_ctl -D "$DIR/data" stop -m immediate >/dev/null 2>&1 || true
    git -C "$ROOT" worktree remove --force "$OLD_SRC" >/dev/null 2>&1 || true
    rm -rf "$DIR"
}
trap cleanup EXIT

install() {
    # A target dir per source tree, so neither build invalidates the other's cache.
    (cd "$1" && CARGO_TARGET_DIR="$2" cargo pgrx install \
        --no-default-features --features "pg$MAJOR" --pg-config "$PG_CONFIG" >"$DIR/install.log" 2>&1) \
        || { tail -n 30 "$DIR/install.log"; exit 1; }
}

q() { psql -X -q -v ON_ERROR_STOP=1 -At -h "$DIR" -p "$PORT" -U postgres "$@"; }

expect() {
    local label=$1 got=$2 want=$3
    if [[ "$got" == "$want" ]]; then
        echo "ok   $label"
    else
        echo "FAIL $label: got $got, want $want"
        failures=$((failures + 1))
    fi
}

# Partitions scanned by a query.
scans() { q -d upgraded -c "EXPLAIN (COSTS OFF) $1" | grep -c "Scan on" || true; }

# What the extension consists of, to compare an updated install with a fresh one.
members() {
    q -d "$1" <<'SQL'
-- Operator classes are named after type oids, which differ between databases.
SELECT regexp_replace(pg_describe_object(classid, objid, 0), 'tr_[0-9]+', 'tr_')
    || coalesce(' ' || (SELECT opcintype::regtype::text FROM pg_opclass
                        WHERE classid = 'pg_opclass'::regclass AND oid = objid), '')
    || coalesce(' ' || (SELECT string_agg(opcintype::regtype::text, ',') FROM pg_opclass
                        WHERE classid = 'pg_opfamily'::regclass AND opcfamily = objid), '')
    || coalesce(' ' || pg_get_functiondef(objid), '')
FROM pg_depend
WHERE refclassid = 'pg_extension'::regclass
  AND refobjid = (SELECT oid FROM pg_extension WHERE extname = 'pg_table_range')
  AND deptype = 'e'
ORDER BY 1;
SQL
}

git -C "$ROOT" worktree add --detach "$OLD_SRC" "$OLD_REF" >/dev/null 2>&1
install "$OLD_SRC" "$ROOT/target/upgrade-old"

initdb -D "$DIR/data" -U postgres -A trust >/dev/null
cat >>"$DIR/data/postgresql.conf" <<EOF
port = $PORT
listen_addresses = ''
unix_socket_directories = '$DIR'
autovacuum = off
EOF
pg_ctl -D "$DIR/data" -l "$DIR/server.log" start >/dev/null

q -d postgres -c "CREATE DATABASE upgraded"
# One column per kind of summary; geometry only where PostGIS is installed.
geo=$(q -d postgres -c "SELECT count(*) FROM pg_available_extensions WHERE name = 'postgis'")
cols="x int, s text, ts timestamptz, r int4range"
row="g, 'p' || lpad(g::text, 5, '0'), '2024-01-01'::timestamptz + g * interval '1 day', int4range(g, g + 10)"
indexed="x, s, ts, r"
# Values only in partition 2, and values in neither partition (the widening row).
in_2=("x = 1050" "s = 'p01050'" "ts = '2026-11-16'" "r && int4range(1050, 1051)")
outside=("x = 5000" "s = 'zz'" "ts = '2040-01-01'" "r && int4range(5000, 5001)")
new_row="1, 5000, 'zz', '2040-01-01', int4range(5000, 5001)"
if ((geo > 0)); then
    q -d upgraded -c "CREATE EXTENSION postgis"
    cols+=", g geometry"
    row+=", ST_MakePoint(g, g)"
    indexed+=", g"
    in_2+=("g && ST_MakeEnvelope(1049, 1049, 1051, 1051)")
    outside+=("g && ST_MakeEnvelope(4999, 4999, 5001, 5001)")
    new_row+=", ST_MakePoint(5000, 5000)"
fi
q -d upgraded <<SQL
CREATE EXTENSION pg_table_range;
CREATE TABLE t (k int, $cols) PARTITION BY LIST (k);
CREATE TABLE t_1 PARTITION OF t FOR VALUES IN (1);
CREATE TABLE t_2 PARTITION OF t FOR VALUES IN (2);
INSERT INTO t SELECT 1, $row FROM generate_series(0, 99) g;
INSERT INTO t SELECT 2, $row FROM generate_series(1000, 1099) g;
CREATE INDEX t_tr ON t USING table_range ($indexed);
SQL
old_version=$(q -d upgraded -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_table_range'")
echo "built summaries with $old_version (PostGIS: $geo)"
# 0.1.1's on-page format is never pruned with, until VACUUM or a re-summarize rebuilds it.
old_scans=1
[[ "$old_version" == 0.1.1 ]] && old_scans=2

install "$ROOT" "$ROOT/target"
pg_ctl -D "$DIR/data" -l "$DIR/server.log" restart >/dev/null
# Read the old text bounds under a different date format than they were written in.
export PGOPTIONS="-c DateStyle=SQL,DMY"

# The new library on the old catalog: what runs between the restart and the update.
for w in "${in_2[@]}"; do
    expect "old summary: $w" "$(scans "SELECT * FROM t WHERE $w")" "$old_scans"
done
q -d upgraded -c "VACUUM t"
for w in "${in_2[@]}"; do
    expect "after VACUUM every summary prunes: $w" "$(scans "SELECT * FROM t WHERE $w")" 1
done

q -d upgraded -c "INSERT INTO t VALUES ($new_row)"
for w in "${outside[@]}"; do
    expect "insert widens a summary: $w" "$(q -d upgraded -c "SELECT count(*) FROM t WHERE $w")" 1
done

q -d upgraded -c "ALTER EXTENSION pg_table_range UPDATE"
expect "updated to the default version" \
    "$(q -d upgraded -c "SELECT extversion = default_version FROM pg_extension, pg_available_extensions WHERE extname = name AND name = 'pg_table_range'")" t
q -d postgres -c "CREATE DATABASE fresh"
# Same types available, since the extension creates an operator class for each.
((geo > 0)) && q -d fresh -c "CREATE EXTENSION postgis"
q -d fresh -c "CREATE EXTENSION pg_table_range"
if diff <(members upgraded) <(members fresh); then
    echo "ok   updated extension matches a fresh install"
else
    echo "FAIL updated extension differs from a fresh install (diff above)"
    failures=$((failures + 1))
fi

# VACUUM first: re-summarizing keeps every row version that still exists.
q -d upgraded -c "DELETE FROM t WHERE x = 5000" -c "VACUUM t"
expect "summarize rebuilds every old summary" \
    "$(q -d upgraded -c "SELECT table_range_summarize('t_tr'::regclass)")" 2
for w in "${outside[@]}"; do
    expect "rebuilt summary prunes the deleted value: $w" "$(scans "SELECT * FROM t WHERE $w")" 0
done
for w in "${in_2[@]}"; do
    expect "rebuilt summary keeps the data: $w" "$(scans "SELECT * FROM t WHERE $w")" 1
done

if ((failures > 0)); then
    tail -n 20 "$DIR/server.log"
    echo "$failures upgrade check(s) failed"
    exit 1
fi
echo "all upgrade checks passed"

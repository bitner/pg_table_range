#!/usr/bin/env bash
# Upgrade test: indexes built by an earlier release keep pruning and widening when this
# build's library is loaded, both before and after ALTER EXTENSION UPDATE, and the updated
# extension matches a fresh install. Builds OLD_REF from a git worktree, installs it into
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
    (cd "$1" && CARGO_TARGET_DIR="$2" cargo pgrx install --release \
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
SELECT pg_describe_object(classid, objid, 0) || coalesce(' ' || pg_get_functiondef(objid), '')
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
q -d upgraded <<'SQL'
CREATE EXTENSION pg_table_range;
CREATE TABLE t (k int, x int) PARTITION BY LIST (k);
CREATE TABLE t_1 PARTITION OF t FOR VALUES IN (1);
CREATE TABLE t_2 PARTITION OF t FOR VALUES IN (2);
INSERT INTO t SELECT 1, g FROM generate_series(0, 99) g;
INSERT INTO t SELECT 2, g FROM generate_series(1000, 1099) g;
CREATE INDEX t_tr ON t USING table_range (x);
SQL
old_version=$(q -d upgraded -c "SELECT extversion FROM pg_extension WHERE extname = 'pg_table_range'")
echo "built summaries with $old_version"

install "$ROOT" "$ROOT/target"
pg_ctl -D "$DIR/data" -l "$DIR/server.log" restart >/dev/null

# The new library on the old catalog: what runs between the restart and the update.
expect "old summary prunes with the new library" "$(scans "SELECT * FROM t WHERE x = 50")" 1
q -d upgraded -c "INSERT INTO t VALUES (1, 5000)"
expect "insert widens an old summary" "$(q -d upgraded -c "SELECT count(*) FROM t WHERE x = 5000")" 1

q -d upgraded -c "ALTER EXTENSION pg_table_range UPDATE"
expect "updated to the default version" \
    "$(q -d upgraded -c "SELECT extversion = default_version FROM pg_extension, pg_available_extensions WHERE extname = name AND name = 'pg_table_range'")" t
q -d postgres -c "CREATE DATABASE fresh"
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
expect "rebuilt summary prunes the deleted value" "$(scans "SELECT * FROM t WHERE x = 5000")" 0
expect "rebuilt summary keeps the data" "$(scans "SELECT * FROM t WHERE x = 1050")" 1

if ((failures > 0)); then
    tail -n 20 "$DIR/server.log"
    echo "$failures upgrade check(s) failed"
    exit 1
fi
echo "all upgrade checks passed"

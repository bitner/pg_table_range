#!/bin/bash
# Runs once, on first cluster init (via /docker-entrypoint-initdb.d), to make the bundled
# extensions immediately usable in the default database. Other databases can still
# `CREATE EXTENSION` on demand.
set -e

psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" <<-'EOSQL'
	CREATE EXTENSION IF NOT EXISTS postgis;
	CREATE EXTENSION IF NOT EXISTS pg_table_range;
EOSQL

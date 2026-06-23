# PostgreSQL 18 with PostGIS and pg_table_range, built on the official postgres image.
#
# The pg18 .deb (built for Debian bookworm, so it is glibc-compatible with this base) is
# expected in ./docker-deb — CI downloads it from the Debian-package build job. To build
# locally:  cargo pgrx package --features pg18 --pg-config <pgdg-18> && packaging/build-deb.sh 18
#           && mkdir -p docker-deb && cp target/deb/postgresql-18-*.deb docker-deb/
#           && docker build -f packaging/Dockerfile -t pg_table_range:18 .
FROM postgres:18-bookworm

COPY docker-deb/*.deb /tmp/
COPY packaging/initdb-extensions.sh /docker-entrypoint-initdb.d/10-extensions.sh

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends postgresql-18-postgis-3; \
    apt-get install -y --no-install-recommends /tmp/postgresql-18-pg-table-range_*.deb; \
    rm -rf /var/lib/apt/lists/* /tmp/*.deb

LABEL org.opencontainers.image.source="https://github.com/bitner/pg_table_range" \
      org.opencontainers.image.description="PostgreSQL 18 + PostGIS + pg_table_range" \
      org.opencontainers.image.licenses="MIT"

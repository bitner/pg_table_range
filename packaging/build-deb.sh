#!/usr/bin/env bash
# Build a PGDG-style Debian package from a `cargo pgrx package` tree.
#
#   packaging/build-deb.sh <pg_major>          # e.g. packaging/build-deb.sh 18
#
# Expects `cargo pgrx package --features pg<major> --pg-config <pgdg pg_config>` to have
# already produced target/release/pg_table_range-pg<major>/usr/... . Emits
# target/deb/postgresql-<major>-pg-table-range_<version>-1_<arch>.deb, which installs the
# .so and control/SQL files into the apt.postgresql.org (PGDG) PostgreSQL of that major.
set -euo pipefail

MAJ="${1:?usage: build-deb.sh <pg_major>}"
VER="$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -1)"
ARCH="$(dpkg --print-architecture)"
TREE="target/release/pg_table_range-pg${MAJ}"
PKGDIR="target/deb/postgresql-${MAJ}-pg-table-range"
DEB="target/deb/postgresql-${MAJ}-pg-table-range_${VER}-1_${ARCH}.deb"

[ -d "$TREE/usr" ] || { echo "missing $TREE/usr — run 'cargo pgrx package' first" >&2; exit 1; }

rm -rf "$PKGDIR"
mkdir -p "$PKGDIR/DEBIAN"
cp -r "$TREE/usr" "$PKGDIR/"

cat > "$PKGDIR/DEBIAN/control" <<EOF
Package: postgresql-${MAJ}-pg-table-range
Version: ${VER}-1
Architecture: ${ARCH}
Maintainer: David Bitner <bitner@dbspatial.com>
Depends: postgresql-${MAJ}
Section: database
Priority: optional
Homepage: https://github.com/bitner/pg_table_range
Description: Planning-time partition pruning by per-partition data ranges
 Prunes partitions at planning time from a compact per-partition summary of
 each column's data (btree min/max, or a covering extent for range types and
 PostGIS geometry), including non-key columns. Built with pgrx for
 PostgreSQL ${MAJ}.
EOF

mkdir -p target/deb
dpkg-deb --build --root-owner-group "$PKGDIR" "$DEB"
echo "built $DEB"

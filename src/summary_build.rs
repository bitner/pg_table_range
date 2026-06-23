use crate::index_storage::{ColSummary, IndexSummary};
use pgrx::prelude::*;
use pgrx::spi::SpiError;

// Computes a leaf partition's summary by scanning its data. Called by the index access
// method's `ambuild` (see `index_am.rs`); the result is persisted into the index's own
// metapage (see `index_storage.rs`) and maintained incrementally by `aminsert`. There is
// no side table — the summary lives in the index, like BRIN.

fn oid_u32(oid: pg_sys::Oid) -> u32 {
    oid.into()
}

/// Build the summary for a single leaf relation's named columns by scanning its data.
/// Returns the typed summary that `ambuild` persists into the index's own metapage.
pub(crate) fn build_one_leaf(
    leaf: pg_sys::Oid,
    columns: &[String],
) -> Result<IndexSummary, SpiError> {
    let leaf_name = match relation_name(leaf)? {
        Some(n) => n,
        None => return Ok(IndexSummary::default()),
    };

    let mut summary = IndexSummary::default();
    for col in columns {
        // Resolve this leaf's attnum for the column by name.
        let attnum = match leaf_attnum(leaf, col)? {
            Some(a) => a,
            None => continue, // column absent on this leaf; KEEP behavior
        };

        let (kind, type_name) = column_kind(leaf, col)?;
        let qcol = quote_ident(col);
        let (e1, e2) = kind.exprs(&qcol);

        let stats_sql =
            format!("SELECT count(*)::bigint, count({qcol})::bigint, {e1}, {e2} FROM {leaf_name}");
        let (total, nonnull, s1, s2) = Spi::connect(|client| {
            let table = client.select(&stats_sql, Some(1), &[])?;
            let res = match table.into_iter().next() {
                Some(r) => (
                    r.get::<i64>(1).ok().flatten().unwrap_or(0),
                    r.get::<i64>(2).ok().flatten().unwrap_or(0),
                    r.get::<String>(3).ok().flatten(),
                    r.get::<String>(4).ok().flatten(),
                ),
                None => (0, 0, None, None),
            };
            Ok::<(i64, i64, Option<String>, Option<String>), SpiError>(res)
        })?;

        let has_nulls = total > nonnull;
        let all_nulls = total > 0 && nonnull == 0;
        let overlap = kind.overlap();

        summary.cols.push(ColSummary {
            attnum,
            overlap,
            type_name,
            min: s1,
            max: s2,
            has_nulls,
            all_nulls,
        });
    }
    Ok(summary)
}

/// How a column is summarized.
enum ColumnKind {
    /// Default: btree min/max, used for scalar comparison pruning.
    MinMax,
    /// Range type: a covering range is stored for `&&` overlap pruning.
    Range,
    /// PostGIS geometry: the bounding extent is stored for `&&` overlap pruning.
    Geometry,
}

impl ColumnKind {
    fn overlap(&self) -> bool {
        !matches!(self, ColumnKind::MinMax)
    }
    /// SQL expressions for (min/extent, max) text given the quoted column name.
    fn exprs(&self, qcol: &str) -> (String, String) {
        match self {
            ColumnKind::MinMax => (format!("min({qcol})::text"), format!("max({qcol})::text")),
            ColumnKind::Range => (
                format!("range_merge(range_agg({qcol}))::text"),
                "NULL::text".to_string(),
            ),
            ColumnKind::Geometry => (
                format!("ST_Extent({qcol})::geometry::text"),
                "NULL::text".to_string(),
            ),
        }
    }
}

/// Classify a leaf column and resolve its SQL type name: range types and PostGIS
/// geometry get extent/overlap summaries; everything else gets btree min/max.
fn column_kind(leaf: pg_sys::Oid, col: &str) -> Result<(ColumnKind, String), SpiError> {
    let row = Spi::connect(|client| {
        let table = client.select(
            &format!(
                "SELECT a.atttypid::regtype::text AS typename, t.typtype::text \
                 FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid \
                 WHERE a.attrelid = {}::oid AND a.attname = {} AND NOT a.attisdropped",
                oid_u32(leaf),
                quote_literal(col)
            ),
            Some(1),
            &[],
        )?;
        let res = table.into_iter().next().map(|r| {
            (
                r.get::<String>(1).ok().flatten(),
                r.get::<String>(2).ok().flatten(),
            )
        });
        Ok::<Option<(Option<String>, Option<String>)>, SpiError>(res)
    })?;

    let (type_name, typtype) = match row {
        Some((Some(tn), tt)) => (tn, tt.unwrap_or_default()),
        _ => return Ok((ColumnKind::MinMax, String::new())),
    };

    let kind = if typtype == "r" {
        ColumnKind::Range
    } else if type_name == "geometry" || type_name.ends_with(".geometry") {
        ColumnKind::Geometry
    } else {
        ColumnKind::MinMax
    };
    Ok((kind, type_name))
}

/// Resolve a relation's column names for the given heap attnums (skips dropped/missing).
pub(crate) fn column_names_for_attnums(
    relid: pg_sys::Oid,
    attnums: &[i16],
) -> Result<Vec<String>, SpiError> {
    let mut names = Vec::new();
    for &attnum in attnums {
        if attnum <= 0 {
            continue;
        }
        let name = Spi::get_one::<String>(&format!(
            "SELECT attname::text FROM pg_attribute \
             WHERE attrelid = {}::oid AND attnum = {} AND NOT attisdropped",
            oid_u32(relid),
            attnum
        ))?;
        if let Some(name) = name {
            names.push(name);
        }
    }
    Ok(names)
}

fn relation_name(relid: pg_sys::Oid) -> Result<Option<String>, SpiError> {
    Spi::get_one::<String>(&format!("SELECT {}::oid::regclass::text", oid_u32(relid)))
}

fn leaf_attnum(leaf: pg_sys::Oid, col: &str) -> Result<Option<i16>, SpiError> {
    Spi::get_one::<i16>(&format!(
        "SELECT attnum FROM pg_attribute \
         WHERE attrelid = {}::oid AND attname = {} AND attnum > 0 AND NOT attisdropped",
        oid_u32(leaf),
        quote_literal(col)
    ))
}

/// Minimal single-quote escaping for SQL string literals.
fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Minimal identifier quoting (always double-quote, escaping embedded quotes).
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

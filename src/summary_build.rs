use pgrx::prelude::*;
use pgrx::spi::SpiError;
use std::sync::OnceLock;

// SPI-driven summary maintenance for the table_range pruning extension.
//
// Summaries are built by the index access method's `ambuild` (see `index_am.rs`): for
// each leaf partition it scans the column's real data and persists one summary row into
// `table_range_summary`, keyed by:
// - `index_oid` = the (leaf) index relation OID,
// - `relid` = the leaf partition (heap) OID the planner sees,
// - `attnum` = the leaf partition's attnum for the column.
//
// Correctness: a missing or `stale` summary means "do not prune". We never persist a
// summary that could cause a false negative; on any failure we leave the partition
// unsummarized (KEEP behavior at planning time).

fn oid_u32(oid: pg_sys::Oid) -> u32 {
    oid.into()
}

/// The extension's schema (where its tables live). PostgreSQL restricts `search_path`
/// during index builds (`ambuild`), so unqualified references fail there; resolving and
/// caching the schema (via pg_catalog, always reachable) keeps every code path working.
pub(crate) fn schema() -> &'static str {
    static SCHEMA: OnceLock<String> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        Spi::get_one::<String>(
            "SELECT relnamespace::regnamespace::text FROM pg_class \
             WHERE relname = 'table_range_summary' AND relkind = 'r' LIMIT 1",
        )
        .ok()
        .flatten()
        .unwrap_or_else(|| "public".to_string())
    })
}

/// Schema-qualified name of the summary table, e.g. `public.table_range_summary`.
pub(crate) fn summary_table() -> String {
    format!("{}.table_range_summary", schema())
}

/// V1 record for the `sql_drop` event-trigger cleanup function.
#[no_mangle]
pub extern "C" fn pg_finfo_table_range_drop_cleanup() -> &'static pg_sys::Pg_finfo_record {
    const V1_API: pg_sys::Pg_finfo_record = pg_sys::Pg_finfo_record { api_version: 1 };
    &V1_API
}

/// Event-trigger handler: when any relation is dropped, remove the summaries that
/// referenced it (by index OID or leaf OID). This closes the gap where a dropped
/// `table_range` index would leave summaries behind that nothing keeps stale anymore.
#[no_mangle]
#[pg_guard]
pub unsafe extern "C-unwind" fn table_range_drop_cleanup(
    _fcinfo: pg_sys::FunctionCallInfo,
) -> pg_sys::Datum {
    pgrx::PgTryBuilder::new(|| {
        let _ = Spi::run(&format!(
            "DELETE FROM {tbl} s USING pg_event_trigger_dropped_objects() d \
             WHERE s.index_oid = d.objid OR s.relid = d.objid",
            tbl = summary_table()
        ));
    })
    .catch_others(|_| ())
    .execute();
    pg_sys::Datum::from(0)
}

/// Build summaries for a single leaf relation's named columns. Called by `ambuild`
/// (keyed by the index OID). Returns the number of summary rows written.
pub(crate) fn build_one_leaf(
    index_oid: pg_sys::Oid,
    leaf: pg_sys::Oid,
    columns: &[String],
) -> Result<i64, SpiError> {
    let leaf_name = match relation_name(leaf)? {
        Some(n) => n,
        None => return Ok(0),
    };

    let mut written = 0i64;
    for col in columns {
        // Resolve this leaf's attnum for the column by name.
        let attnum = match leaf_attnum(leaf, col)? {
            Some(a) => a,
            None => continue, // column absent on this leaf; KEEP behavior
        };

        let kind = column_kind(leaf, col)?;
        let qcol = quote_ident(col);

        // Per-kind summary expressions. `minmax` stores the btree min/max; `overlap`
        // stores a single covering extent for `&&` pruning (range types / geometry).
        let (e1, e2) = match &kind {
            ColumnKind::MinMax => (format!("min({qcol})::text"), format!("max({qcol})::text")),
            ColumnKind::Range { .. } => (
                format!("range_merge(range_agg({qcol}))::text"),
                "NULL::text".to_string(),
            ),
            ColumnKind::Geometry { .. } => (
                format!("ST_Extent({qcol})::geometry::text"),
                "NULL::text".to_string(),
            ),
        };

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

        upsert_summary(
            index_oid,
            leaf,
            attnum,
            kind.tag(),
            kind.type_name(),
            s1.as_deref(),
            s2.as_deref(),
            has_nulls,
            all_nulls,
        )?;
        written += 1;
    }
    Ok(written)
}

/// How a column is summarized.
enum ColumnKind {
    /// Default: btree min/max, used for scalar comparison pruning.
    MinMax,
    /// Range type: a covering range is stored for `&&` overlap pruning.
    Range { type_name: String },
    /// PostGIS geometry: the bounding extent is stored for `&&` overlap pruning.
    Geometry { type_name: String },
}

impl ColumnKind {
    fn tag(&self) -> &'static str {
        match self {
            ColumnKind::MinMax => "minmax",
            ColumnKind::Range { .. } | ColumnKind::Geometry { .. } => "overlap",
        }
    }
    fn type_name(&self) -> Option<&str> {
        match self {
            ColumnKind::MinMax => None,
            ColumnKind::Range { type_name } | ColumnKind::Geometry { type_name } => {
                Some(type_name.as_str())
            }
        }
    }
}

/// Classify a leaf column: range types and PostGIS geometry get extent/overlap
/// summaries; everything else gets btree min/max.
fn column_kind(leaf: pg_sys::Oid, col: &str) -> Result<ColumnKind, SpiError> {
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
        _ => return Ok(ColumnKind::MinMax),
    };

    if typtype == "r" {
        Ok(ColumnKind::Range { type_name })
    } else if type_name == "geometry" || type_name.ends_with(".geometry") {
        Ok(ColumnKind::Geometry { type_name })
    } else {
        Ok(ColumnKind::MinMax)
    }
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

#[allow(clippy::too_many_arguments)]
fn upsert_summary(
    index_oid: pg_sys::Oid,
    leaf: pg_sys::Oid,
    attnum: i16,
    kind: &str,
    type_name: Option<&str>,
    min_text: Option<&str>,
    max_text: Option<&str>,
    has_nulls: bool,
    all_nulls: bool,
) -> Result<(), SpiError> {
    let lit = |v: Option<&str>| match v {
        Some(s) => quote_literal(s),
        None => "NULL".to_string(),
    };
    let q = format!(
        "INSERT INTO {tbl} AS s \
         (index_oid, relid, attnum, kind, type_name, min_summary, max_summary, has_nulls, all_nulls, stale, tuple_version) \
         VALUES ({p}::oid, {r}::oid, {a}, {kind}, {tn}, {min}, {max}, {hn}, {an}, false, 1) \
         ON CONFLICT (index_oid, relid, attnum) DO UPDATE SET \
            kind = EXCLUDED.kind, \
            type_name = EXCLUDED.type_name, \
            min_summary = EXCLUDED.min_summary, \
            max_summary = EXCLUDED.max_summary, \
            has_nulls = EXCLUDED.has_nulls, \
            all_nulls = EXCLUDED.all_nulls, \
            stale = false, \
            tuple_version = s.tuple_version + 1",
        tbl = summary_table(),
        p = oid_u32(index_oid),
        r = oid_u32(leaf),
        a = attnum,
        kind = quote_literal(kind),
        tn = lit(type_name),
        min = lit(min_text),
        max = lit(max_text),
        hn = if has_nulls { "true" } else { "false" },
        an = if all_nulls { "true" } else { "false" },
    );
    Spi::run(&q)
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

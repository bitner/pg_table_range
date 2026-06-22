use pgrx::prelude::*;
use pgrx::spi::SpiError;
use std::sync::OnceLock;

// Real, SPI-driven summary maintenance for the table_range pruning extension.
//
// Scans a registered parent relation's leaf partitions and persists one
// per-(partition, column) min/max/null summary into `table_range_summary`.
//
// Summaries are keyed by:
// - `index_oid` = the registered parent relation OID (synthetic key, no real index),
// - `relid` = the leaf partition OID,
// - `attnum` = the leaf partition's attnum for the column (resolved by name so
//   differing physical column order across partitions is handled).
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

/// Schema-qualified name of the registration table.
pub(crate) fn registered_table() -> String {
    format!("{}.table_range_registered", schema())
}

/// Register a parent relation and build summaries for the named columns.
#[pg_extern]
fn table_range_create(parent: pg_sys::Oid, columns: Vec<String>) -> i64 {
    if columns.is_empty() {
        error!("table_range_create: at least one column is required");
    }
    validate_columns_exist(parent, &columns);

    // Persist registration (idempotent).
    let cols_literal = pg_array_text_literal(&columns);
    let reg = format!(
        "INSERT INTO table_range_registered (parent_relid, columns, refreshed_at) \
         VALUES ({}::oid, {}, now()) \
         ON CONFLICT (parent_relid) DO UPDATE SET columns = EXCLUDED.columns, refreshed_at = now()",
        oid_u32(parent),
        cols_literal
    );
    Spi::run(&reg).unwrap_or_else(|e| error!("table_range_create: failed to register parent: {e}"));

    build_summaries(parent, &columns)
        .unwrap_or_else(|e| error!("table_range_create: summary build failed: {e}"))
}

/// Recompute summaries for an already-registered parent relation.
#[pg_extern]
fn table_range_refresh(parent: pg_sys::Oid) -> i64 {
    let columns = registered_columns(parent)
        .unwrap_or_else(|e| error!("table_range_refresh: lookup failed: {e}"));
    let columns = match columns {
        Some(c) => c,
        None => error!(
            "table_range_refresh: parent {} is not registered",
            oid_u32(parent)
        ),
    };
    let written = build_summaries(parent, &columns)
        .unwrap_or_else(|e| error!("table_range_refresh: summary build failed: {e}"));
    Spi::run(&format!(
        "UPDATE table_range_registered SET refreshed_at = now() WHERE parent_relid = {}::oid",
        oid_u32(parent)
    ))
    .ok();
    written
}

/// Unregister a parent relation, drop its summaries, and remove its triggers.
#[pg_extern]
fn table_range_drop(parent: pg_sys::Oid) -> bool {
    // Remove staleness triggers from every leaf first (best-effort).
    if let Ok(leaves) = leaf_partitions(parent) {
        for leaf in leaves {
            if let Ok(Some(name)) = relation_name(leaf) {
                let _ = Spi::run(&format!(
                    "DROP TRIGGER IF EXISTS {trg} ON {tbl}; \
                     DROP TRIGGER IF EXISTS {trg}_trunc ON {tbl}",
                    trg = STALE_TRIGGER_NAME,
                    tbl = name
                ));
            }
        }
    }

    let p = oid_u32(parent);
    Spi::run(&format!(
        "DELETE FROM {tbl} WHERE index_oid = {p}::oid",
        tbl = summary_table()
    ))
    .and_then(|_| {
        Spi::run(&format!(
            "DELETE FROM table_range_registered WHERE parent_relid = {p}::oid"
        ))
    })
    .is_ok()
}

/// Number of leaf partitions currently summarized for a parent.
#[pg_extern]
fn table_range_summary_count(parent: pg_sys::Oid) -> i64 {
    Spi::get_one::<i64>(&format!(
        "SELECT count(DISTINCT relid)::bigint FROM {tbl} WHERE index_oid = {p}::oid",
        tbl = summary_table(),
        p = oid_u32(parent)
    ))
    .ok()
    .flatten()
    .unwrap_or(0)
}

/// V1 record for the `sql_drop` event-trigger cleanup function.
#[no_mangle]
pub extern "C" fn pg_finfo_table_range_drop_cleanup() -> &'static pg_sys::Pg_finfo_record {
    const V1_API: pg_sys::Pg_finfo_record = pg_sys::Pg_finfo_record { api_version: 1 };
    &V1_API
}

/// Event-trigger handler: when any relation is dropped, remove the summaries and
/// registration that referenced it. This closes the correctness gap where a dropped
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
        let _ = Spi::run(&format!(
            "DELETE FROM {reg} r USING pg_event_trigger_dropped_objects() d \
             WHERE r.parent_relid = d.objid",
            reg = registered_table()
        ));
    })
    .catch_others(|_| ())
    .execute();
    pg_sys::Datum::from(0)
}

fn validate_columns_exist(parent: pg_sys::Oid, columns: &[String]) {
    for col in columns {
        let found = Spi::get_one::<bool>(&format!(
            "SELECT EXISTS (SELECT 1 FROM pg_attribute \
             WHERE attrelid = {}::oid AND attname = {} AND attnum > 0 AND NOT attisdropped)",
            oid_u32(parent),
            quote_literal(col)
        ))
        .ok()
        .flatten()
        .unwrap_or(false);
        if !found {
            error!(
                "table_range_create: column {:?} does not exist on relation {}",
                col,
                oid_u32(parent)
            );
        }
    }
}

fn registered_columns(parent: pg_sys::Oid) -> Result<Option<Vec<String>>, SpiError> {
    let mut out: Vec<String> = Vec::new();
    let mut registered = false;
    Spi::connect(|client| {
        let table = client.select(
            &format!(
                "SELECT unnest(columns) AS c FROM table_range_registered WHERE parent_relid = {}::oid",
                oid_u32(parent)
            ),
            None,
            &[],
        )?;
        for row in table {
            registered = true;
            if let Ok(Some(c)) = row.get::<String>(1) {
                out.push(c);
            }
        }
        Ok::<(), SpiError>(())
    })?;
    if !registered {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

/// Enumerate leaf partitions of `parent`. For a non-partitioned table this returns
/// the table itself, so summaries work for plain tables too.
fn leaf_partitions(parent: pg_sys::Oid) -> Result<Vec<pg_sys::Oid>, SpiError> {
    let mut leaves: Vec<pg_sys::Oid> = Vec::new();
    Spi::connect(|client| {
        let table = client.select(
            &format!(
                "SELECT relid::oid FROM pg_partition_tree({}::oid::regclass) WHERE isleaf",
                oid_u32(parent)
            ),
            None,
            &[],
        )?;
        for row in table {
            if let Ok(Some(oid)) = row.get::<pg_sys::Oid>(1) {
                leaves.push(oid);
            }
        }
        Ok::<(), SpiError>(())
    })?;
    Ok(leaves)
}

/// Trigger name installed on each leaf to mark its summaries stale on data change.
const STALE_TRIGGER_NAME: &str = "table_range_stale_trg";

/// Install (idempotently) the staleness triggers on a leaf partition.
///
/// A row-level trigger is required for INSERT/UPDATE/DELETE because statement-level
/// triggers on a leaf do not fire for tuples routed through the partitioned parent;
/// row-level triggers do. TRUNCATE cannot be row-level, so it gets a statement
/// trigger. Both mark only this leaf's summaries stale (precise, not global).
fn install_stale_trigger(leaf_name: &str) -> Result<(), SpiError> {
    Spi::run(&format!(
        "DROP TRIGGER IF EXISTS {trg} ON {tbl}; \
         CREATE TRIGGER {trg} AFTER INSERT OR UPDATE OR DELETE ON {tbl} \
         FOR EACH ROW EXECUTE FUNCTION table_range_stale_trigger(); \
         DROP TRIGGER IF EXISTS {trg}_trunc ON {tbl}; \
         CREATE TRIGGER {trg}_trunc AFTER TRUNCATE ON {tbl} \
         FOR EACH STATEMENT EXECUTE FUNCTION table_range_stale_trigger();",
        trg = STALE_TRIGGER_NAME,
        tbl = leaf_name
    ))
}

/// Returns the number of summary rows written.
fn build_summaries(parent: pg_sys::Oid, columns: &[String]) -> Result<i64, SpiError> {
    let leaves = leaf_partitions(parent)?;
    let mut written = 0i64;
    for leaf in &leaves {
        written += build_one_leaf(parent, *leaf, columns, true)?;
    }
    Ok(written)
}

/// Build summaries for a single leaf relation's named columns and install the
/// correctness safety-net trigger on it. Used by both `table_range_create` (keyed by
/// parent OID) and the index AM's `ambuild` (keyed by index OID). Returns the number
/// of summary rows written.
pub(crate) fn build_one_leaf(
    index_oid: pg_sys::Oid,
    leaf: pg_sys::Oid,
    columns: &[String],
    install_trigger: bool,
) -> Result<i64, SpiError> {
    let leaf_name = match relation_name(leaf)? {
        Some(n) => n,
        None => return Ok(0),
    };
    // Ensure the correctness safety-net trigger exists before (re)building.
    if install_trigger {
        install_stale_trigger(&leaf_name)?;
    }

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
    parent: pg_sys::Oid,
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
        p = oid_u32(parent),
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

fn pg_array_text_literal(items: &[String]) -> String {
    let inner = items
        .iter()
        .map(|s| quote_literal(s))
        .collect::<Vec<_>>()
        .join(", ");
    format!("ARRAY[{}]::text[]", inner)
}

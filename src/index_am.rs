use crate::index_storage::{self, ColSummary};
use crate::insert_cache::{self, InsertCache};
use crate::prune_hook::{btree_cmp_proc, datum_cmp, datum_to_text, text_to_datum};
use crate::stable_io::with_stable_io;
use crate::summary_build::{geometry_schema, spi_value};
use pgrx::pg_sys;
use pgrx::prelude::*;

// Custom index access method `table_range`, providing the ergonomic
// `CREATE INDEX ... USING table_range (cols)` front-end over the same summary engine.
//
// This is not a conventional scannable index: instead of indexing rows, it stores one
// min/max/null (or extent) summary per indexed column in its own metapage. `ambuild`
// scans the leaf relation to build it; `aminsert` widens it in place as rows arrive (like
// BRIN, and needing no MVCC because the summary only has to be over-inclusive);
// `amvacuumcleanup` rebuilds summaries that need it. The
// planner hook prunes partitions using those summaries. The index is never chosen for
// scans (no `amgettuple`/`amgetbitmap`, prohibitive cost estimate).

/// V1 function-info record so PostgreSQL can call `table_range_amhandler` as a
/// `LANGUAGE c` function declared in the access-method SQL below.
#[no_mangle]
pub extern "C" fn pg_finfo_table_range_amhandler() -> &'static pg_sys::Pg_finfo_record {
    const V1_API: pg_sys::Pg_finfo_record = pg_sys::Pg_finfo_record { api_version: 1 };
    &V1_API
}

/// Access-method handler: returns a populated `IndexAmRoutine`.
#[no_mangle]
#[pg_guard]
pub unsafe extern "C-unwind" fn table_range_amhandler(
    _fcinfo: pg_sys::FunctionCallInfo,
) -> pg_sys::Datum {
    {
        let mut amroutine =
            PgBox::<pg_sys::IndexAmRoutine>::alloc_node(pg_sys::NodeTag::T_IndexAmRoutine);

        // `alloc_node` zeroes the struct (palloc0), so every false/0/InvalidOid field is
        // already at its default. We set only the non-default fields, which keeps this
        // portable across PG 16/17/18 (whose IndexAmRoutine has different optional flags).
        amroutine.amcanmulticol = true; // CREATE INDEX may list several columns
        amroutine.amoptionalkey = true; // queries need not constrain the first column
        amroutine.amstorage = true; // opclasses declare a STORAGE type
                                    // Summarizing (like BRIN): updates of indexed columns can stay HOT.
        amroutine.amsummarizing = true;

        amroutine.ambuild = Some(am_build);
        amroutine.ambuildempty = Some(am_buildempty);
        amroutine.aminsert = Some(am_insert);
        amroutine.ambulkdelete = Some(am_bulkdelete);
        amroutine.amvacuumcleanup = Some(am_vacuumcleanup);
        amroutine.amcostestimate = Some(am_costestimate);
        amroutine.amoptions = Some(am_options);
        amroutine.amvalidate = Some(am_validate);

        pg_sys::Datum::from(amroutine.into_pg() as *mut core::ffi::c_void)
    }
}

/// Errors propagate: a failed build must fail `CREATE INDEX`, not silently disable pruning.
#[pg_guard]
unsafe extern "C-unwind" fn am_build(
    heap: pg_sys::Relation,
    index: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
) -> *mut pg_sys::IndexBuildResult {
    // A partial index never sees inserts outside its predicate, so could not cover them.
    if !(*index_info).ii_Predicate.is_null() {
        ereport!(
            ERROR,
            PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
            "table_range indexes cannot be partial"
        );
    }
    // Summaries are keyed by column, so a column indexed twice (e.g. under two
    // collations) would mix both orderings in one summary.
    let attnums = &(&(*index_info).ii_IndexAttrNumbers)[..(*index_info).ii_NumIndexAttrs as usize];
    if attnums
        .iter()
        .enumerate()
        .any(|(i, a)| *a > 0 && attnums[..i].contains(a))
    {
        ereport!(
            ERROR,
            PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
            "a column can appear only once in a table_range index"
        );
    }

    let mut built = crate::summary_build::build_leaf_summary(heap, index, index_info, false);
    index_storage::write_summary(index, &mut built.summary).unwrap_or_else(|e| error!("{e}"));

    let result = pg_sys::palloc0(std::mem::size_of::<pg_sys::IndexBuildResult>())
        as *mut pg_sys::IndexBuildResult;
    // index_build stores this as the heap's reltuples.
    (*result).heap_tuples = built.heap_tuples;
    (*result).index_tuples = 0.0;
    result
}

#[pg_guard]
unsafe extern "C-unwind" fn am_buildempty(_index: pg_sys::Relation) {}

/// Incrementally widen the partition's metapage summary to include the inserted row —
/// the BRIN-style maintenance path. Because the summary only needs to be over-inclusive,
/// this update is in place and needs no MVCC; an insert that stays within the existing
/// range writes nothing. This keeps pruning correct and active without any REINDEX.
///
/// Errors are not caught: an insert the summary does not cover must not succeed.
#[pg_guard]
#[allow(clippy::too_many_arguments)] // signature is fixed by PostgreSQL's aminsert_function
unsafe extern "C-unwind" fn am_insert(
    index: pg_sys::Relation,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    _heap_tid: pg_sys::ItemPointer,
    heap: pg_sys::Relation,
    _check_unique: pg_sys::IndexUniqueCheck::Type,
    _index_unchanged: bool,
    index_info: *mut pg_sys::IndexInfo,
) -> bool {
    widen_on_insert(index, heap, index_info, values, isnull);
    false
}

/// Compare-and-swap against the bytes read, so concurrent widenings never overwrite each other.
unsafe fn widen_on_insert(
    index: pg_sys::Relation,
    heap: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
) {
    unsafe fn stable<T>(f: impl FnOnce() -> T) -> T {
        with_stable_io(f)
            .unwrap_or_else(|| error!("table_range: cannot maintain a summary in parallel mode"))
    }
    let slot = insert_cache::slot(index_info);
    loop {
        pgrx::check_for_interrupts!();
        // No summary, or an unreadable one: the planner never prunes with it.
        let Some(page) = index_storage::read_blob(index) else {
            return;
        };
        if slot.as_ref().is_none_or(|c| c.blob != page) {
            insert_cache::forget(slot);
            let Some(summary) = index_storage::deserialize(&page) else {
                return;
            };
            *slot = Some(stable(|| {
                InsertCache::decode(page, summary, index, index_info)
            }));
        }
        let cache = slot.as_mut().expect("decoded above");
        if cache.covers(values, isnull) {
            return;
        }
        let mut summary = cache.summary.clone();
        if !stable(|| cache.widen(&mut summary, values, isnull)) {
            return;
        }
        let after = index_storage::encode_fitting(&mut summary);
        if index_storage::replace_blob_if(index, &cache.blob, &after)
            .unwrap_or_else(|e| error!("{e}"))
        {
            crate::summary_cache::note_changed((*index).rd_id, (*heap).rd_id);
            cache.wrote(after, summary);
            return;
        }
        // Changed by another backend since we read it: decode the new page and retry.
        pgrx::debug1!("table_range: summary changed concurrently, retrying");
        insert_cache::forget(slot);
    }
}

pub(crate) struct IndexColumn {
    /// Position among the index's columns.
    pub pos: usize,
    pub attnum: i16,
    pub typ: pg_sys::Oid,
    pub collation: pg_sys::Oid,
}

pub(crate) unsafe fn index_columns(
    index: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
) -> Vec<IndexColumn> {
    (0..(*index_info).ii_NumIndexAttrs.max(0) as usize)
        .map(|pos| IndexColumn {
            pos,
            attnum: (*index_info).ii_IndexAttrNumbers[pos],
            typ: att_typid((*index).rd_att, pos),
            collation: if (*index).rd_indcollation.is_null() {
                pg_sys::Oid::INVALID
            } else {
                *(*index).rd_indcollation.add(pos)
            },
        })
        .collect()
}
/// The type OID of a tuple descriptor's `i`-th attribute, portable across PG versions.
/// PG18 made `TupleDescAttr` an inline function (bound by pgrx) and moved attributes to
/// `compact_attrs` (which has no `atttypid`); PG13–17 expose `attrs` directly and only a
/// `TupleDescAttr` macro (which bindgen does not surface as `pg_sys::TupleDescAttr`).
#[cfg(feature = "pg18")]
pub(crate) unsafe fn att_typid(tupdesc: pg_sys::TupleDesc, i: usize) -> pg_sys::Oid {
    (*pg_sys::TupleDescAttr(tupdesc, i as i32)).atttypid
}
#[cfg(not(feature = "pg18"))]
pub(crate) unsafe fn att_typid(tupdesc: pg_sys::TupleDesc, i: usize) -> pg_sys::Oid {
    let natts = (*tupdesc).natts as usize;
    (*tupdesc).attrs.as_slice(natts)[i].atttypid
}

/// Returns whether the summary changed. A value that cannot be folded in makes the column
/// unbounded rather than leaving it too narrow.
pub(crate) unsafe fn widen_column(
    col: &mut ColSummary,
    typoid: pg_sys::Oid,
    collation: pg_sys::Oid,
    value: pg_sys::Datum,
    isnull: bool,
) -> bool {
    if isnull {
        if !col.has_nulls {
            col.has_nulls = true;
            return true;
        }
        return false;
    }

    let mut changed = false;
    if col.all_nulls {
        col.all_nulls = false;
        changed = true;
    }
    if col.unbounded {
        return changed;
    }

    let widened = if col.overlap {
        widen_overlap(col, typoid, value)
    } else {
        widen_minmax(col, typoid, collation, value)
    };
    match widened {
        Some(c) => changed | c,
        None => {
            col.make_unbounded();
            true
        }
    }
}

/// `None`: the value could not be compared against the stored bounds.
unsafe fn widen_minmax(
    col: &mut ColSummary,
    typoid: pg_sys::Oid,
    collation: pg_sys::Oid,
    value: pg_sys::Datum,
) -> Option<bool> {
    // Domains compare and parse as their base type.
    let base = pg_sys::getBaseType(typoid);
    let new_text = datum_to_text(typoid, value)?;
    match (&col.min, &col.max) {
        (Some(min), Some(max)) => {
            let cmp = btree_cmp_proc(base)?;
            let min_d = text_to_datum(base, min)?;
            let max_d = text_to_datum(base, max)?;
            let mut changed = false;
            if datum_cmp(cmp, collation, value, min_d) < 0 {
                col.min = Some(new_text.clone());
                changed = true;
            }
            if datum_cmp(cmp, collation, value, max_d) > 0 {
                col.max = Some(new_text);
                changed = true;
            }
            Some(changed)
        }
        // No range yet (was all-null/empty): seed it with this value.
        (None, None) => {
            col.min = Some(new_text.clone());
            col.max = Some(new_text);
            Some(true)
        }
        _ => None,
    }
}

/// `None`: the new extent could not be computed.
unsafe fn widen_overlap(
    col: &mut ColSummary,
    typoid: pg_sys::Oid,
    value: pg_sys::Datum,
) -> Option<bool> {
    let base = pg_sys::getBaseType(typoid);
    let widened = if crate::summary_build::is_geometry(base) {
        geometry_extent_with(col, typoid, value)?
    } else {
        let merged = match &col.min {
            None => value,
            Some(ext) => {
                let ext = text_to_datum(base, ext)?;
                pg_sys::OidFunctionCall2Coll(
                    pg_sys::Oid::from(pg_sys::F_RANGE_MERGE_ANYRANGE_ANYRANGE),
                    pg_sys::Oid::INVALID,
                    ext,
                    value,
                )
            }
        };
        Some(datum_to_text(typoid, merged)?)
    };

    match widened {
        Some(w) if col.min.as_ref() != Some(&w) => {
            col.min = Some(w);
            Some(true)
        }
        Some(_) => Some(false),
        // An empty geometry has no extent and can never satisfy `&&`: nothing to cover.
        None => Some(false),
    }
}

/// `Some(None)` if neither the stored extent nor `value` has one (empty geometries).
unsafe fn geometry_extent_with(
    col: &ColSummary,
    typoid: pg_sys::Oid,
    value: pg_sys::Datum,
) -> Option<Option<String>> {
    let new_text = datum_to_text(typoid, value)?;
    let tn = &col.type_name;
    let ns = geometry_schema(tn);
    let lit = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let mut rows = format!("(CAST({} AS {tn}))", lit(&new_text));
    if let Some(ext) = &col.min {
        rows.push_str(&format!(", (CAST({} AS {tn}))", lit(ext)));
    }
    let sql = format!("SELECT {ns}.st_extent(g)::{tn}::pg_catalog.text FROM (VALUES {rows}) v(g)");
    pg_sys::SPI_connect();
    let extent = spi_value(&sql);
    pg_sys::SPI_finish();
    Some(extent)
}

#[pg_guard]
unsafe extern "C-unwind" fn am_bulkdelete(
    _info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
    _callback: pg_sys::IndexBulkDeleteCallback,
    _callback_state: *mut core::ffi::c_void,
) -> *mut pg_sys::IndexBulkDeleteResult {
    stats
}

/// Rebuilds summaries that are unbounded (e.g. once an oversized value is deleted), were
/// left mid-rebuild, or are in an on-page format this build does not prune with.
#[pg_guard]
unsafe extern "C-unwind" fn am_vacuumcleanup(
    info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
) -> *mut pg_sys::IndexBulkDeleteResult {
    // shortcut: skipped under VACUUM (PARALLEL); run table_range_summarize() instead.
    if !info.is_null()
        && !(*info).analyze_only
        && !(*info).heaprel.is_null()
        && !pg_sys::IsInParallelMode()
    {
        crate::summarize::resummarize((*info).index, (*info).heaprel, true);
    }
    stats
}

/// Prohibitive cost so the planner never selects this index for an actual scan.
#[pg_guard]
#[allow(clippy::too_many_arguments)] // signature is fixed by PostgreSQL's amcostestimate_function
unsafe extern "C-unwind" fn am_costestimate(
    _root: *mut pg_sys::PlannerInfo,
    _path: *mut pg_sys::IndexPath,
    _loop_count: f64,
    index_startup_cost: *mut pg_sys::Cost,
    index_total_cost: *mut pg_sys::Cost,
    index_selectivity: *mut pg_sys::Selectivity,
    index_correlation: *mut f64,
    index_pages: *mut f64,
) {
    *index_startup_cost = 0.0;
    *index_total_cost = f64::MAX / 2.0;
    *index_selectivity = 1.0;
    *index_correlation = 0.0;
    *index_pages = 0.0;
}

/// No reloptions are supported; always returns NULL.
#[pg_guard]
unsafe extern "C-unwind" fn am_options(
    _reloptions: pg_sys::Datum,
    _validate: bool,
) -> *mut pg_sys::bytea {
    std::ptr::null_mut()
}

#[pg_guard]
unsafe extern "C-unwind" fn am_validate(_opclassoid: pg_sys::Oid) -> bool {
    true
}

extension_sql!(
    r#"
    CREATE FUNCTION table_range_amhandler(internal) RETURNS index_am_handler
        LANGUAGE c AS 'MODULE_PATHNAME', 'table_range_amhandler';

    CREATE ACCESS METHOD table_range TYPE INDEX HANDLER table_range_amhandler;
    COMMENT ON ACCESS METHOD table_range IS
        'Early partition pruning via per-partition data-range summaries';

    -- CREATE INDEX ... USING table_range needs a default operator class for the column
    -- type. Rather than hardcode a list, mirror the operator-class coverage that already
    -- exists: any btree-ordered type (scalar min/max), every range type, and PostGIS
    -- geometry/geography (extent). The AM stores only summaries, so these classes carry
    -- no operators or support procedures. This runs at install and again whenever an
    -- extension is created, so installing PostGIS makes geometry "just work" with no
    -- manual step.
    CREATE FUNCTION table_range_sync_opclasses() RETURNS void
        LANGUAGE plpgsql AS $$
        DECLARE
            tr_am oid;
            r record;
        BEGIN
            SELECT oid INTO tr_am FROM pg_am WHERE amname = 'table_range';
            IF tr_am IS NULL THEN
                RETURN;
            END IF;
            FOR r IN
                SELECT DISTINCT cand.typid, format_type(cand.typid, NULL) AS typname
                FROM (
                    SELECT bc.opcintype AS typid
                    FROM pg_opclass bc JOIN pg_am am ON am.oid = bc.opcmethod
                    WHERE am.amname = 'btree' AND bc.opcdefault
                    UNION
                    SELECT t.oid FROM pg_type t WHERE t.typtype = 'r'
                    UNION
                    SELECT t.oid FROM pg_type t WHERE t.typname IN ('geometry', 'geography')
                ) cand
                WHERE cand.typid NOT IN ('anyrange'::regtype, 'anyarray'::regtype)
                  AND NOT EXISTS (
                      SELECT 1 FROM pg_opclass tc
                      WHERE tc.opcmethod = tr_am AND tc.opcdefault
                        AND tc.opcintype = cand.typid
                  )
            LOOP
                BEGIN
                    EXECUTE format(
                        'CREATE OPERATOR CLASS %I DEFAULT FOR TYPE %s USING table_range AS STORAGE %s',
                        'tr_' || r.typid, r.typname, r.typname);
                EXCEPTION WHEN OTHERS THEN
                    -- Skip types that cannot host a storage-only opclass.
                    NULL;
                END;
            END LOOP;
        END;
        $$;

    SELECT table_range_sync_opclasses();

    CREATE FUNCTION table_range_opclass_sync_evt() RETURNS event_trigger
        LANGUAGE plpgsql AS $$
        BEGIN
            PERFORM table_range_sync_opclasses();
        END;
        $$;

    -- Re-sync when any extension is installed (e.g. PostGIS), so new types that gain a
    -- btree/geometry opclass automatically become usable with table_range.
    CREATE EVENT TRIGGER table_range_opclass_sync_trg
        ON ddl_command_end WHEN TAG IN ('CREATE EXTENSION')
        EXECUTE FUNCTION table_range_opclass_sync_evt();
    "#,
    name = "table_range_access_method"
);

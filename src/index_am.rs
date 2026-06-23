use crate::index_storage::{self, ColSummary};
use crate::prune_hook::{btree_cmp_proc, datum_cmp, datum_to_text, text_to_datum};
use pgrx::pg_sys;
use pgrx::prelude::*;

// Custom index access method `table_range`, providing the ergonomic
// `CREATE INDEX ... USING table_range (cols)` front-end over the same summary engine.
//
// This is not a conventional scannable index: instead of indexing rows, it stores one
// min/max/null (or extent) summary per indexed column in its own metapage. `ambuild`
// scans the leaf relation to build it; `aminsert` widens it in place as rows arrive (like
// BRIN, and needing no MVCC because the summary only has to be over-inclusive). The
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

/// Build the summary for one (leaf) relation from its actual data.
#[pg_guard]
unsafe extern "C-unwind" fn am_build(
    heap: pg_sys::Relation,
    index: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
) -> *mut pg_sys::IndexBuildResult {
    let heap_relid = (*heap).rd_id;

    let nattrs = (*index_info).ii_NumIndexAttrs.max(0) as usize;
    let attnums: Vec<i16> = (0..nattrs)
        .map(|i| (*index_info).ii_IndexAttrNumbers[i])
        .collect();

    // Compute the summary via SPI under a freshly pushed snapshot so the snapshot the
    // CREATE INDEX portal relies on is restored afterwards. Any failure degrades to
    // "no summary" (KEEP at planning time).
    pg_sys::PushActiveSnapshot(pg_sys::GetTransactionSnapshot());
    let summary = pgrx::PgTryBuilder::new(|| {
        let names = crate::summary_build::column_names_for_attnums(heap_relid, &attnums).ok()?;
        crate::summary_build::build_one_leaf(heap_relid, &names).ok()
    })
    .catch_others(|_| None)
    .execute();
    pg_sys::PopActiveSnapshot();

    // Persist the summary into the index's own metapage. Page writes use no snapshot,
    // so this happens after the SPI section.
    if let Some(summary) = summary {
        let _ = crate::index_storage::write_summary(index, &summary);
    }

    let result = pg_sys::palloc0(std::mem::size_of::<pg_sys::IndexBuildResult>())
        as *mut pg_sys::IndexBuildResult;
    (*result).heap_tuples = 0.0;
    (*result).index_tuples = 0.0;
    result
}

#[pg_guard]
unsafe extern "C-unwind" fn am_buildempty(_index: pg_sys::Relation) {}

/// Incrementally widen the partition's metapage summary to include the inserted row —
/// the BRIN-style maintenance path. Because the summary only needs to be over-inclusive,
/// this update is in place and needs no MVCC; an insert that stays within the existing
/// range writes nothing. This keeps pruning correct and active without any REINDEX.
#[pg_guard]
#[allow(clippy::too_many_arguments)] // signature is fixed by PostgreSQL's aminsert_function
unsafe extern "C-unwind" fn am_insert(
    index: pg_sys::Relation,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    _heap_tid: pg_sys::ItemPointer,
    _heap: pg_sys::Relation,
    _check_unique: pg_sys::IndexUniqueCheck::Type,
    _index_unchanged: bool,
    index_info: *mut pg_sys::IndexInfo,
) -> bool {
    pgrx::PgTryBuilder::new(|| widen_on_insert(index, index_info, values, isnull))
        .catch_others(|_| ())
        .execute();
    false
}

/// Read the metapage summary, widen each indexed column to cover the new value, and
/// write it back only if something changed.
unsafe fn widen_on_insert(
    index: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
) {
    let mut summary = match index_storage::read_summary(index) {
        Some(s) => s,
        None => return,
    };
    let nattrs = (*index_info).ii_NumIndexAttrs.max(0) as usize;
    let mut changed = false;
    for i in 0..nattrs {
        let heap_attnum = (*index_info).ii_IndexAttrNumbers[i];
        let col = match summary.cols.iter_mut().find(|c| c.attnum == heap_attnum) {
            Some(c) => c,
            None => continue,
        };
        let typoid = att_typid((*index).rd_att, i);
        let collation = if (*index).rd_indcollation.is_null() {
            pg_sys::Oid::INVALID
        } else {
            *(*index).rd_indcollation.add(i)
        };
        changed |= widen_column(col, typoid, collation, *values.add(i), *isnull.add(i));
    }
    if changed {
        let _ = index_storage::write_summary(index, &summary);
        // The on-page summary just widened; drop any cached (now-too-narrow) copy in every
        // backend so planning never prunes away the newly covered values.
        crate::summary_cache::note_widened((*index).rd_id);
    }
}

/// The type OID of a tuple descriptor's `i`-th attribute, portable across PG versions.
/// PG18 made `TupleDescAttr` an inline function (bound by pgrx) and moved attributes to
/// `compact_attrs` (which has no `atttypid`); PG13–17 expose `attrs` directly and only a
/// `TupleDescAttr` macro (which bindgen does not surface as `pg_sys::TupleDescAttr`).
#[cfg(feature = "pg18")]
unsafe fn att_typid(tupdesc: pg_sys::TupleDesc, i: usize) -> pg_sys::Oid {
    (*pg_sys::TupleDescAttr(tupdesc, i as i32)).atttypid
}
#[cfg(not(feature = "pg18"))]
unsafe fn att_typid(tupdesc: pg_sys::TupleDesc, i: usize) -> pg_sys::Oid {
    let natts = (*tupdesc).natts as usize;
    (*tupdesc).attrs.as_slice(natts)[i].atttypid
}

/// Widen one column's summary for a single inserted value. Returns whether it changed.
unsafe fn widen_column(
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

    if col.overlap {
        return changed | widen_overlap(col, typoid, value);
    }

    // btree min/max: widen using the column type's compare support function.
    let new_text = match datum_to_text(typoid, value) {
        Some(t) => t,
        None => return changed,
    };
    match (&col.min, &col.max) {
        (Some(min), Some(max)) => {
            let cmp = match btree_cmp_proc(typoid) {
                Some(c) => c,
                None => return changed,
            };
            if let (Some(min_d), Some(max_d)) =
                (text_to_datum(typoid, min), text_to_datum(typoid, max))
            {
                if datum_cmp(cmp, collation, value, min_d) < 0 {
                    col.min = Some(new_text.clone());
                    changed = true;
                }
                if datum_cmp(cmp, collation, value, max_d) > 0 {
                    col.max = Some(new_text);
                    changed = true;
                }
            }
        }
        // No range yet (was all-null/empty): seed it with this value.
        _ => {
            col.min = Some(new_text.clone());
            col.max = Some(new_text);
            changed = true;
        }
    }
    changed
}

/// Widen an overlap (range/geometry) extent to cover a new value, via the type's own
/// union operator (delegated to SQL). Writes nothing if the value is already covered.
unsafe fn widen_overlap(col: &mut ColSummary, typoid: pg_sys::Oid, value: pg_sys::Datum) -> bool {
    let new_text = match datum_to_text(typoid, value) {
        Some(t) => t,
        None => return false,
    };
    let tn = &col.type_name;
    let is_geometry = tn == "geometry" || tn.ends_with(".geometry");
    let lit = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let new_lit = lit(&new_text);

    let sql = match &col.min {
        None if is_geometry => format!(
            "SELECT ST_Extent(g)::geometry::text FROM (VALUES (CAST({new_lit} AS {tn}))) v(g)"
        ),
        None => format!("SELECT CAST({new_lit} AS {tn})::text"),
        Some(ext) if is_geometry => format!(
            "SELECT ST_Extent(g)::geometry::text FROM \
             (VALUES (CAST({} AS {tn})), (CAST({new_lit} AS {tn}))) v(g)",
            lit(ext)
        ),
        Some(ext) => format!(
            "SELECT range_merge(CAST({} AS {tn}), CAST({new_lit} AS {tn}))::text",
            lit(ext)
        ),
    };

    let widened = Spi::get_one::<String>(&sql).ok().flatten();
    if widened.is_some() && widened != col.min {
        col.min = widened;
        return true;
    }
    false
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

#[pg_guard]
unsafe extern "C-unwind" fn am_vacuumcleanup(
    _info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
) -> *mut pg_sys::IndexBulkDeleteResult {
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

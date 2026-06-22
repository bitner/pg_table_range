use pgrx::pg_sys;
use pgrx::prelude::*;
use std::cell::RefCell;
use std::collections::HashSet;

thread_local! {
    /// Leaf relids whose summaries we've already marked stale in the current
    /// transaction, so a bulk insert marks each partition at most once instead of
    /// once per row. Cleared at transaction end (so a later transaction re-marks after
    /// a concurrent REINDEX could have refreshed the summary).
    static STALE_MARKED: RefCell<HashSet<u32>> = RefCell::new(HashSet::new());
}

/// Register the transaction callback that resets the per-transaction stale memo.
pub fn install() {
    unsafe {
        pg_sys::RegisterXactCallback(Some(xact_callback), std::ptr::null_mut());
    }
}

unsafe extern "C-unwind" fn xact_callback(
    event: pg_sys::XactEvent::Type,
    _arg: *mut core::ffi::c_void,
) {
    if event == pg_sys::XactEvent::XACT_EVENT_COMMIT
        || event == pg_sys::XactEvent::XACT_EVENT_ABORT
        || event == pg_sys::XactEvent::XACT_EVENT_PREPARE
    {
        STALE_MARKED.with(|s| s.borrow_mut().clear());
    }
}

// Custom index access method `table_range`, providing the ergonomic
// `CREATE INDEX ... USING table_range (cols)` front-end over the same summary engine.
//
// This is not a conventional scannable index: it stores nothing in index pages.
// Instead, `ambuild` scans the (leaf) relation and writes one min/max/null summary per
// indexed column into `table_range_summary` (keyed by the index OID), and installs
// the staleness trigger that keeps the summary conservative on data changes. The planner
// hook then prunes partitions exactly as it does for the function interface. The index is
// never chosen for scans (no `amgettuple`/`amgetbitmap`, prohibitive cost estimate).

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
    let index_relid = (*index).rd_id;

    let nattrs = (*index_info).ii_NumIndexAttrs.max(0) as usize;
    let attnums: Vec<i16> = (0..nattrs)
        .map(|i| (*index_info).ii_IndexAttrNumbers[i])
        .collect();

    // Build summaries via SPI under a freshly pushed snapshot so the snapshot the
    // CREATE INDEX portal relies on is restored afterwards. We skip trigger
    // installation here (DDL is unsafe inside the build portal); staleness for the AM
    // path is handled by `aminsert`. Any failure degrades to "no summary" (KEEP).
    pg_sys::PushActiveSnapshot(pg_sys::GetTransactionSnapshot());
    pgrx::PgTryBuilder::new(|| {
        if let Ok(names) = crate::summary_build::column_names_for_attnums(heap_relid, &attnums) {
            let _ = crate::summary_build::build_one_leaf(index_relid, heap_relid, &names, false);
        }
    })
    .catch_others(|_| ())
    .execute();
    pg_sys::PopActiveSnapshot();

    let result = pg_sys::palloc0(std::mem::size_of::<pg_sys::IndexBuildResult>())
        as *mut pg_sys::IndexBuildResult;
    (*result).heap_tuples = 0.0;
    (*result).index_tuples = 0.0;
    result
}

#[pg_guard]
unsafe extern "C-unwind" fn am_buildempty(_index: pg_sys::Relation) {}

/// Conservatively mark this partition's summaries stale on insert so the planner stops
/// pruning it until a rebuild (REINDEX) recomputes the range. The `AND NOT stale` guard
/// makes the steady state a cheap no-op once a partition is already marked.
#[pg_guard]
#[allow(clippy::too_many_arguments)] // signature is fixed by PostgreSQL's aminsert_function
unsafe extern "C-unwind" fn am_insert(
    _index: pg_sys::Relation,
    _values: *mut pg_sys::Datum,
    _isnull: *mut bool,
    _heap_tid: pg_sys::ItemPointer,
    heap: pg_sys::Relation,
    _check_unique: pg_sys::IndexUniqueCheck::Type,
    _index_unchanged: bool,
    _index_info: *mut pg_sys::IndexInfo,
) -> bool {
    let heap_relid: u32 = (*heap).rd_id.into();
    // Already marked stale in this transaction? Then nothing more to do this statement.
    if STALE_MARKED.with(|s| s.borrow().contains(&heap_relid)) {
        return false;
    }
    let marked = pgrx::PgTryBuilder::new(|| {
        Spi::run(&format!(
            "UPDATE {tbl} SET stale = true WHERE relid = {heap_relid}::oid AND NOT stale",
            tbl = crate::summary_build::summary_table()
        ))
        .is_ok()
    })
    .catch_others(|_| false)
    .execute();
    // Only memo on success, so a failed mark is retried on the next row.
    if marked {
        STALE_MARKED.with(|s| s.borrow_mut().insert(heap_relid));
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
        'Conservative early partition pruning via min/max range summaries';

    -- Minimal default operator classes so CREATE INDEX ... USING table_range resolves a
    -- class for each common column type. The AM stores only summaries, so these carry no
    -- operators or support procedures (amvalidate accepts them).
    CREATE OPERATOR CLASS bool_tr_ops    DEFAULT FOR TYPE boolean     USING table_range AS STORAGE boolean;
    CREATE OPERATOR CLASS int2_tr_ops    DEFAULT FOR TYPE smallint    USING table_range AS STORAGE smallint;
    CREATE OPERATOR CLASS int4_tr_ops    DEFAULT FOR TYPE integer     USING table_range AS STORAGE integer;
    CREATE OPERATOR CLASS int8_tr_ops    DEFAULT FOR TYPE bigint      USING table_range AS STORAGE bigint;
    CREATE OPERATOR CLASS float4_tr_ops  DEFAULT FOR TYPE real        USING table_range AS STORAGE real;
    CREATE OPERATOR CLASS float8_tr_ops  DEFAULT FOR TYPE double precision USING table_range AS STORAGE double precision;
    CREATE OPERATOR CLASS numeric_tr_ops DEFAULT FOR TYPE numeric     USING table_range AS STORAGE numeric;
    CREATE OPERATOR CLASS text_tr_ops    DEFAULT FOR TYPE text        USING table_range AS STORAGE text;
    CREATE OPERATOR CLASS varchar_tr_ops DEFAULT FOR TYPE varchar     USING table_range AS STORAGE varchar;
    CREATE OPERATOR CLASS bpchar_tr_ops  DEFAULT FOR TYPE bpchar      USING table_range AS STORAGE bpchar;
    CREATE OPERATOR CLASS date_tr_ops    DEFAULT FOR TYPE date        USING table_range AS STORAGE date;
    CREATE OPERATOR CLASS time_tr_ops    DEFAULT FOR TYPE time        USING table_range AS STORAGE time;
    CREATE OPERATOR CLASS timestamp_tr_ops   DEFAULT FOR TYPE timestamp   USING table_range AS STORAGE timestamp;
    CREATE OPERATOR CLASS timestamptz_tr_ops DEFAULT FOR TYPE timestamptz USING table_range AS STORAGE timestamptz;
    CREATE OPERATOR CLASS uuid_tr_ops    DEFAULT FOR TYPE uuid        USING table_range AS STORAGE uuid;
    CREATE OPERATOR CLASS oid_tr_ops     DEFAULT FOR TYPE oid         USING table_range AS STORAGE oid;
    "#,
    name = "table_range_access_method",
    requires = ["table_range_bootstrap_sql"]
);

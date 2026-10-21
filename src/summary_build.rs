use crate::index_am::att_typid;
use crate::index_storage::{ColSummary, IndexSummary};
use crate::prune_hook::{btree_cmp_proc, datum_to_text, text_to_datum};
use pgrx::prelude::*;
use pgrx::PgMemoryContexts;
use std::ffi::{CStr, CString};

// Index-build scan, not SQL: it sees recently-dead tuples and works during REINDEX.

/// `heap_tuples` becomes the heap's `reltuples` via `index_build`.
pub(crate) struct LeafBuild {
    pub summary: IndexSummary,
    pub heap_tuples: f64,
}

// shortcut: geometry extents go through SQL `ST_Extent` per batch; call PostGIS box
// functions directly if geometry builds or inserts become a bottleneck.
const GEOMETRY_BATCH: usize = 1000;

enum Accumulator {
    MinMax {
        cmp: *mut pg_sys::FmgrInfo,
        min: Option<pg_sys::Datum>,
        max: Option<pg_sys::Datum>,
    },
    Range {
        merge: *mut pg_sys::FmgrInfo,
        extent: Option<pg_sys::Datum>,
    },
    Geometry {
        batch: Vec<pg_sys::Datum>,
        extent: Option<String>,
    },
    /// Stored as unbounded.
    Unsupported,
}

struct ColumnBuild {
    /// Index into the build callback's `values`.
    index_pos: usize,
    attnum: i16,
    typ: pg_sys::Oid,
    type_name: String,
    collation: pg_sys::Oid,
    typlen: i16,
    typbyval: bool,
    nonnull: i64,
    nulls: i64,
    acc: Accumulator,
}

struct BuildState {
    cols: Vec<ColumnBuild>,
    cxt: pg_sys::MemoryContext,
}

/// `include_in_progress`: the heap is not locked against writers, so rows of in-progress
/// inserts must be covered too.
pub(crate) unsafe fn build_leaf_summary(
    heap: pg_sys::Relation,
    index: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
    include_in_progress: bool,
) -> LeafBuild {
    let build_cxt = PgMemoryContexts::new("table_range build");
    let cxt = build_cxt.value();
    let mut state = BuildState {
        cols: PgMemoryContexts::For(cxt).switch_to(|_| plan_columns(index, index_info)),
        cxt,
    };

    let scan = (*(*heap).rd_tableam)
        .index_build_range_scan
        .unwrap_or_else(|| error!("table_range: table access method cannot build indexes"));
    let heap_tuples = scan(
        heap,
        index,
        index_info,
        true, // allow_sync
        include_in_progress,
        !include_in_progress, // progress: only a CREATE INDEX / REINDEX reports it
        0,
        pg_sys::InvalidBlockNumber,
        Some(build_callback),
        &mut state as *mut BuildState as *mut std::ffi::c_void,
        std::ptr::null_mut(),
    );

    let summary = crate::stable_io::with_stable_io(|| finish(&mut state))
        .unwrap_or_else(|| error!("table_range: cannot build a summary in parallel mode"));
    drop(build_cxt);
    LeafBuild {
        summary,
        heap_tuples,
    }
}

unsafe fn plan_columns(
    index: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
) -> Vec<ColumnBuild> {
    let nattrs = (*index_info).ii_NumIndexAttrs.max(0) as usize;
    let mut cols = Vec::new();
    for index_pos in 0..nattrs {
        let attnum = (*index_info).ii_IndexAttrNumbers[index_pos];
        if attnum <= 0 {
            continue; // expression column: not summarized, so never used for pruning
        }
        let typ = att_typid((*index).rd_att, index_pos);
        let base = pg_sys::getBaseType(typ);
        let collation = if (*index).rd_indcollation.is_null() {
            pg_sys::Oid::INVALID
        } else {
            *(*index).rd_indcollation.add(index_pos)
        };
        let mut typlen: i16 = 0;
        let mut typbyval = false;
        let mut typalign: std::ffi::c_char = 0;
        pg_sys::get_typlenbyvalalign(typ, &mut typlen, &mut typbyval, &mut typalign);

        let acc = if typlen == -2 {
            Accumulator::Unsupported // cstring-like values are not copied
        } else if pg_sys::get_typtype(base) as u8 == b'r' {
            Accumulator::Range {
                merge: fmgr_in_current(pg_sys::Oid::from(pg_sys::F_RANGE_MERGE_ANYRANGE_ANYRANGE)),
                extent: None,
            }
        } else if type_name_parts(base).is_some_and(|(_, name)| name == "geometry") {
            Accumulator::Geometry {
                batch: Vec::new(),
                extent: None,
            }
        } else {
            match btree_cmp_proc(base) {
                Some(proc) => Accumulator::MinMax {
                    cmp: fmgr_in_current(proc),
                    min: None,
                    max: None,
                },
                None => Accumulator::Unsupported,
            }
        };
        cols.push(ColumnBuild {
            index_pos,
            attnum,
            typ,
            type_name: qualified_type_name(typ),
            collation,
            typlen,
            typbyval,
            nonnull: 0,
            nulls: 0,
            acc,
        });
    }
    cols
}

#[pg_guard]
unsafe extern "C-unwind" fn build_callback(
    _index: pg_sys::Relation,
    _tid: pg_sys::ItemPointer,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    _tuple_is_alive: bool,
    state: *mut std::ffi::c_void,
) {
    // Recently-dead tuples are folded in too: an older snapshot may still see them.
    let state = &mut *(state as *mut BuildState);
    let cxt = state.cxt;
    for col in state.cols.iter_mut() {
        if *isnull.add(col.index_pos) {
            col.nulls += 1;
            continue;
        }
        col.nonnull += 1;
        let value = *values.add(col.index_pos);
        PgMemoryContexts::For(cxt).switch_to(|_| col.add(value));
    }
}

impl ColumnBuild {
    /// `value` lives in the scan's per-tuple context, so anything kept is copied.
    unsafe fn add(&mut self, value: pg_sys::Datum) {
        let (typlen, typbyval, collation) = (self.typlen, self.typbyval, self.collation);
        match &mut self.acc {
            Accumulator::MinMax { cmp, min, max } => match (*min, *max) {
                (Some(lo), Some(hi)) => {
                    if call_cmp(*cmp, collation, value, lo) < 0 {
                        free_datum(lo, typbyval);
                        *min = Some(copy_datum(value, typlen, typbyval));
                    }
                    if call_cmp(*cmp, collation, value, hi) > 0 {
                        free_datum(hi, typbyval);
                        *max = Some(copy_datum(value, typlen, typbyval));
                    }
                }
                _ => {
                    *min = Some(copy_datum(value, typlen, typbyval));
                    *max = Some(copy_datum(value, typlen, typbyval));
                }
            },
            Accumulator::Range { merge, extent } => {
                *extent = Some(match *extent {
                    None => copy_datum(value, typlen, typbyval),
                    Some(old) => {
                        let merged =
                            pg_sys::FunctionCall2Coll(*merge, pg_sys::Oid::INVALID, old, value);
                        // range_merge may hand back one of its inputs unchanged.
                        if merged == value {
                            free_datum(old, typbyval);
                            copy_datum(value, typlen, typbyval)
                        } else {
                            if merged != old {
                                free_datum(old, typbyval);
                            }
                            merged
                        }
                    }
                });
            }
            Accumulator::Geometry { batch, .. } => {
                batch.push(copy_datum(value, typlen, typbyval));
                if batch.len() >= GEOMETRY_BATCH {
                    self.flush_geometry();
                }
            }
            Accumulator::Unsupported => {}
        }
    }

    /// `ST_Extent` over an array parameter, so no table is planned during the build.
    unsafe fn flush_geometry(&mut self) {
        let typ = pg_sys::getBaseType(self.typ);
        let (typlen, typbyval) = (self.typlen, self.typbyval);
        let sql = format!(
            "SELECT {ns}.st_extent(g)::{ty}::pg_catalog.text FROM pg_catalog.unnest($1) AS g",
            ns = geometry_schema(&self.type_name),
            ty = self.type_name
        );
        let Accumulator::Geometry { batch, extent } = &mut self.acc else {
            return;
        };
        if let Some(text) = extent.as_deref() {
            match text_to_datum(typ, text) {
                Some(d) => batch.push(d),
                None => error!("table_range: cannot re-read geometry extent"),
            }
        }
        if batch.is_empty() {
            return;
        }
        let mut typalign: std::ffi::c_char = 0;
        let (mut l, mut b) = (0i16, false);
        pg_sys::get_typlenbyvalalign(typ, &mut l, &mut b, &mut typalign);
        let array = pg_sys::construct_array(
            batch.as_mut_ptr(),
            batch.len() as i32,
            typ,
            typlen as i32,
            typbyval,
            typalign,
        );
        let mut argtype = pg_sys::get_array_type(typ);
        let mut arg = pg_sys::Datum::from(array);
        let cmd = CString::new(sql).expect("SQL contains no NUL bytes");

        pg_sys::PushActiveSnapshot(pg_sys::GetTransactionSnapshot());
        pg_sys::SPI_connect();
        let rc = pg_sys::SPI_execute_with_args(
            cmd.as_ptr(),
            1,
            &mut argtype,
            &mut arg,
            std::ptr::null(),
            true,
            1,
        );
        if rc != pg_sys::SPI_OK_SELECT as i32 {
            error!("table_range: ST_Extent batch failed ({rc})");
        }
        let result = first_value();
        pg_sys::SPI_finish();
        pg_sys::PopActiveSnapshot();

        for d in batch.drain(..) {
            free_datum(d, typbyval);
        }
        pg_sys::pfree(array.cast());
        // NULL: every value was empty, so there is nothing to cover yet.
        *extent = result;
    }
}

unsafe fn finish(state: &mut BuildState) -> IndexSummary {
    let mut summary = IndexSummary::default();
    let cxt = state.cxt;
    for col in state.cols.iter_mut() {
        if matches!(col.acc, Accumulator::Geometry { .. }) {
            PgMemoryContexts::For(cxt).switch_to(|_| col.flush_geometry());
        }
        let (overlap, min, max, unbounded) = match &col.acc {
            Accumulator::MinMax { min, max, .. } => (
                false,
                min.and_then(|d| datum_to_text(col.typ, d)),
                max.and_then(|d| datum_to_text(col.typ, d)),
                false,
            ),
            Accumulator::Range { extent, .. } => (
                true,
                extent.and_then(|d| datum_to_text(col.typ, d)),
                None,
                false,
            ),
            Accumulator::Geometry { extent, .. } => (true, extent.clone(), None, false),
            Accumulator::Unsupported => (false, None, None, true),
        };
        let mut out = ColSummary {
            attnum: col.attnum,
            overlap,
            type_name: col.type_name.clone(),
            collation: col.collation,
            min,
            max,
            has_nulls: col.nulls > 0,
            all_nulls: col.nonnull == 0 && col.nulls > 0,
            unbounded,
        };
        // A bound that could not be rendered as UTF-8 is unknown, not absent.
        let lost_bound = match &col.acc {
            Accumulator::MinMax { .. } => out.min.is_none() || out.max.is_none(),
            Accumulator::Range { .. } => out.min.is_none(),
            _ => false,
        };
        if out.unbounded || (col.nonnull > 0 && lost_bound) {
            out.make_unbounded();
        }
        summary.cols.push(out);
    }
    summary
}

unsafe fn fmgr_in_current(proc: pg_sys::Oid) -> *mut pg_sys::FmgrInfo {
    let finfo = pg_sys::palloc0(std::mem::size_of::<pg_sys::FmgrInfo>()) as *mut pg_sys::FmgrInfo;
    pg_sys::fmgr_info(proc, finfo);
    finfo
}

unsafe fn call_cmp(
    cmp: *mut pg_sys::FmgrInfo,
    collation: pg_sys::Oid,
    a: pg_sys::Datum,
    b: pg_sys::Datum,
) -> i32 {
    pg_sys::FunctionCall2Coll(cmp, collation, a, b).value() as i32
}

pub(crate) unsafe fn copy_datum(
    value: pg_sys::Datum,
    typlen: i16,
    typbyval: bool,
) -> pg_sys::Datum {
    if typbyval {
        value
    } else if typlen == -1 {
        pg_sys::Datum::from(pg_sys::pg_detoast_datum_copy(value.cast_mut_ptr()))
    } else {
        let len = typlen as usize;
        let copy = pg_sys::palloc(len);
        std::ptr::copy_nonoverlapping(value.cast_mut_ptr::<u8>(), copy as *mut u8, len);
        pg_sys::Datum::from(copy)
    }
}

pub(crate) unsafe fn free_datum(value: pg_sys::Datum, typbyval: bool) {
    if !typbyval {
        pg_sys::pfree(value.cast_mut_ptr());
    }
}

unsafe fn type_name_parts(typ: pg_sys::Oid) -> Option<(String, String)> {
    let tup = pg_sys::SearchSysCache1(
        pg_sys::SysCacheIdentifier::TYPEOID as i32,
        pg_sys::Datum::from(typ),
    );
    if tup.is_null() {
        return None;
    }
    let form = pg_sys::heap_tuple_get_struct::<pg_sys::FormData_pg_type>(tup);
    let name = CStr::from_ptr((*form).typname.data.as_ptr())
        .to_string_lossy()
        .into_owned();
    let nsp = pg_sys::get_namespace_name((*form).typnamespace);
    pg_sys::ReleaseSysCache(tup);
    if nsp.is_null() {
        return None;
    }
    Some((CStr::from_ptr(nsp).to_string_lossy().into_owned(), name))
}

unsafe fn qualified_type_name(typ: pg_sys::Oid) -> String {
    let (nsp, name) = type_name_parts(typ)
        .unwrap_or_else(|| error!("table_range: type {} not found", u32::from(typ)));
    let nsp = CString::new(nsp).expect("no NUL in identifier");
    let name = CString::new(name).expect("no NUL in identifier");
    let q = pg_sys::quote_qualified_identifier(nsp.as_ptr(), name.as_ptr());
    CStr::from_ptr(q).to_string_lossy().into_owned()
}

/// PostGIS functions live in the same schema as its `geometry` type.
pub(crate) fn geometry_schema(type_name: &str) -> &str {
    type_name.strip_suffix(".geometry").unwrap_or("public")
}

/// `None` for NULL, no row, or a value that is not valid UTF-8.
unsafe fn first_value() -> Option<String> {
    if pg_sys::SPI_processed == 0 || pg_sys::SPI_tuptable.is_null() {
        return None;
    }
    let table = pg_sys::SPI_tuptable;
    let p = pg_sys::SPI_getvalue(*(*table).vals, (*table).tupdesc, 1);
    if p.is_null() {
        None
    } else {
        CStr::from_ptr(p).to_str().ok().map(str::to_owned)
    }
}

/// Read-only, so SPI uses the active snapshot rather than the transaction snapshot.
pub(crate) unsafe fn spi_value(sql: &str) -> Option<String> {
    let cmd = CString::new(sql).expect("SQL contains no NUL bytes");
    let rc = pg_sys::SPI_execute(cmd.as_ptr(), true, 1);
    if rc != pg_sys::SPI_OK_SELECT as i32 {
        error!("table_range: SPI_execute failed ({rc}): {sql}");
    }
    first_value()
}

/// First column of every row of a read-only statement on the current SPI connection.
pub(crate) unsafe fn spi_column(sql: &str) -> Vec<Option<String>> {
    let cmd = CString::new(sql).expect("SQL contains no NUL bytes");
    if pg_sys::SPI_execute(cmd.as_ptr(), true, 0) != pg_sys::SPI_OK_SELECT as i32 {
        error!("table_range: SPI_execute failed: {sql}");
    }
    let table = pg_sys::SPI_tuptable;
    (0..pg_sys::SPI_processed as usize)
        .map(|r| {
            let p = pg_sys::SPI_getvalue(*(*table).vals.add(r), (*table).tupdesc, 1);
            (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
        })
        .collect()
}

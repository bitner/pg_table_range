use pgrx::prelude::*;
use pgrx::PgMemoryContexts;

use crate::index_am::{index_columns, widen_column};
use crate::index_storage::IndexSummary;
use crate::prune_hook::{btree_cmp_proc, datum_to_text, text_to_datum};
use crate::summary_build::{copy_datum, free_datum};

// The summary decoded once per statement (kept in IndexInfo's ii_AmCache), so an insert
// whose values are already covered only reads the page and compares binary values.

pub(crate) struct InsertCache {
    /// Page bytes the decoded bounds belong to.
    blob: Vec<u8>,
    cols: Vec<CachedCol>,
    /// ii_Context: holds the decoded datums and FmgrInfos for the statement.
    cxt: pg_sys::MemoryContext,
}

struct CachedCol {
    pos: usize,
    attnum: i16,
    typ: pg_sys::Oid,
    collation: pg_sys::Oid,
    typlen: i16,
    typbyval: bool,
    has_nulls: bool,
    all_nulls: bool,
    kind: Kind,
}

enum Kind {
    Unbounded,
    /// `None` while the column has no values.
    MinMax {
        cmp: *mut pg_sys::FmgrInfo,
        bounds: Option<(pg_sys::Datum, pg_sys::Datum)>,
    },
    Range {
        contains: *mut pg_sys::FmgrInfo,
        merge: *mut pg_sys::FmgrInfo,
        extent: Option<pg_sys::Datum>,
    },
    /// Not decoded (geometry, or bounds that failed to parse): always widen_column.
    Opaque,
}

/// The statement's cache slot for this index.
pub(crate) unsafe fn slot(index_info: *mut pg_sys::IndexInfo) -> &'static mut Option<InsertCache> {
    if (*index_info).ii_AmCache.is_null() {
        let p = PgMemoryContexts::For((*index_info).ii_Context)
            .leak_and_drop_on_delete(Option::<InsertCache>::None);
        (*index_info).ii_AmCache = p.cast();
    }
    &mut *((*index_info).ii_AmCache as *mut Option<InsertCache>)
}

impl InsertCache {
    pub(crate) fn matches(&self, blob: &[u8]) -> bool {
        self.blob == blob
    }

    /// Call with type I/O GUCs pinned.
    pub(crate) unsafe fn decode(
        blob: Vec<u8>,
        summary: &IndexSummary,
        index: pg_sys::Relation,
        index_info: *mut pg_sys::IndexInfo,
    ) -> InsertCache {
        let cxt = (*index_info).ii_Context;
        let cols = PgMemoryContexts::For(cxt).switch_to(|_| {
            index_columns(index, index_info)
                .into_iter()
                .filter_map(|c| {
                    let s = summary.cols.iter().find(|s| s.attnum == c.attnum)?;
                    let base = pg_sys::getBaseType(c.typ);
                    let mut typlen: i16 = 0;
                    let mut typbyval = false;
                    pg_sys::get_typlenbyval(c.typ, &mut typlen, &mut typbyval);
                    let parse =
                        |t: &Option<String>| t.as_deref().and_then(|t| text_to_datum(base, t));
                    let kind = if s.unbounded {
                        Kind::Unbounded
                    } else if !s.overlap {
                        match (btree_cmp_proc(base), &s.min, &s.max) {
                            (Some(proc), None, None) => Kind::MinMax {
                                cmp: fmgr(proc, cxt),
                                bounds: None,
                            },
                            (Some(proc), Some(_), Some(_)) => {
                                match (parse(&s.min), parse(&s.max)) {
                                    (Some(lo), Some(hi)) => Kind::MinMax {
                                        cmp: fmgr(proc, cxt),
                                        bounds: Some((lo, hi)),
                                    },
                                    _ => Kind::Opaque,
                                }
                            }
                            _ => Kind::Opaque,
                        }
                    } else if pg_sys::get_typtype(base) as u8 == b'r' {
                        let extent = parse(&s.min);
                        if s.min.is_some() && extent.is_none() {
                            Kind::Opaque
                        } else {
                            Kind::Range {
                                contains: fmgr(pg_sys::Oid::from(pg_sys::F_RANGE_CONTAINS), cxt),
                                merge: fmgr(
                                    pg_sys::Oid::from(pg_sys::F_RANGE_MERGE_ANYRANGE_ANYRANGE),
                                    cxt,
                                ),
                                extent,
                            }
                        }
                    } else {
                        Kind::Opaque
                    };
                    Some(CachedCol {
                        pos: c.pos,
                        attnum: c.attnum,
                        typ: c.typ,
                        collation: c.collation,
                        typlen,
                        typbyval,
                        has_nulls: s.has_nulls,
                        all_nulls: s.all_nulls,
                        kind,
                    })
                })
                .collect()
        });
        InsertCache { blob, cols, cxt }
    }

    /// The row is already covered, so the summary needs no change.
    pub(crate) unsafe fn covers(&self, values: *mut pg_sys::Datum, isnull: *mut bool) -> bool {
        self.cols.iter().all(|c| {
            if *isnull.add(c.pos) {
                return c.has_nulls;
            }
            let v = *values.add(c.pos);
            !c.all_nulls
                && match &c.kind {
                    Kind::Unbounded => true,
                    Kind::MinMax {
                        cmp,
                        bounds: Some((lo, hi)),
                    } => {
                        call(*cmp, c.collation, v, *lo) >= 0 && call(*cmp, c.collation, v, *hi) <= 0
                    }
                    Kind::Range {
                        contains,
                        extent: Some(e),
                        ..
                    } => call(*contains, pg_sys::Oid::INVALID, *e, v) & 0xff != 0,
                    _ => false,
                }
        })
    }

    /// Widen `summary` (decoded from this cache's blob) to cover the row, outputting only
    /// the new bounds' text. Returns whether it changed. Call with type I/O GUCs pinned.
    pub(crate) unsafe fn widen(
        &self,
        summary: &mut IndexSummary,
        values: *mut pg_sys::Datum,
        isnull: *mut bool,
    ) -> bool {
        let mut changed = false;
        for c in &self.cols {
            let Some(s) = summary.cols.iter_mut().find(|s| s.attnum == c.attnum) else {
                continue;
            };
            let (v, null) = (*values.add(c.pos), *isnull.add(c.pos));
            let text = |d| datum_to_text(c.typ, d);
            match &c.kind {
                _ if null || s.unbounded => {
                    changed |= widen_column(s, c.typ, c.collation, v, null);
                }
                Kind::MinMax { cmp, bounds } => {
                    if s.all_nulls {
                        s.all_nulls = false;
                        changed = true;
                    }
                    let (low, high) = match bounds {
                        Some((lo, hi)) => (
                            call(*cmp, c.collation, v, *lo) < 0,
                            call(*cmp, c.collation, v, *hi) > 0,
                        ),
                        None => (true, true),
                    };
                    if low || high {
                        match text(v) {
                            Some(t) => {
                                if low {
                                    s.min = Some(t.clone());
                                }
                                if high {
                                    s.max = Some(t);
                                }
                            }
                            None => s.make_unbounded(),
                        }
                        changed = true;
                    }
                }
                Kind::Range {
                    contains,
                    merge,
                    extent,
                } => {
                    if s.all_nulls {
                        s.all_nulls = false;
                        changed = true;
                    }
                    let covered = extent
                        .is_some_and(|e| call(*contains, pg_sys::Oid::INVALID, e, v) & 0xff != 0);
                    if !covered {
                        let merged = match extent {
                            Some(e) => call_datum(*merge, *e, v),
                            None => v,
                        };
                        match text(merged) {
                            Some(t) => s.min = Some(t),
                            None => s.make_unbounded(),
                        }
                        changed = true;
                    }
                }
                Kind::Unbounded | Kind::Opaque => {
                    changed |= widen_column(s, c.typ, c.collation, v, null);
                }
            }
        }
        changed
    }

    /// After writing `summary` (as `blob`) for this row, update the decoded bounds from the
    /// row's own values instead of re-parsing.
    pub(crate) unsafe fn absorb(
        &mut self,
        blob: Vec<u8>,
        summary: &IndexSummary,
        values: *mut pg_sys::Datum,
        isnull: *mut bool,
    ) {
        let cxt = self.cxt;
        PgMemoryContexts::For(cxt).switch_to(|_| {
            for c in &mut self.cols {
                let Some(s) = summary.cols.iter().find(|s| s.attnum == c.attnum) else {
                    continue;
                };
                c.has_nulls = s.has_nulls;
                c.all_nulls = s.all_nulls;
                if s.unbounded {
                    c.kind = Kind::Unbounded;
                    continue;
                }
                if *isnull.add(c.pos) {
                    continue;
                }
                let v = *values.add(c.pos);
                let (typlen, typbyval) = (c.typlen, c.typbyval);
                match &mut c.kind {
                    Kind::MinMax { cmp, bounds } => match bounds {
                        None => {
                            *bounds = Some((
                                copy_datum(v, typlen, typbyval),
                                copy_datum(v, typlen, typbyval),
                            ))
                        }
                        Some((lo, hi)) => {
                            if call(*cmp, c.collation, v, *lo) < 0 {
                                free_datum(*lo, typbyval);
                                *lo = copy_datum(v, typlen, typbyval);
                            }
                            if call(*cmp, c.collation, v, *hi) > 0 {
                                free_datum(*hi, typbyval);
                                *hi = copy_datum(v, typlen, typbyval);
                            }
                        }
                    },
                    Kind::Range {
                        contains,
                        merge,
                        extent,
                    } => {
                        *extent = Some(match *extent {
                            None => copy_datum(v, typlen, typbyval),
                            Some(e) if call(*contains, pg_sys::Oid::INVALID, e, v) & 0xff != 0 => e,
                            Some(e) => {
                                let merged = call_datum(*merge, e, v);
                                copy_datum(merged, typlen, typbyval)
                            }
                        });
                    }
                    Kind::Unbounded | Kind::Opaque => {}
                }
            }
        });
        self.blob = blob;
    }
}

unsafe fn fmgr(proc: pg_sys::Oid, cxt: pg_sys::MemoryContext) -> *mut pg_sys::FmgrInfo {
    let finfo = pg_sys::MemoryContextAllocZero(cxt, std::mem::size_of::<pg_sys::FmgrInfo>())
        as *mut pg_sys::FmgrInfo;
    pg_sys::fmgr_info_cxt(proc, finfo, cxt);
    finfo
}

unsafe fn call_datum(
    f: *mut pg_sys::FmgrInfo,
    a: pg_sys::Datum,
    b: pg_sys::Datum,
) -> pg_sys::Datum {
    pg_sys::FunctionCall2Coll(f, pg_sys::Oid::INVALID, a, b)
}

/// For procs returning int32 (btree compare) or bool (`& 0xff`).
unsafe fn call(
    f: *mut pg_sys::FmgrInfo,
    collation: pg_sys::Oid,
    a: pg_sys::Datum,
    b: pg_sys::Datum,
) -> i32 {
    pg_sys::FunctionCall2Coll(f, collation, a, b).value() as i32
}

use pgrx::prelude::*;
use pgrx::PgMemoryContexts;

use crate::index_am::{index_columns, widen_column};
use crate::index_storage::IndexSummary;
use crate::prune_hook::{btree_cmp_proc, datum_to_text, text_to_datum};
use crate::summary_build::{
    call_cmp, copy_datum, fmgr_in_current, free_datum, is_geometry, type_name_parts,
};

// The summary decoded once per statement (kept in IndexInfo's ii_AmCache), so an insert
// whose values are already covered only reads the page and compares binary values.

pub(crate) struct InsertCache {
    /// The page bytes everything below was decoded from.
    pub(crate) blob: Vec<u8>,
    pub(crate) summary: IndexSummary,
    cols: Vec<CachedCol>,
    /// Child of ii_Context holding the decoded datums; deleted when the cache is replaced.
    cxt: pg_sys::MemoryContext,
}

struct CachedCol {
    pos: usize,
    /// Position of this column in `summary.cols`.
    summary_pos: usize,
    typ: pg_sys::Oid,
    collation: pg_sys::Oid,
    typlen: i16,
    typbyval: bool,
    kind: Kind,
}

enum Kind {
    Unbounded,
    /// `bounds` is `None` while the column has no values.
    MinMax {
        cmp: *mut pg_sys::FmgrInfo,
        bounds: Option<(pg_sys::Datum, pg_sys::Datum)>,
    },
    /// `extent` is `None` while the column has no values.
    Range {
        contains: *mut pg_sys::FmgrInfo,
        merge: *mut pg_sys::FmgrInfo,
        extent: Option<pg_sys::Datum>,
    },
    /// Covered rows are checked with PostGIS's bounding-box `~`; widening uses `ST_Extent`.
    Geometry {
        contains: *mut pg_sys::FmgrInfo,
        extent: Option<pg_sys::Datum>,
    },
    /// Bounds that do not parse back: always widen through `widen_column`.
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

/// Empty the slot, freeing the decoded datums.
pub(crate) unsafe fn forget(slot: &mut Option<InsertCache>) {
    if let Some(old) = slot.take() {
        pg_sys::MemoryContextDelete(old.cxt);
    }
}

impl InsertCache {
    /// Call with type I/O GUCs pinned.
    pub(crate) unsafe fn decode(
        blob: Vec<u8>,
        summary: IndexSummary,
        index: pg_sys::Relation,
        index_info: *mut pg_sys::IndexInfo,
    ) -> InsertCache {
        let cxt = pg_sys::AllocSetContextCreateExtended(
            (*index_info).ii_Context,
            c"table_range insert cache".as_ptr(),
            pg_sys::ALLOCSET_DEFAULT_MINSIZE as usize,
            pg_sys::ALLOCSET_DEFAULT_INITSIZE as usize,
            pg_sys::ALLOCSET_DEFAULT_MAXSIZE as usize,
        );
        let cols = PgMemoryContexts::For(cxt).switch_to(|_| {
            index_columns(index, index_info)
                .into_iter()
                .filter_map(|c| {
                    let summary_pos = summary.cols.iter().position(|s| s.attnum == c.attnum)?;
                    let s = &summary.cols[summary_pos];
                    let base = pg_sys::getBaseType(c.typ);
                    let mut typlen: i16 = 0;
                    let mut typbyval = false;
                    pg_sys::get_typlenbyval(c.typ, &mut typlen, &mut typbyval);
                    let parse = |t: &Option<String>| match t {
                        None => Some(None),
                        Some(t) => text_to_datum(base, t).map(Some),
                    };
                    let kind = match (parse(&s.min), parse(&s.max)) {
                        _ if s.unbounded => Kind::Unbounded,
                        (Some(lo), Some(hi)) if !s.overlap => {
                            match (btree_cmp_proc(base), lo, hi) {
                                (Some(p), Some(lo), Some(hi)) => Kind::MinMax {
                                    cmp: fmgr_in_current(p),
                                    bounds: Some((lo, hi)),
                                },
                                (Some(p), None, None) => Kind::MinMax {
                                    cmp: fmgr_in_current(p),
                                    bounds: None,
                                },
                                _ => Kind::Opaque,
                            }
                        }
                        (Some(extent), _) if pg_sys::get_typtype(base) as u8 == b'r' => {
                            Kind::Range {
                                contains: fmgr_in_current(pg_sys::Oid::from(
                                    pg_sys::F_RANGE_CONTAINS,
                                )),
                                merge: fmgr_in_current(pg_sys::Oid::from(
                                    pg_sys::F_RANGE_MERGE_ANYRANGE_ANYRANGE,
                                )),
                                extent,
                            }
                        }
                        (Some(extent), _) => match geometry_contains_proc(base) {
                            Some(p) => Kind::Geometry {
                                contains: fmgr_in_current(p),
                                extent,
                            },
                            None => Kind::Opaque,
                        },
                        _ => Kind::Opaque,
                    };
                    Some(CachedCol {
                        pos: c.pos,
                        summary_pos,
                        typ: c.typ,
                        collation: c.collation,
                        typlen,
                        typbyval,
                        kind,
                    })
                })
                .collect()
        });
        InsertCache {
            blob,
            summary,
            cols,
            cxt,
        }
    }

    /// The row is already covered, so the summary needs no change.
    pub(crate) unsafe fn covers(&self, values: *mut pg_sys::Datum, isnull: *mut bool) -> bool {
        self.cols.iter().all(|c| {
            let s = &self.summary.cols[c.summary_pos];
            if *isnull.add(c.pos) {
                return s.has_nulls;
            }
            let v = *values.add(c.pos);
            // An all-null column must still record its first value.
            !s.all_nulls
                && match &c.kind {
                    Kind::Unbounded => true,
                    Kind::MinMax {
                        cmp,
                        bounds: Some((lo, hi)),
                    } => {
                        call_cmp(*cmp, c.collation, v, *lo) >= 0
                            && call_cmp(*cmp, c.collation, v, *hi) <= 0
                    }
                    Kind::Range {
                        contains,
                        extent: Some(e),
                        ..
                    }
                    | Kind::Geometry {
                        contains,
                        extent: Some(e),
                    } => test(*contains, *e, v),
                    _ => false,
                }
        })
    }

    /// Widen `summary` (a copy of `self.summary`) to cover the row, updating the decoded
    /// bounds to match. Returns whether it changed. If the caller cannot write the result,
    /// it must `forget` this cache. Call with type I/O GUCs pinned.
    pub(crate) unsafe fn widen(
        &mut self,
        summary: &mut IndexSummary,
        values: *mut pg_sys::Datum,
        isnull: *mut bool,
    ) -> bool {
        let cxt = self.cxt;
        PgMemoryContexts::For(cxt).switch_to(|_| {
            let mut changed = false;
            for c in &mut self.cols {
                let s = &mut summary.cols[c.summary_pos];
                let (v, null) = (*values.add(c.pos), *isnull.add(c.pos));
                if null || s.unbounded || matches!(c.kind, Kind::Unbounded | Kind::Opaque) {
                    changed |= widen_column(s, c.typ, c.collation, v, null);
                    continue;
                }
                if s.all_nulls {
                    s.all_nulls = false;
                    changed = true;
                }
                let (typ, typlen, typbyval) = (c.typ, c.typlen, c.typbyval);
                let copy = |d| copy_datum(d, typlen, typbyval);
                changed |= match &mut c.kind {
                    Kind::MinMax { cmp, bounds } => {
                        let (low, high) = match bounds {
                            Some((lo, hi)) => (
                                call_cmp(*cmp, c.collation, v, *lo) < 0,
                                call_cmp(*cmp, c.collation, v, *hi) > 0,
                            ),
                            None => (true, true),
                        };
                        match datum_to_text(typ, v) {
                            _ if !(low || high) => false,
                            None => {
                                s.make_unbounded();
                                true
                            }
                            Some(text) => {
                                match bounds {
                                    None => *bounds = Some((copy(v), copy(v))),
                                    Some((lo, hi)) => {
                                        for (side, moved) in [(lo, low), (hi, high)] {
                                            if moved {
                                                free_datum(*side, typbyval);
                                                *side = copy(v);
                                            }
                                        }
                                    }
                                }
                                if low {
                                    s.min = Some(text.clone());
                                }
                                if high {
                                    s.max = Some(text);
                                }
                                true
                            }
                        }
                    }
                    Kind::Range {
                        contains,
                        merge,
                        extent,
                    } => match *extent {
                        Some(e) if test(*contains, e, v) => false,
                        old => {
                            let merged = match old {
                                Some(e) => {
                                    // range_merge may return one of its inputs as is.
                                    let m = call(*merge, e, v);
                                    if m == e || m == v {
                                        copy(m)
                                    } else {
                                        m
                                    }
                                }
                                None => copy(v),
                            };
                            match datum_to_text(typ, merged) {
                                Some(text) => s.min = Some(text),
                                None => s.make_unbounded(),
                            }
                            if let Some(e) = old {
                                free_datum(e, typbyval);
                            }
                            *extent = Some(merged);
                            true
                        }
                    },
                    Kind::Geometry { contains, extent } => match *extent {
                        Some(e) if test(*contains, e, v) => false,
                        old => {
                            let widened = widen_column(s, typ, c.collation, v, false);
                            if widened {
                                // ST_Extent produced the new extent; parse it (hex, GUC-free).
                                if let Some(e) = old {
                                    free_datum(e, typbyval);
                                }
                                *extent = s
                                    .min
                                    .as_deref()
                                    .and_then(|t| text_to_datum(pg_sys::getBaseType(typ), t));
                            }
                            widened
                        }
                    },
                    Kind::Unbounded | Kind::Opaque => unreachable!("handled above"),
                };
                if s.unbounded {
                    c.kind = Kind::Unbounded;
                }
            }
            changed
        })
    }

    /// `summary` (as `blob`) was written: it is now what the page holds.
    pub(crate) fn wrote(&mut self, blob: Vec<u8>, summary: IndexSummary) {
        for c in &mut self.cols {
            // encode_fitting may have dropped bounds that did not fit on the page.
            if summary.cols[c.summary_pos].unbounded {
                c.kind = Kind::Unbounded;
            }
        }
        self.blob = blob;
        self.summary = summary;
    }
}

/// PostGIS's bounding-box `~` for its `geometry` type, looked up in that type's schema.
unsafe fn geometry_contains_proc(base: pg_sys::Oid) -> Option<pg_sys::Oid> {
    if !is_geometry(base) {
        return None;
    }
    let nsp = std::ffi::CString::new(type_name_parts(base)?.0).ok()?;
    let mut names = pgrx::PgList::<pg_sys::String>::new();
    names.push(pg_sys::makeString(pg_sys::pstrdup(nsp.as_ptr())));
    names.push(pg_sys::makeString(pg_sys::pstrdup(c"~".as_ptr())));
    let op = pg_sys::OpernameGetOprid(names.into_pg(), base, base);
    (op != pg_sys::Oid::INVALID).then(|| pg_sys::get_opcode(op))
}

unsafe fn call(f: *mut pg_sys::FmgrInfo, a: pg_sys::Datum, b: pg_sys::Datum) -> pg_sys::Datum {
    pg_sys::FunctionCall2Coll(f, pg_sys::Oid::INVALID, a, b)
}

/// A boolean proc's result (bool is the low byte of the Datum).
unsafe fn test(f: *mut pg_sys::FmgrInfo, a: pg_sys::Datum, b: pg_sys::Datum) -> bool {
    call(f, a, b).value() & 0xff != 0
}

use pgrx::pg_sys;
use pgrx::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, CString};

use crate::index_storage::ColSummary;
use crate::{TABLE_RANGE_ENABLE_PRUNING, TABLE_RANGE_LOG_PRUNING_DEBUG};

// Real planner-time partition pruning via `set_rel_pathlist_hook`, with a per-plan
// summary cache driven by `planner_hook`.
//
// For every base/partition-member relation the planner builds, we look up that
// relation's cached summaries and evaluate its restriction clauses (`Var op Const`,
// `Var IS [NOT] NULL`, `Var IN (...)`). Typed comparisons are performed in-memory with
// the column type's btree compare support function, so any error (e.g. an
// incompatible cast) is caught and degrades to KEEP. If any single clause proves the
// partition cannot contain a matching row, we call `mark_dummy_rel` so the planner
// eliminates it before generating child paths.
//
// Each partition's summary is read from its table_range index's metapage and cached for
// the duration of one top-level planner invocation.

extern "C" {
    fn mark_dummy_rel(rel: *mut pg_sys::RelOptInfo);
}

/// btree "compare" support function number (BTORDER_PROC).
const BTORDER_PROC: u16 = 1;

static mut PREV_PATHLIST_HOOK: pg_sys::set_rel_pathlist_hook_type = None;
static mut PREV_PLANNER_HOOK: pg_sys::planner_hook_type = None;

/// Per-partition summaries read from each partition's index, cached for one planner
/// invocation (keyed by partition relid). A relid present with an empty vec means
/// "checked, no table_range index / no summary".
type SummaryMap = HashMap<u32, Vec<ColSummary>>;

thread_local! {
    /// Summaries read during the current planner invocation. Cleared per top-level plan.
    static CACHE: RefCell<SummaryMap> = RefCell::new(HashMap::new());
    /// The table_range access-method OID, resolved once per plan.
    static AM_OID: Cell<Option<pg_sys::Oid>> = const { Cell::new(None) };
    /// Nesting depth of planner invocations (SPI during planning re-enters).
    static PLAN_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// Guards against re-entering pruning logic from the SPI overlap evaluation issues.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
    /// Per-plan memo of each column type's btree compare proc OID (the lookup is three
    /// syscache hits, identical for every partition of a column, so we do it once).
    static CMP_PROC_MEMO: RefCell<HashMap<u32, Option<pg_sys::Oid>>> = RefCell::new(HashMap::new());
    /// Per-plan memo of parsed query constants, keyed by (type oid, text). The same
    /// constant is otherwise re-rendered and re-parsed once per partition.
    static CONST_MEMO: RefCell<HashMap<(u32, String), pg_sys::Datum>> = RefCell::new(HashMap::new());
}

/// Compare proc for `typ`, memoized for the current plan. See [`btree_cmp_proc`].
unsafe fn cmp_proc_cached(typ: pg_sys::Oid) -> Option<pg_sys::Oid> {
    let key: u32 = typ.into();
    if let Some(v) = CMP_PROC_MEMO.with(|m| m.borrow().get(&key).copied()) {
        return v;
    }
    let v = btree_cmp_proc(typ);
    CMP_PROC_MEMO.with(|m| {
        m.borrow_mut().insert(key, v);
    });
    v
}

/// Parse `text` to a Datum of `typ`, memoized for the current plan. The cached Datum is
/// allocated in the planner's memory context (which outlives the plan) and the memo is
/// cleared per top-level plan, so the pointer stays valid for its lifetime.
unsafe fn const_datum_cached(typ: pg_sys::Oid, text: &str) -> Option<pg_sys::Datum> {
    let key = (typ.into(), text.to_string());
    if let Some(d) = CONST_MEMO.with(|m| m.borrow().get(&key).copied()) {
        return Some(d);
    }
    let d = text_to_datum(typ, text)?;
    CONST_MEMO.with(|m| {
        m.borrow_mut().insert(key, d);
    });
    Some(d)
}

/// Install our planner and pathlist hooks, preserving any previously-registered hooks.
pub fn install() {
    unsafe {
        PREV_PATHLIST_HOOK = pg_sys::set_rel_pathlist_hook;
        pg_sys::set_rel_pathlist_hook = Some(table_range_pathlist_hook);
        PREV_PLANNER_HOOK = pg_sys::planner_hook;
        pg_sys::planner_hook = Some(table_range_planner_hook);
    }
}

unsafe extern "C-unwind" fn table_range_planner_hook(
    parse: *mut pg_sys::Query,
    query_string: *const std::ffi::c_char,
    cursor_options: i32,
    bound_params: pg_sys::ParamListInfo,
) -> *mut pg_sys::PlannedStmt {
    // Invalidate the cache when entering the outermost planner invocation.
    let outermost = PLAN_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v == 0
    });
    if outermost {
        clear_cache();
    }

    let result = match PREV_PLANNER_HOOK {
        Some(prev) => prev(parse, query_string, cursor_options, bound_params),
        None => pg_sys::standard_planner(parse, query_string, cursor_options, bound_params),
    };

    let last = PLAN_DEPTH.with(|d| {
        let v = d.get() - 1;
        d.set(v);
        v == 0
    });
    if last {
        clear_cache();
    }
    result
}

unsafe extern "C-unwind" fn table_range_pathlist_hook(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    rti: pg_sys::Index,
    rte: *mut pg_sys::RangeTblEntry,
) {
    if let Some(prev) = PREV_PATHLIST_HOOK {
        prev(root, rel, rti, rte);
    }

    if IN_HOOK.with(|f| f.get()) {
        return;
    }
    if !TABLE_RANGE_ENABLE_PRUNING.get() || rel.is_null() || rte.is_null() {
        return;
    }
    let reloptkind = (*rel).reloptkind;
    if reloptkind != pg_sys::RelOptKind::RELOPT_BASEREL
        && reloptkind != pg_sys::RelOptKind::RELOPT_OTHER_MEMBER_REL
    {
        return;
    }
    if (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION {
        return;
    }
    let relid = (*rte).relid;
    if relid == pg_sys::Oid::INVALID {
        return;
    }

    IN_HOOK.with(|f| f.set(true));
    let prune = pgrx::PgTryBuilder::new(|| evaluate_relation(rel, relid))
        .catch_others(|_| false)
        .execute();
    IN_HOOK.with(|f| f.set(false));

    if prune {
        if TABLE_RANGE_LOG_PRUNING_DEBUG.get() {
            let oid: u32 = relid.into();
            pgrx::log!("table_range: pruning partition relid={oid}");
        }
        mark_dummy_rel(rel);
    }
}

fn clear_cache() {
    CACHE.with(|c| c.borrow_mut().clear());
    AM_OID.with(|c| c.set(None));
    CMP_PROC_MEMO.with(|c| c.borrow_mut().clear());
    CONST_MEMO.with(|c| c.borrow_mut().clear());
}

/// The table_range access-method OID, resolved once per planner invocation.
unsafe fn table_range_am_oid() -> pg_sys::Oid {
    if let Some(oid) = AM_OID.with(|c| c.get()) {
        return oid;
    }
    let oid = pg_sys::get_am_oid(c"table_range".as_ptr(), true);
    AM_OID.with(|c| c.set(Some(oid)));
    oid
}

/// Read the partition's summary from its table_range index's metapage (the index is
/// found in the relation's index list and is already locked by the planner). The result
/// is cached for this plan. An empty vec means "no table_range index / no summary".
unsafe fn load_summary(rel: *mut pg_sys::RelOptInfo, relid_u32: u32) {
    if CACHE.with(|c| c.borrow().contains_key(&relid_u32)) {
        return;
    }
    let cols = read_index_summary(rel);
    CACHE.with(|c| {
        c.borrow_mut().insert(relid_u32, cols);
    });
}

// We rely on the planner having put the table_range index into `rel->indexlist`. The
// planner only lists indexes with `indisvalid = true`, so a table_range index must be
// valid for pruning to engage — if anything marks it invalid (e.g. an external
// "hide indexes" DDL hook), `indexlist` omits it and we silently fall back to KEEP.
unsafe fn read_index_summary(rel: *mut pg_sys::RelOptInfo) -> Vec<ColSummary> {
    let am = table_range_am_oid();
    if am == pg_sys::Oid::INVALID || (*rel).indexlist.is_null() {
        return Vec::new();
    }
    let indexes = pgrx::PgList::<pg_sys::IndexOptInfo>::from_pg((*rel).indexlist);
    for idx in indexes.iter_ptr() {
        if idx.is_null() || (*idx).relam != am {
            continue;
        }
        let irel = pg_sys::index_open((*idx).indexoid, pg_sys::AccessShareLock as i32);
        // A partitioned (parent) index has no storage; only leaf indexes hold summaries.
        let has_storage =
            (*(*irel).rd_rel).relkind != pg_sys::RELKIND_PARTITIONED_INDEX as std::ffi::c_char;
        let summary = if has_storage {
            crate::index_storage::read_summary(irel)
        } else {
            None
        };
        pg_sys::index_close(irel, pg_sys::AccessShareLock as i32);
        return summary.map(|s| s.cols).unwrap_or_default();
    }
    Vec::new()
}

/// Returns true iff some restriction clause proves the partition cannot match.
unsafe fn evaluate_relation(rel: *mut pg_sys::RelOptInfo, relid: pg_sys::Oid) -> bool {
    let relid_u32: u32 = relid.into();
    load_summary(rel, relid_u32);

    let restrictlist = (*rel).baserestrictinfo;
    if restrictlist.is_null() {
        return false;
    }
    let restrictinfos = pgrx::PgList::<pg_sys::RestrictInfo>::from_pg(restrictlist);

    CACHE.with(|c| {
        let borrow = c.borrow();
        let rows = match borrow.get(&relid_u32) {
            Some(rows) if !rows.is_empty() => rows,
            _ => return false, // no summary -> never prune
        };

        // Top-level restriction clauses are implicitly AND-ed: if any one clause proves
        // the partition cannot match, the partition is pruned.
        for ri in restrictinfos.iter_ptr() {
            if ri.is_null() {
                continue;
            }
            let clause = (*ri).clause;
            if !clause.is_null() && clause_proves_prune(clause as *mut pg_sys::Node, rows, 0) {
                return true;
            }
        }
        false
    })
}

/// Recursively decide whether a clause proves the partition cannot contain a matching
/// row. Boolean structure composes the leaf decisions:
///   - `AND(xs)` prunes if **any** child prunes,
///   - `OR(xs)`  prunes only if **every** child prunes,
///   - `NOT(..)` and unknown shapes are conservative (do not prune).
unsafe fn clause_proves_prune(node: *mut pg_sys::Node, rows: &[ColSummary], depth: u32) -> bool {
    if node.is_null() || depth > 32 {
        return false;
    }
    if (*node).type_ == pg_sys::NodeTag::T_BoolExpr {
        let boolexpr = node as *mut pg_sys::BoolExpr;
        let args = pgrx::PgList::<pg_sys::Node>::from_pg((*boolexpr).args);
        match (*boolexpr).boolop {
            pg_sys::BoolExprType::AND_EXPR => args
                .iter_ptr()
                .any(|child| clause_proves_prune(child, rows, depth + 1)),
            pg_sys::BoolExprType::OR_EXPR => {
                !args.is_empty()
                    && args
                        .iter_ptr()
                        .all(|child| clause_proves_prune(child, rows, depth + 1))
            }
            _ => false, // NOT_EXPR: conservative
        }
    } else if let Some(spec) = extract_qual(node) {
        rows.iter()
            .find(|r| r.attnum == spec.attnum())
            .map(|row| spec.proves_prune(row))
            .unwrap_or(false)
    } else {
        false
    }
}

// ---------------------------------------------------------------------------------
// Clause extraction
// ---------------------------------------------------------------------------------

enum QualSpec {
    /// `Var op Const`: prune decision from the column-typed btree comparison.
    Compare {
        attnum: i16,
        strategy: i16,
        typ: pg_sys::Oid,
        collation: pg_sys::Oid,
        const_text: String,
    },
    /// `Var IS [NOT] NULL`: decided from the summary's null flags.
    Null { attnum: i16, is_null: bool },
    /// `Var IN (const array)` / `= ANY`: prune iff no listed value falls in [min, max].
    InList {
        attnum: i16,
        typ: pg_sys::Oid,
        collation: pg_sys::Oid,
        elems: Vec<Option<String>>,
    },
    /// `Var && Const` (overlap): prune iff the partition's extent does not overlap the
    /// constant. Used for range types and PostGIS geometry.
    Overlap {
        attnum: i16,
        const_text: String,
        const_type_name: String,
    },
}

impl QualSpec {
    fn attnum(&self) -> i16 {
        match self {
            QualSpec::Compare { attnum, .. }
            | QualSpec::Null { attnum, .. }
            | QualSpec::InList { attnum, .. }
            | QualSpec::Overlap { attnum, .. } => *attnum,
        }
    }

    /// Evaluate whether this clause proves the partition cannot match.
    unsafe fn proves_prune(&self, row: &ColSummary) -> bool {
        match self {
            // Null flags are valid for both minmax and overlap summaries.
            QualSpec::Null { is_null, .. } => {
                if *is_null {
                    !row.has_nulls
                } else {
                    row.all_nulls
                }
            }
            // Scalar comparisons only apply to btree min/max summaries.
            QualSpec::Compare {
                strategy,
                typ,
                collation,
                const_text,
                ..
            } => !row.overlap && eval_compare(row, *strategy, *typ, *collation, const_text),
            QualSpec::InList {
                typ,
                collation,
                elems,
                ..
            } => !row.overlap && eval_in_list(row, *typ, *collation, elems),
            // Overlap only applies to extent summaries.
            QualSpec::Overlap {
                const_text,
                const_type_name,
                ..
            } => row.overlap && eval_overlap(row, const_text, const_type_name),
        }
    }
}

unsafe fn extract_qual(node: *mut pg_sys::Node) -> Option<QualSpec> {
    match (*node).type_ {
        pg_sys::NodeTag::T_OpExpr => extract_opexpr(node as *mut pg_sys::OpExpr),
        pg_sys::NodeTag::T_NullTest => extract_nulltest(node as *mut pg_sys::NullTest),
        pg_sys::NodeTag::T_ScalarArrayOpExpr => {
            extract_saop(node as *mut pg_sys::ScalarArrayOpExpr)
        }
        _ => None,
    }
}

unsafe fn extract_opexpr(opexpr: *mut pg_sys::OpExpr) -> Option<QualSpec> {
    let args = pgrx::PgList::<pg_sys::Node>::from_pg((*opexpr).args);
    if args.len() != 2 {
        return None;
    }
    let left = strip_relabel(args.get_ptr(0)?);
    let right = strip_relabel(args.get_ptr(1)?);

    let (var, con, commuted) =
        if is_tag(left, pg_sys::NodeTag::T_Var) && is_tag(right, pg_sys::NodeTag::T_Const) {
            (left as *mut pg_sys::Var, right as *mut pg_sys::Const, false)
        } else if is_tag(left, pg_sys::NodeTag::T_Const) && is_tag(right, pg_sys::NodeTag::T_Var) {
            (right as *mut pg_sys::Var, left as *mut pg_sys::Const, true)
        } else {
            return None;
        };

    let attnum = (*var).varattno;
    if attnum <= 0 || (*con).constisnull {
        return None;
    }

    // Scalar btree comparison (`<`, `<=`, `=`, `>=`, `>`).
    if let Some(strategy) = btree_strategy((*opexpr).opno, (*var).vartype) {
        let strategy = if commuted {
            commute_strategy(strategy)
        } else {
            strategy
        };
        if (1..=5).contains(&strategy) {
            let const_text = datum_to_text((*con).consttype, (*con).constvalue)?;
            return Some(QualSpec::Compare {
                attnum,
                strategy,
                typ: (*var).vartype,
                collation: (*opexpr).inputcollid,
                const_text,
            });
        }
        return None;
    }

    // Overlap operator (`&&`) for range types / PostGIS geometry. `&&` is commutative,
    // so operand order does not matter.
    if operator_name((*opexpr).opno).as_deref() == Some("&&") {
        let const_text = datum_to_text((*con).consttype, (*con).constvalue)?;
        let const_type_name = type_name((*con).consttype)?;
        return Some(QualSpec::Overlap {
            attnum,
            const_text,
            const_type_name,
        });
    }
    None
}

unsafe fn extract_nulltest(nulltest: *mut pg_sys::NullTest) -> Option<QualSpec> {
    let arg = strip_relabel((*nulltest).arg as *mut pg_sys::Node);
    if !is_tag(arg, pg_sys::NodeTag::T_Var) {
        return None;
    }
    let var = arg as *mut pg_sys::Var;
    let attnum = (*var).varattno;
    if attnum <= 0 {
        return None;
    }
    let is_null = match (*nulltest).nulltesttype {
        pg_sys::NullTestType::IS_NULL => true,
        pg_sys::NullTestType::IS_NOT_NULL => false,
        _ => return None,
    };
    Some(QualSpec::Null { attnum, is_null })
}

unsafe fn extract_saop(saop: *mut pg_sys::ScalarArrayOpExpr) -> Option<QualSpec> {
    if !(*saop).useOr {
        return None;
    }
    let args = pgrx::PgList::<pg_sys::Node>::from_pg((*saop).args);
    if args.len() != 2 {
        return None;
    }
    let left = strip_relabel(args.get_ptr(0)?);
    let right = strip_relabel(args.get_ptr(1)?);
    if !is_tag(left, pg_sys::NodeTag::T_Var) || !is_tag(right, pg_sys::NodeTag::T_Const) {
        return None;
    }
    let var = left as *mut pg_sys::Var;
    let con = right as *mut pg_sys::Const;
    let attnum = (*var).varattno;
    if attnum <= 0 || (*con).constisnull {
        return None;
    }
    // Only the equality operator gives the "any element in range" semantics.
    if btree_strategy((*saop).opno, (*var).vartype)? != 3 {
        return None;
    }
    let elems = array_const_texts(con)?;
    Some(QualSpec::InList {
        attnum,
        typ: (*var).vartype,
        collation: (*saop).inputcollid,
        elems,
    })
}

// ---------------------------------------------------------------------------------
// Typed in-memory evaluation
// ---------------------------------------------------------------------------------

unsafe fn eval_compare(
    row: &ColSummary,
    strategy: i16,
    typ: pg_sys::Oid,
    collation: pg_sys::Oid,
    const_text: &str,
) -> bool {
    let (min, max) = match (&row.min, &row.max) {
        (Some(min), Some(max)) => (min, max),
        _ => return false, // no usable range -> conservative KEEP
    };
    let cmpproc = match cmp_proc_cached(typ) {
        Some(p) => p,
        None => return false,
    };
    let k = match const_datum_cached(typ, const_text) {
        Some(d) => d,
        None => return false,
    };
    let min_d = match text_to_datum(typ, min) {
        Some(d) => d,
        None => return false,
    };
    let max_d = match text_to_datum(typ, max) {
        Some(d) => d,
        None => return false,
    };
    let cmp = |a, b| datum_cmp(cmpproc, collation, a, b);
    match strategy {
        1 => cmp(min_d, k) >= 0,                     // col < K  : prune iff min >= K
        2 => cmp(min_d, k) > 0,                      // col <= K : prune iff min > K
        3 => cmp(k, min_d) < 0 || cmp(k, max_d) > 0, // col = K : prune iff K outside [min,max]
        4 => cmp(max_d, k) < 0,                      // col >= K : prune iff max < K
        5 => cmp(max_d, k) <= 0,                     // col > K  : prune iff max <= K
        _ => false,
    }
}

unsafe fn eval_in_list(
    row: &ColSummary,
    typ: pg_sys::Oid,
    collation: pg_sys::Oid,
    elems: &[Option<String>],
) -> bool {
    let (min, max) = match (&row.min, &row.max) {
        (Some(min), Some(max)) => (min, max),
        _ => return false,
    };
    let cmpproc = match cmp_proc_cached(typ) {
        Some(p) => p,
        None => return false,
    };
    let min_d = match text_to_datum(typ, min) {
        Some(d) => d,
        None => return false,
    };
    let max_d = match text_to_datum(typ, max) {
        Some(d) => d,
        None => return false,
    };
    // Prune iff no listed value falls within [min, max]. A conversion failure on any
    // element is conservative: treat it as "could match" -> KEEP.
    for elem in elems {
        let text = match elem {
            Some(t) => t,
            None => continue, // NULL element never matches a value
        };
        let d = match const_datum_cached(typ, text) {
            Some(d) => d,
            None => return false,
        };
        if datum_cmp(cmpproc, collation, d, min_d) >= 0
            && datum_cmp(cmpproc, collation, d, max_d) <= 0
        {
            return false; // this value is in range -> KEEP
        }
    }
    true
}

/// `Var && Const`: prune iff the partition's stored extent does not overlap the
/// constant. The overlap test is delegated to PostgreSQL's own `&&` operator on the
/// column's type (range types, PostGIS geometry), so it works wherever that operator
/// is defined. A missing extent or any error is conservative (KEEP).
unsafe fn eval_overlap(row: &ColSummary, const_text: &str, const_type_name: &str) -> bool {
    let extent = match &row.min {
        Some(e) => e,
        None => return false,
    };
    let type_name = &row.type_name;
    if type_name.is_empty() {
        return false;
    }
    let sql = format!(
        "SELECT NOT (CAST({ext} AS {ext_t}) && CAST({k} AS {k_t}))",
        ext = sql_literal(extent),
        ext_t = type_name,
        k = sql_literal(const_text),
        k_t = const_type_name,
    );
    Spi::get_one::<bool>(&sql).ok().flatten().unwrap_or(false)
}

/// Minimal single-quote escaping for SQL string literals built in the planner hook.
fn sql_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// ---------------------------------------------------------------------------------
// pg_sys helpers
// ---------------------------------------------------------------------------------

/// Operator name for an operator OID (e.g. "&&"), or `None`.
unsafe fn operator_name(opno: pg_sys::Oid) -> Option<String> {
    let ptr = pg_sys::get_opname(opno);
    if ptr.is_null() {
        return None;
    }
    let name = CStr::from_ptr(ptr).to_str().ok().map(|s| s.to_owned());
    pg_sys::pfree(ptr as *mut _);
    name
}

pub(crate) unsafe fn datum_cmp(
    cmpproc: pg_sys::Oid,
    collation: pg_sys::Oid,
    a: pg_sys::Datum,
    b: pg_sys::Datum,
) -> i32 {
    pg_sys::OidFunctionCall2Coll(cmpproc, collation, a, b).value() as i32
}

/// Default btree "compare" support proc for a type, or `None` if unavailable.
pub(crate) unsafe fn btree_cmp_proc(typ: pg_sys::Oid) -> Option<pg_sys::Oid> {
    let opclass = pg_sys::GetDefaultOpClass(typ, pg_sys::BTREE_AM_OID);
    if opclass == pg_sys::Oid::INVALID {
        return None;
    }
    let opfamily = pg_sys::get_opclass_family(opclass);
    if opfamily == pg_sys::Oid::INVALID {
        return None;
    }
    let proc = pg_sys::get_opfamily_proc(opfamily, typ, typ, BTORDER_PROC as i16);
    if proc == pg_sys::Oid::INVALID {
        None
    } else {
        Some(proc)
    }
}

/// Convert text to a Datum of `typ` via the type's input function.
pub(crate) unsafe fn text_to_datum(typ: pg_sys::Oid, s: &str) -> Option<pg_sys::Datum> {
    let mut infunc = pg_sys::Oid::INVALID;
    let mut typioparam = pg_sys::Oid::INVALID;
    pg_sys::getTypeInputInfo(typ, &mut infunc, &mut typioparam);
    if infunc == pg_sys::Oid::INVALID {
        return None;
    }
    let cstr = CString::new(s).ok()?;
    let datum = pg_sys::OidInputFunctionCall(infunc, cstr.as_ptr() as *mut _, typioparam, -1);
    Some(datum)
}

/// Render a Datum of `typ` to its text representation via the type's output function.
pub(crate) unsafe fn datum_to_text(typ: pg_sys::Oid, datum: pg_sys::Datum) -> Option<String> {
    let mut outfunc = pg_sys::Oid::INVALID;
    let mut is_varlena = false;
    pg_sys::getTypeOutputInfo(typ, &mut outfunc, &mut is_varlena);
    if outfunc == pg_sys::Oid::INVALID {
        return None;
    }
    let cstr = pg_sys::OidOutputFunctionCall(outfunc, datum);
    if cstr.is_null() {
        return None;
    }
    let text = CStr::from_ptr(cstr).to_str().ok().map(|s| s.to_owned());
    pg_sys::pfree(cstr as *mut _);
    text
}

/// SQL type name for a type OID (e.g. "geometry", "int4range").
unsafe fn type_name(typ: pg_sys::Oid) -> Option<String> {
    let ptr = pg_sys::format_type_be(typ);
    if ptr.is_null() {
        return None;
    }
    let name = CStr::from_ptr(ptr).to_str().ok().map(|s| s.to_owned());
    pg_sys::pfree(ptr as *mut _);
    name
}

/// Decompose a non-null array Const into per-element text values.
unsafe fn array_const_texts(con: *mut pg_sys::Const) -> Option<Vec<Option<String>>> {
    let elem_type = pg_sys::get_element_type((*con).consttype);
    if elem_type == pg_sys::Oid::INVALID {
        return None;
    }
    let mut typlen: i16 = 0;
    let mut typbyval = false;
    let mut typalign: std::ffi::c_char = 0;
    pg_sys::get_typlenbyvalalign(elem_type, &mut typlen, &mut typbyval, &mut typalign);

    let arr = pg_sys::pg_detoast_datum((*con).constvalue.cast_mut_ptr()) as *mut pg_sys::ArrayType;
    if arr.is_null() {
        return None;
    }
    let mut elems: *mut pg_sys::Datum = std::ptr::null_mut();
    let mut nulls: *mut bool = std::ptr::null_mut();
    let mut nelems: i32 = 0;
    pg_sys::deconstruct_array(
        arr,
        elem_type,
        typlen as i32,
        typbyval,
        typalign,
        &mut elems,
        &mut nulls,
        &mut nelems,
    );

    let mut out = Vec::with_capacity(nelems as usize);
    for i in 0..nelems as usize {
        if *nulls.add(i) {
            out.push(None);
        } else {
            out.push(datum_to_text(elem_type, *elems.add(i)));
        }
    }
    Some(out)
}

unsafe fn strip_relabel(mut node: *mut pg_sys::Node) -> *mut pg_sys::Node {
    while !node.is_null() && (*node).type_ == pg_sys::NodeTag::T_RelabelType {
        node = (*(node as *mut pg_sys::RelabelType)).arg as *mut pg_sys::Node;
    }
    node
}

unsafe fn is_tag(node: *mut pg_sys::Node, tag: pg_sys::NodeTag) -> bool {
    !node.is_null() && (*node).type_ == tag
}

/// Map an operator to a btree strategy number (1..5) over `lefttype`'s default family.
unsafe fn btree_strategy(opno: pg_sys::Oid, lefttype: pg_sys::Oid) -> Option<i16> {
    let opclass = pg_sys::GetDefaultOpClass(lefttype, pg_sys::BTREE_AM_OID);
    if opclass == pg_sys::Oid::INVALID {
        return None;
    }
    let opfamily = pg_sys::get_opclass_family(opclass);
    if opfamily == pg_sys::Oid::INVALID {
        return None;
    }
    let s = pg_sys::get_op_opfamily_strategy(opno, opfamily);
    if s == 0 {
        None
    } else {
        Some(s as i16)
    }
}

fn commute_strategy(strategy: i16) -> i16 {
    match strategy {
        1 => 5,
        2 => 4,
        3 => 3,
        4 => 2,
        5 => 1,
        other => other,
    }
}

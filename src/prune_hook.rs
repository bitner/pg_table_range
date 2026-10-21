use pgrx::pg_guard;
use pgrx::pg_sys;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::rc::Rc;

use crate::index_storage::ColSummary;
use crate::{TABLE_RANGE_ENABLE_PRUNING, TABLE_RANGE_LOG_PRUNING_DEBUG};

// Real planner-time partition pruning via `set_rel_pathlist_hook`, with a per-plan
// summary cache driven by `planner_hook`.
//
// For every base/partition-member relation the planner builds, we look up that
// relation's cached summaries and evaluate its restriction clauses (`Var op Const`,
// `Var IS [NOT] NULL`, `Var IN (...)`, `Var && Const`) in memory against the query's own
// constant Datums. Anything that cannot be decided exactly degrades to KEEP. If any single
// clause proves the partition cannot contain a matching row, we call `mark_dummy_rel`.
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
type SummaryMap = HashMap<u32, Rc<Vec<ColSummary>>>;

thread_local! {
    /// Summaries read during the current planner invocation. Cleared per top-level plan.
    static CACHE: RefCell<SummaryMap> = RefCell::new(HashMap::new());
    /// The table_range access-method OID, resolved once per plan.
    static AM_OID: Cell<Option<pg_sys::Oid>> = const { Cell::new(None) };
    /// Nesting depth of planner invocations (SPI during planning re-enters).
    static PLAN_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// Guards against re-entering pruning logic from catalog access during evaluation.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
    /// Per-plan cache of resolved `FmgrInfo` structs (one palloc per function), so each
    /// comparison / input-function call across partitions skips the `fmgr_info` syscache
    /// lookup. Pointers are into the planner memory context and cleared per top-level plan.
    static FMGR_MEMO: RefCell<HashMap<u32, *mut pg_sys::FmgrInfo>> = RefCell::new(HashMap::new());
    /// Per-plan memo of a type's input function + ioparam (from `getTypeInputInfo`).
    static INPUT_INFO_MEMO: RefCell<HashMap<u32, (pg_sys::Oid, pg_sys::Oid)>> =
        RefCell::new(HashMap::new());
    /// Per-plan memo keyed by (operator, column is right operand): same for every partition.
    static OP_MEMO: RefCell<HashMap<(u32, bool), Option<CompareOp>>> = RefCell::new(HashMap::new());
}

/// A planner-cached `FmgrInfo` for `proc_oid`, valid for the current plan. Avoids the
/// per-call `fmgr_info` syscache lookup that `OidFunctionCall*` does. Planner-only: the
/// cache is cleared per top-level plan (the `aminsert` path must not use this).
unsafe fn fmgr_cached(proc_oid: pg_sys::Oid) -> *mut pg_sys::FmgrInfo {
    let key: u32 = proc_oid.into();
    if let Some(p) = FMGR_MEMO.with(|m| m.borrow().get(&key).copied()) {
        return p;
    }
    let p = pg_sys::palloc0(std::mem::size_of::<pg_sys::FmgrInfo>()) as *mut pg_sys::FmgrInfo;
    pg_sys::fmgr_info(proc_oid, p);
    FMGR_MEMO.with(|m| {
        m.borrow_mut().insert(key, p);
    });
    p
}

/// Compare two datums of the same type using a plan-cached `FmgrInfo`. Planner-only.
unsafe fn cmp_cached(
    cmpproc: pg_sys::Oid,
    collation: pg_sys::Oid,
    a: pg_sys::Datum,
    b: pg_sys::Datum,
) -> i32 {
    pg_sys::FunctionCall2Coll(fmgr_cached(cmpproc), collation, a, b).value() as i32
}

/// Parse `text` to a Datum of `typ` using plan-cached type-input info and `FmgrInfo`.
/// Planner-only (clears per plan); the `aminsert` path uses [`text_to_datum`] instead.
unsafe fn parse_cached(typ: pg_sys::Oid, text: &str) -> Option<pg_sys::Datum> {
    let key: u32 = typ.into();
    let (infunc, ioparam) = match INPUT_INFO_MEMO.with(|m| m.borrow().get(&key).copied()) {
        Some(v) => v,
        None => {
            let mut infunc = pg_sys::Oid::INVALID;
            let mut ioparam = pg_sys::Oid::INVALID;
            pg_sys::getTypeInputInfo(typ, &mut infunc, &mut ioparam);
            INPUT_INFO_MEMO.with(|m| {
                m.borrow_mut().insert(key, (infunc, ioparam));
            });
            (infunc, ioparam)
        }
    };
    if infunc == pg_sys::Oid::INVALID {
        return None;
    }
    let cstr = CString::new(text).ok()?;
    Some(pg_sys::InputFunctionCall(
        fmgr_cached(infunc),
        cstr.as_ptr() as *mut _,
        ioparam,
        -1,
    ))
}

#[derive(Clone, Copy)]
struct CompareOp {
    /// Strategy (1..=5) with the column on the left.
    strategy: i16,
    /// May be polymorphic (e.g. anyenum).
    var_type: pg_sys::Oid,
    /// Compare proc for (column-side type, constant-side type).
    cmpproc: pg_sys::Oid,
}

unsafe fn compare_op_cached(opno: pg_sys::Oid, commuted: bool) -> Option<CompareOp> {
    let key = (opno.into(), commuted);
    if let Some(v) = OP_MEMO.with(|m| m.borrow().get(&key).copied()) {
        return v;
    }
    let v = compare_op(opno, commuted);
    OP_MEMO.with(|m| {
        m.borrow_mut().insert(key, v);
    });
    v
}

unsafe fn compare_op(opno: pg_sys::Oid, commuted: bool) -> Option<CompareOp> {
    let mut left = pg_sys::Oid::INVALID;
    let mut right = pg_sys::Oid::INVALID;
    pg_sys::op_input_types(opno, &mut left, &mut right);
    let (var_type, const_type) = if commuted {
        (right, left)
    } else {
        (left, right)
    };
    let opfamily = default_btree_family(var_type)?;
    let strategy = match pg_sys::get_op_opfamily_strategy(opno, opfamily) {
        s @ 1..=5 => s as i16,
        _ => return None,
    };
    // Cross-type operators (float8 > float4) compare via the family's cross-type proc.
    let cmpproc = pg_sys::get_opfamily_proc(opfamily, var_type, const_type, BTORDER_PROC as i16);
    if cmpproc == pg_sys::Oid::INVALID {
        return None;
    }
    Some(CompareOp {
        strategy: if commuted {
            commute_strategy(strategy)
        } else {
            strategy
        },
        var_type,
        cmpproc,
    })
}

/// A polymorphic operator input type (anyenum, anyarray) cannot parse text; use the column's.
unsafe fn summary_parse_type(op_var_type: pg_sys::Oid, vartype: pg_sys::Oid) -> pg_sys::Oid {
    if pg_sys::get_typtype(op_var_type) as u8 == b'p' {
        pg_sys::getBaseType(vartype)
    } else {
        op_var_type
    }
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

// `#[pg_guard]`: planner ERRORs reach Rust as panics, which must not unwind into C.
#[pg_guard]
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

    // Per-plan state points into planner memory: reset it even when planning errors.
    pgrx::PgTryBuilder::new(|| match PREV_PLANNER_HOOK {
        Some(prev) => prev(parse, query_string, cursor_options, bound_params),
        None => pg_sys::standard_planner(parse, query_string, cursor_options, bound_params),
    })
    .finally(|| {
        let last = PLAN_DEPTH.with(|d| {
            let v = d.get().saturating_sub(1);
            d.set(v);
            v == 0
        });
        if last {
            clear_cache();
        }
    })
    .execute()
}

#[pg_guard]
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
    // shortcut: an ERROR here is caught without a subtransaction; switch input parsing to
    // soft errors (InputFunctionCallSafe) and drop the catch once nothing else can raise.
    let prune = pgrx::PgTryBuilder::new(|| evaluate_relation(rel, relid))
        .catch_others(|_| false)
        .execute();
    IN_HOOK.with(|f| f.set(false));

    // shortcut: decided at plan time, so under READ COMMITTED a widening that commits
    // between planning and execution is missed by that execution; recheck at executor
    // start if that window matters.
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
    // FmgrInfo structs are palloc'd in the planner context and freed when it resets; we
    // just drop the (now-dangling-after-reset) pointers at the start/end of each plan.
    FMGR_MEMO.with(|c| c.borrow_mut().clear());
    INPUT_INFO_MEMO.with(|c| c.borrow_mut().clear());
    OP_MEMO.with(|c| c.borrow_mut().clear());
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

/// Empty shared summary returned when a relation has no usable table_range summary.
fn empty_summary() -> Rc<Vec<ColSummary>> {
    thread_local!(static EMPTY: Rc<Vec<ColSummary>> = Rc::new(Vec::new()));
    EMPTY.with(Rc::clone)
}

// We rely on the planner having put the table_range index into `rel->indexlist`. The
// planner only lists indexes with `indisvalid = true`, so a table_range index must be
// valid for pruning to engage — if anything marks it invalid (e.g. an external
// "hide indexes" DDL hook), `indexlist` omits it and we silently fall back to KEEP.
unsafe fn read_index_summary(rel: *mut pg_sys::RelOptInfo) -> Rc<Vec<ColSummary>> {
    let am = table_range_am_oid();
    if am == pg_sys::Oid::INVALID || (*rel).indexlist.is_null() {
        return empty_summary();
    }
    let indexes = pgrx::PgList::<pg_sys::IndexOptInfo>::from_pg((*rel).indexlist);
    for idx in indexes.iter_ptr() {
        if idx.is_null() || (*idx).relam != am {
            continue;
        }
        let indexoid = (*idx).indexoid;
        // Warm path: a backend-lifetime cache lets repeated plans skip the index open and
        // metapage read+deserialize entirely (kept coherent by a relcache callback).
        if let Some(cached) = crate::summary_cache::get(indexoid) {
            return cached;
        }
        let irel = pg_sys::index_open(indexoid, pg_sys::AccessShareLock as i32);
        // A partitioned (parent) index has no storage; only leaf indexes hold summaries.
        let has_storage =
            (*(*irel).rd_rel).relkind != pg_sys::RELKIND_PARTITIONED_INDEX as std::ffi::c_char;
        let summary = if has_storage {
            crate::index_storage::read_summary(irel)
        } else {
            None
        };
        pg_sys::index_close(irel, pg_sys::AccessShareLock as i32);
        let cols = Rc::new(
            summary
                .filter(|s| !s.summarizing)
                .map(|s| s.cols)
                .unwrap_or_default(),
        );
        crate::summary_cache::put(indexoid, Rc::clone(&cols));
        return cols;
    }
    empty_summary()
}

/// Returns true iff some restriction clause proves the partition cannot match.
unsafe fn evaluate_relation(rel: *mut pg_sys::RelOptInfo, relid: pg_sys::Oid) -> bool {
    crate::stable_io::with_stable_io(|| evaluate_relation_inner(rel, relid)).unwrap_or(false)
}

unsafe fn evaluate_relation_inner(rel: *mut pg_sys::RelOptInfo, relid: pg_sys::Oid) -> bool {
    let relid_u32: u32 = relid.into();
    load_summary(rel, relid_u32);

    let restrictlist = (*rel).baserestrictinfo;
    if restrictlist.is_null() {
        return false;
    }
    let restrictinfos = pgrx::PgList::<pg_sys::RestrictInfo>::from_pg(restrictlist);

    CACHE.with(|c| {
        let borrow = c.borrow();
        let rows: &[ColSummary] = match borrow.get(&relid_u32) {
            Some(rows) if !rows.is_empty() => rows.as_slice(),
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
    /// `Var op Const` with a btree operator.
    Compare {
        attnum: i16,
        strategy: i16,
        parse_type: pg_sys::Oid,
        cmpproc: pg_sys::Oid,
        collation: pg_sys::Oid,
        konst: pg_sys::Datum,
    },
    /// `Var IS [NOT] NULL`: decided from the summary's null flags.
    Null { attnum: i16, is_null: bool },
    /// `Var IN (const array)` / `= ANY`: prune iff no listed value falls in [min, max].
    InList {
        attnum: i16,
        parse_type: pg_sys::Oid,
        cmpproc: pg_sys::Oid,
        collation: pg_sys::Oid,
        elems: Vec<pg_sys::Datum>,
    },
    /// `Var && Const` (overlap): prune iff the partition's extent does not overlap the
    /// constant, evaluated with the clause's own operator. Used for range types and
    /// PostGIS geometry.
    Overlap {
        attnum: i16,
        parse_type: pg_sys::Oid,
        opfunc: pg_sys::Oid,
        collation: pg_sys::Oid,
        konst: pg_sys::Datum,
        commuted: bool,
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
            // min/max order says nothing about another collation's order.
            QualSpec::Compare {
                strategy,
                parse_type,
                cmpproc,
                collation,
                konst,
                ..
            } => {
                !row.overlap
                    && row.collation == *collation
                    && eval_compare(row, *strategy, *parse_type, *cmpproc, *collation, *konst)
            }
            QualSpec::InList {
                parse_type,
                cmpproc,
                collation,
                elems,
                ..
            } => {
                !row.overlap
                    && row.collation == *collation
                    && eval_in_list(row, *parse_type, *cmpproc, *collation, elems)
            }
            // Overlap only applies to extent summaries.
            QualSpec::Overlap {
                parse_type,
                opfunc,
                collation,
                konst,
                commuted,
                ..
            } => {
                row.overlap
                    && eval_overlap(row, *parse_type, *opfunc, *collation, *konst, *commuted)
            }
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
    let opno = (*opexpr).opno;

    // Scalar btree comparison (`<`, `<=`, `=`, `>=`, `>`).
    if let Some(op) = compare_op_cached(opno, commuted) {
        return Some(QualSpec::Compare {
            attnum,
            strategy: op.strategy,
            parse_type: summary_parse_type(op.var_type, (*var).vartype),
            cmpproc: op.cmpproc,
            collation: (*opexpr).inputcollid,
            konst: (*con).constvalue,
        });
    }

    if is_trusted_overlap_op(opno, (*var).vartype) {
        return Some(QualSpec::Overlap {
            attnum,
            parse_type: pg_sys::getBaseType((*var).vartype),
            opfunc: pg_sys::get_opcode(opno),
            collation: (*opexpr).inputcollid,
            konst: (*con).constvalue,
            commuted,
        });
    }
    None
}

/// Only an `&&` from pg_catalog or the column type's schema is trusted to mean "overlaps".
unsafe fn is_trusted_overlap_op(opno: pg_sys::Oid, vartype: pg_sys::Oid) -> bool {
    if operator_name(opno).as_deref() != Some("&&") {
        return false;
    }
    let ns = pg_sys::get_func_namespace(pg_sys::get_opcode(opno));
    ns == pg_sys::Oid::from(pg_sys::PG_CATALOG_NAMESPACE)
        || Some(ns) == type_namespace(pg_sys::getBaseType(vartype))
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
    let op = compare_op_cached((*saop).opno, false)?;
    if op.strategy != 3 {
        return None;
    }
    Some(QualSpec::InList {
        attnum,
        parse_type: summary_parse_type(op.var_type, (*var).vartype),
        cmpproc: op.cmpproc,
        collation: (*saop).inputcollid,
        elems: array_const_datums(con)?,
    })
}

// ---------------------------------------------------------------------------------
// Typed in-memory evaluation
// ---------------------------------------------------------------------------------

unsafe fn parsed_bounds(
    row: &ColSummary,
    parse_type: pg_sys::Oid,
) -> Option<(pg_sys::Datum, pg_sys::Datum)> {
    match (&row.min, &row.max) {
        (Some(min), Some(max)) if !row.unbounded => Some((
            parse_cached(parse_type, min)?,
            parse_cached(parse_type, max)?,
        )),
        _ => None, // no usable range -> conservative KEEP
    }
}

unsafe fn eval_compare(
    row: &ColSummary,
    strategy: i16,
    parse_type: pg_sys::Oid,
    cmpproc: pg_sys::Oid,
    collation: pg_sys::Oid,
    k: pg_sys::Datum,
) -> bool {
    let Some((min_d, max_d)) = parsed_bounds(row, parse_type) else {
        return false;
    };
    let cmp = |bound| cmp_cached(cmpproc, collation, bound, k);
    match strategy {
        1 => cmp(min_d) >= 0,                  // col < K  : prune iff min >= K
        2 => cmp(min_d) > 0,                   // col <= K : prune iff min > K
        3 => cmp(min_d) > 0 || cmp(max_d) < 0, // col = K  : prune iff K outside [min,max]
        4 => cmp(max_d) < 0,                   // col >= K : prune iff max < K
        5 => cmp(max_d) <= 0,                  // col > K  : prune iff max <= K
        _ => false,
    }
}

unsafe fn eval_in_list(
    row: &ColSummary,
    parse_type: pg_sys::Oid,
    cmpproc: pg_sys::Oid,
    collation: pg_sys::Oid,
    elems: &[pg_sys::Datum],
) -> bool {
    let Some((min_d, max_d)) = parsed_bounds(row, parse_type) else {
        return false;
    };
    // Prune iff no listed value falls within [min, max].
    !elems.iter().any(|&d| {
        cmp_cached(cmpproc, collation, min_d, d) <= 0
            && cmp_cached(cmpproc, collation, max_d, d) >= 0
    })
}

/// `Var && Const`: prune iff the partition's stored extent does not overlap the
/// constant. Every row's value lies within the extent, so if the extent does not overlap
/// the constant, no row can.
unsafe fn eval_overlap(
    row: &ColSummary,
    parse_type: pg_sys::Oid,
    opfunc: pg_sys::Oid,
    collation: pg_sys::Oid,
    k: pg_sys::Datum,
    commuted: bool,
) -> bool {
    let extent = match &row.min {
        Some(e) if !row.unbounded => e,
        _ => return false,
    };
    let Some(ext) = parse_cached(parse_type, extent) else {
        return false;
    };
    let (a, b) = if commuted { (k, ext) } else { (ext, k) };
    let overlaps = pg_sys::FunctionCall2Coll(fmgr_cached(opfunc), collation, a, b);
    overlaps.value() & 0xff == 0
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

/// Looked up under the opclass's input type, which may differ from `typ` (varchar -> text).
pub(crate) unsafe fn btree_cmp_proc(typ: pg_sys::Oid) -> Option<pg_sys::Oid> {
    let opclass = pg_sys::GetDefaultOpClass(typ, pg_sys::BTREE_AM_OID);
    if opclass == pg_sys::Oid::INVALID {
        return None;
    }
    let opfamily = pg_sys::get_opclass_family(opclass);
    let intype = pg_sys::get_opclass_input_type(opclass);
    let proc = pg_sys::get_opfamily_proc(opfamily, intype, intype, BTORDER_PROC as i16);
    (proc != pg_sys::Oid::INVALID).then_some(proc)
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

unsafe fn type_namespace(typ: pg_sys::Oid) -> Option<pg_sys::Oid> {
    let tup = pg_sys::SearchSysCache1(
        pg_sys::SysCacheIdentifier::TYPEOID as i32,
        pg_sys::Datum::from(typ),
    );
    if tup.is_null() {
        return None;
    }
    let ns = (*pg_sys::heap_tuple_get_struct::<pg_sys::FormData_pg_type>(tup)).typnamespace;
    pg_sys::ReleaseSysCache(tup);
    Some(ns)
}

/// NULL elements are dropped: they never match.
unsafe fn array_const_datums(con: *mut pg_sys::Const) -> Option<Vec<pg_sys::Datum>> {
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

    Some(
        (0..nelems as usize)
            .filter(|&i| !*nulls.add(i))
            .map(|i| *elems.add(i))
            .collect(),
    )
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

unsafe fn default_btree_family(typ: pg_sys::Oid) -> Option<pg_sys::Oid> {
    let opclass = pg_sys::GetDefaultOpClass(typ, pg_sys::BTREE_AM_OID);
    if opclass == pg_sys::Oid::INVALID {
        return None;
    }
    let opfamily = pg_sys::get_opclass_family(opclass);
    (opfamily != pg_sys::Oid::INVALID).then_some(opfamily)
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

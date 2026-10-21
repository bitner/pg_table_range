use pgrx::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::rc::Rc;

use crate::index_storage::ColSummary;

// Pre-expansion pruning: before standard_planner, evaluate the query's top-level WHERE
// against every leaf's summary and AND in a partition-key qual covering only the leaves
// that can match, so native pruning drops the rest before they are locked or opened.
// The injected qual is implied by the query (every matching row is in a surviving leaf).
//
// shortcut: single-level RANGE on one plain key column, no default partition, top-level
// query only; extend to LIST, sub-partitioning and subqueries if the spike pays off.

/// What pruning needs about one partitioned table, without opening its leaves.
struct Directory {
    key_attno: i16,
    key_type: pg_sys::Oid,
    key_typmod: i32,
    key_collation: pg_sys::Oid,
    ge_op: pg_sys::Oid,
    lt_op: pg_sys::Oid,
    /// In bound order.
    leaves: Vec<Leaf>,
}

struct Leaf {
    /// table_range index on the leaf; `None` (or a different column layout) never prunes.
    index: Option<pg_sys::Oid>,
    /// Bounds as text in the key type; `None` is MINVALUE / MAXVALUE.
    lower: Option<String>,
    upper: Option<String>,
}

thread_local! {
    /// parent oid -> directory; dropped on any relcache invalidation of the parent.
    static DIRECTORIES: RefCell<HashMap<u32, Option<Rc<Directory>>>> = RefCell::new(HashMap::new());
}

pub(crate) fn invalidate(relid: pg_sys::Oid) {
    DIRECTORIES.with(|d| {
        if relid == pg_sys::Oid::INVALID {
            d.borrow_mut().clear();
        } else {
            d.borrow_mut().remove(&u32::from(relid));
        }
    });
}

/// AND a surviving-partitions qual into `parse` for each partitioned table it can help.
pub(crate) unsafe fn inject_partition_quals(parse: *mut pg_sys::Query) {
    let cmd = (*parse).commandType;
    if !matches!(
        cmd,
        pg_sys::CmdType::CMD_SELECT | pg_sys::CmdType::CMD_UPDATE | pg_sys::CmdType::CMD_DELETE
    ) || (*parse).jointree.is_null()
        || (*(*parse).jointree).quals.is_null()
    {
        return;
    }
    let conjuncts = top_level_conjuncts((*(*parse).jointree).quals);
    let rtable = pgrx::PgList::<pg_sys::RangeTblEntry>::from_pg((*parse).rtable);
    for (i, rte) in rtable.iter_ptr().enumerate() {
        let rti = (i + 1) as u32;
        if (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION
            || !(*rte).inh
            || (*rte).relkind != pg_sys::RELKIND_PARTITIONED_TABLE as std::ffi::c_char
        {
            continue;
        }
        let quals: Vec<*mut pg_sys::Node> = conjuncts
            .iter()
            .copied()
            .filter(|&q| only_references(q, rti))
            .collect();
        if quals.is_empty() {
            continue;
        }
        let Some(dir) = directory((*rte).relid) else {
            continue;
        };
        if let Some(qual) = surviving_qual(&dir, rti, &quals) {
            let jt = (*parse).jointree;
            (*jt).quals = pg_sys::make_and_qual((*jt).quals, qual);
        }
    }
}

/// Const-folded copies of the WHERE clause's top-level AND items.
unsafe fn top_level_conjuncts(quals: *mut pg_sys::Node) -> Vec<*mut pg_sys::Node> {
    let folded = pg_sys::eval_const_expressions(
        std::ptr::null_mut(),
        pg_sys::copyObjectImpl(quals.cast()).cast(),
    );
    pgrx::PgList::<pg_sys::Node>::from_pg(pg_sys::make_ands_implicit(folded.cast()))
        .iter_ptr()
        .collect()
}

/// Every Var in `node` is a column of range-table entry `rti` at this query level.
unsafe fn only_references(node: *mut pg_sys::Node, rti: u32) -> bool {
    let vars = pgrx::PgList::<pg_sys::Node>::from_pg(pg_sys::pull_var_clause(
        node,
        (pg_sys::PVC_RECURSE_AGGREGATES
            | pg_sys::PVC_RECURSE_WINDOWFUNCS
            | pg_sys::PVC_RECURSE_PLACEHOLDERS) as i32,
    ));
    !vars.is_empty()
        && vars.iter_ptr().all(|v| {
            (*v).type_ == pg_sys::NodeTag::T_Var && {
                let var = v as *mut pg_sys::Var;
                (*var).varno as u32 == rti && (*var).varlevelsup == 0
            }
        })
}

/// `None` when every leaf survives (nothing to add).
unsafe fn surviving_qual(
    dir: &Directory,
    rti: u32,
    quals: &[*mut pg_sys::Node],
) -> Option<*mut pg_sys::Node> {
    // Merge adjacent surviving leaves into (lower, upper) intervals.
    let mut intervals: Vec<(Option<&str>, Option<&str>)> = Vec::new();
    let mut all_survive = true;
    let mut extends = false;
    for leaf in &dir.leaves {
        let survives = match leaf.index.and_then(|i| leaf_summary(i)) {
            Some(rows) => !quals
                .iter()
                .any(|&q| crate::prune_hook::clause_proves_prune(q, rows.as_slice(), 0)),
            None => true,
        };
        all_survive &= survives;
        if !survives {
            extends = false;
            continue;
        }
        match intervals.last_mut() {
            Some(last) if extends && last.1 == leaf.lower.as_deref() => {
                last.1 = leaf.upper.as_deref()
            }
            _ => intervals.push((leaf.lower.as_deref(), leaf.upper.as_deref())),
        }
        extends = true;
    }
    if all_survive {
        return None;
    }
    let var = || {
        pg_sys::makeVar(
            rti as i32,
            dir.key_attno,
            dir.key_type,
            dir.key_typmod,
            dir.key_collation,
            0,
        ) as *mut pg_sys::Node
    };
    let mut disjuncts = Vec::new();
    for (lower, upper) in intervals {
        let mut sides = Vec::new();
        if let Some(lo) = lower {
            sides.push(op_clause(dir, dir.ge_op, var(), lo)?);
        }
        if let Some(hi) = upper {
            sides.push(op_clause(dir, dir.lt_op, var(), hi)?);
        }
        disjuncts.push(match sides.len() {
            0 => return None, // an unbounded interval: everything survives
            1 => sides[0],
            _ => pg_sys::make_andclause(list(&sides)) as *mut pg_sys::Node,
        });
    }
    Some(match disjuncts.len() {
        0 => pg_sys::makeBoolConst(false, false),
        1 => disjuncts[0],
        _ => pg_sys::make_orclause(list(&disjuncts)) as *mut pg_sys::Node,
    })
}

unsafe fn op_clause(
    dir: &Directory,
    opno: pg_sys::Oid,
    var: *mut pg_sys::Node,
    bound: &str,
) -> Option<*mut pg_sys::Node> {
    let datum = crate::prune_hook::text_to_datum(dir.key_type, bound)?;
    let mut typlen: i16 = 0;
    let mut typbyval = false;
    pg_sys::get_typlenbyval(dir.key_type, &mut typlen, &mut typbyval);
    let konst = pg_sys::makeConst(
        dir.key_type,
        dir.key_typmod,
        dir.key_collation,
        typlen as i32,
        datum,
        false,
        typbyval,
    );
    let expr = pg_sys::make_opclause(
        opno,
        pg_sys::BOOLOID,
        false,
        var.cast(),
        konst.cast(),
        pg_sys::Oid::INVALID,
        dir.key_collation,
    );
    (*(expr as *mut pg_sys::OpExpr)).opfuncid = pg_sys::get_opcode(opno);
    Some(expr as *mut pg_sys::Node)
}

unsafe fn list(nodes: &[*mut pg_sys::Node]) -> *mut pg_sys::List {
    let mut l = pgrx::PgList::<pg_sys::Node>::new();
    for &n in nodes {
        l.push(n);
    }
    l.into_pg()
}

unsafe fn leaf_summary(index: pg_sys::Oid) -> Option<Rc<Vec<ColSummary>>> {
    if let Some(cached) = crate::summary_cache::get(index) {
        return (!cached.is_empty()).then_some(cached);
    }
    let irel = pg_sys::index_open(index, pg_sys::AccessShareLock as i32);
    let cols = Rc::new(
        crate::index_storage::read_summary(irel)
            .map(|s| s.cols)
            .unwrap_or_default(),
    );
    pg_sys::index_close(irel, pg_sys::AccessShareLock as i32);
    crate::summary_cache::put(index, Rc::clone(&cols));
    (!cols.is_empty()).then_some(cols)
}

unsafe fn directory(parent: pg_sys::Oid) -> Option<Rc<Directory>> {
    let key = u32::from(parent);
    if let Some(d) = DIRECTORIES.with(|d| d.borrow().get(&key).cloned()) {
        return d;
    }
    let built = build_directory(parent).map(Rc::new);
    DIRECTORIES.with(|d| d.borrow_mut().insert(key, built.clone()));
    built
}

/// Catalog reads only: no leaf is locked or opened.
unsafe fn build_directory(parent: pg_sys::Oid) -> Option<Directory> {
    let p = u32::from(parent);
    pg_sys::SPI_connect();
    let key = spi_rows(&format!(
        "SELECT pt.partattrs[0], a.atttypid, a.atttypmod, pt.partcollation[0], oc.opcfamily
         FROM pg_catalog.pg_partitioned_table pt
         JOIN pg_catalog.pg_attribute a ON a.attrelid = pt.partrelid AND a.attnum = pt.partattrs[0]
         JOIN pg_catalog.pg_opclass oc ON oc.oid = pt.partclass[0]
         WHERE pt.partrelid = {p} AND pt.partstrat = 'r' AND pt.partnatts = 1
           AND pt.partattrs[0] <> 0 AND pt.partdefid = 0"
    ));
    let leaf_indexes = spi_rows(&format!(
        "SELECT i.inhrelid,
                (SELECT x.indexrelid FROM pg_catalog.pg_index x
                 JOIN pg_catalog.pg_class c ON c.oid = x.indexrelid
                 JOIN pg_catalog.pg_am am ON am.oid = c.relam
                 WHERE x.indrelid = i.inhrelid AND x.indisvalid AND am.amname = 'table_range'
                 LIMIT 1),
                NOT EXISTS (SELECT 1 FROM pg_catalog.pg_attribute pa
                            WHERE pa.attrelid = {p} AND pa.attnum > 0 AND NOT pa.attisdropped
                              AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_attribute la
                                              WHERE la.attrelid = i.inhrelid
                                                AND la.attnum = pa.attnum
                                                AND la.attname = pa.attname
                                                AND NOT la.attisdropped))
         FROM pg_catalog.pg_inherits i WHERE i.inhparent = {p}"
    ));
    pg_sys::SPI_finish();

    let key = key.first()?;
    let num = |i: usize| key.get(i).cloned().flatten();
    let key_attno: i16 = num(0)?.parse().ok()?;
    let key_type = pg_sys::Oid::from(num(1)?.parse::<u32>().ok()?);
    let key_typmod: i32 = num(2)?.parse().ok()?;
    let key_collation = pg_sys::Oid::from(num(3)?.parse::<u32>().ok()?);
    let opfamily = pg_sys::Oid::from(num(4)?.parse::<u32>().ok()?);
    let ge_op = pg_sys::get_opfamily_member(
        opfamily,
        key_type,
        key_type,
        pg_sys::BTGreaterEqualStrategyNumber as i16,
    );
    let lt_op = pg_sys::get_opfamily_member(
        opfamily,
        key_type,
        key_type,
        pg_sys::BTLessStrategyNumber as i16,
    );
    if ge_op == pg_sys::Oid::INVALID || lt_op == pg_sys::Oid::INVALID {
        return None;
    }
    let index_of: HashMap<u32, Option<pg_sys::Oid>> = leaf_indexes
        .iter()
        .filter_map(|r| {
            let leaf: u32 = r.first()?.as_ref()?.parse().ok()?;
            let same_layout = r.get(2)?.as_deref() == Some("t");
            let index = r
                .get(1)?
                .as_ref()
                .and_then(|t| t.parse::<u32>().ok())
                .filter(|_| same_layout)
                .map(pg_sys::Oid::from);
            Some((leaf, index))
        })
        .collect();

    // The parent is locked by the parser; its descriptor lists the leaves and bounds.
    let rel = pg_sys::relation_open(parent, pg_sys::NoLock as i32);
    let desc = pg_sys::RelationGetPartitionDesc(rel, true);
    let bounds = (*desc).boundinfo;
    let mut leaves = Vec::new();
    let mut ok = !bounds.is_null();
    if ok {
        for i in 1..(*bounds).ndatums as usize {
            let part = *(*bounds).indexes.add(i);
            if part < 0 {
                continue; // a gap between partitions
            }
            if !*(*desc).is_leaf.add(part as usize) {
                ok = false; // sub-partitioned
                break;
            }
            let bound = |j: usize| -> Option<String> {
                let kind = **(*bounds).kind.add(j);
                if kind != pg_sys::PartitionRangeDatumKind::PARTITION_RANGE_DATUM_VALUE {
                    return None;
                }
                crate::prune_hook::datum_to_text(key_type, **(*bounds).datums.add(j))
            };
            let leaf_oid = u32::from(*(*desc).oids.add(part as usize));
            leaves.push(Leaf {
                index: index_of.get(&leaf_oid).copied().flatten(),
                lower: bound(i - 1),
                upper: bound(i),
            });
        }
    }
    pg_sys::relation_close(rel, pg_sys::NoLock as i32);
    ok.then_some(Directory {
        key_attno,
        key_type,
        key_typmod,
        key_collation,
        ge_op,
        lt_op,
        leaves,
    })
}

/// All rows of a read-only statement on the current SPI connection, as text.
unsafe fn spi_rows(sql: &str) -> Vec<Vec<Option<String>>> {
    let cmd = CString::new(sql).expect("SQL contains no NUL bytes");
    if pg_sys::SPI_execute(cmd.as_ptr(), true, 0) != pg_sys::SPI_OK_SELECT as i32 {
        error!("table_range: SPI_execute failed: {sql}");
    }
    let table = pg_sys::SPI_tuptable;
    let tupdesc = (*table).tupdesc;
    (0..pg_sys::SPI_processed as usize)
        .map(|r| {
            let tuple = *(*table).vals.add(r);
            (1..=(*tupdesc).natts)
                .map(|c| {
                    let p = pg_sys::SPI_getvalue(tuple, tupdesc, c);
                    (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
                })
                .collect()
        })
        .collect()
}

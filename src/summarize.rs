use pgrx::prelude::*;

use crate::index_am::{index_columns, widen_column};
use crate::index_storage::{self, ColSummary, IndexSummary};

// Inserts continue during a rebuild: the page is flagged `summarizing` (never pruned) with
// empty bounds that concurrent inserts widen, then merged with the scan's result.
//
// shortcut: a rebuild that errors or is cancelled leaves the page summarizing (never
// pruned) until the next VACUUM or table_range_summarize; clear it on abort if that bites.

/// Returns whether the summary was rebuilt. `only_unbounded` skips readable summaries whose
/// bounds are all known, so a routine VACUUM does not rescan the partition.
pub(crate) unsafe fn resummarize(
    index: pg_sys::Relation,
    heap: pg_sys::Relation,
    only_unbounded: bool,
) -> bool {
    let index_info = pg_sys::BuildIndexInfo(index);
    if !start_collecting(index, index_info, only_unbounded) {
        return false;
    }
    let built = crate::summary_build::build_leaf_summary(heap, index, index_info, true);

    // Extending geometry extents runs SQL, which needs a snapshot (VACUUM has none).
    pg_sys::PushActiveSnapshot(pg_sys::GetTransactionSnapshot());
    let done = loop {
        pgrx::check_for_interrupts!();
        let Some(before) = index_storage::read_blob(index) else {
            break false;
        };
        let Some(collected) = index_storage::deserialize(&before) else {
            break false;
        };
        if !collected.summarizing {
            break false;
        }
        let mut merged = built.summary.clone();
        crate::stable_io::with_stable_io(|| fold_in(&mut merged, &collected, index, index_info))
            .unwrap_or_else(|| error!("table_range: cannot re-summarize in parallel mode"));
        let after = index_storage::encode_fitting(&mut merged);
        if index_storage::replace_blob_if(index, &before, &after).unwrap_or_else(|e| error!("{e}"))
        {
            break true;
        }
    };
    pg_sys::PopActiveSnapshot();
    if done {
        // The summary narrowed: let caches and cached plans pick up the tighter bounds.
        crate::summary_cache::note_changed((*index).rd_id, (*heap).rd_id);
    }
    done
}

/// Swap in empty bounds and the `summarizing` flag; `false` if there is nothing to do.
unsafe fn start_collecting(
    index: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
    only_unbounded: bool,
) -> bool {
    loop {
        pgrx::check_for_interrupts!();
        let Some(before) = index_storage::read_blob(index) else {
            return false;
        };
        let summary = match index_storage::deserialize(&before) {
            Some(s) if only_unbounded && !s.summarizing && !s.cols.iter().any(|c| c.unbounded) => {
                return false
            }
            Some(s) => s,
            // Unreadable (e.g. an older on-page format, never pruned with): rebuild it.
            None => crate::summary_build::empty_summary(index, index_info),
        };
        let collecting = IndexSummary {
            cols: summary
                .cols
                .into_iter()
                .map(|c| ColSummary {
                    min: None,
                    max: None,
                    has_nulls: false,
                    all_nulls: false,
                    unbounded: false,
                    ..c
                })
                .collect(),
            summarizing: true,
        };
        let after = index_storage::serialize(&collecting);
        if index_storage::replace_blob_if(index, &before, &after).unwrap_or_else(|e| error!("{e}"))
        {
            return true;
        }
    }
}

/// Widen the scanned summary with the values inserts collected during the scan.
unsafe fn fold_in(
    merged: &mut IndexSummary,
    collected: &IndexSummary,
    index: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
) {
    for c in index_columns(index, index_info) {
        let (Some(m), Some(p)) = (
            merged.cols.iter_mut().find(|s| s.attnum == c.attnum),
            collected.cols.iter().find(|s| s.attnum == c.attnum),
        ) else {
            continue;
        };
        m.has_nulls |= p.has_nulls;
        if p.unbounded {
            m.make_unbounded();
            m.all_nulls = false;
            continue;
        }
        for bound in [&p.min, &p.max].into_iter().flatten() {
            match crate::prune_hook::text_to_datum(pg_sys::getBaseType(c.typ), bound) {
                Some(d) => {
                    widen_column(m, c.typ, c.collation, d, false);
                }
                // Still a non-null value: unknown bounds, not an all-null column.
                None => {
                    m.make_unbounded();
                    m.all_nulls = false;
                }
            }
        }
    }
}

/// Rebuild every summary of a table_range index (each leaf of a partitioned index) without
/// blocking inserts, e.g. to re-tighten bounds after deletes. Returns how many were rebuilt.
#[pg_extern]
fn table_range_summarize(index: pg_sys::Oid) -> i32 {
    unsafe {
        let am = pg_sys::get_am_oid(c"table_range".as_ptr(), false);
        let name = |oid| {
            let p = pg_sys::get_rel_name(oid);
            if p.is_null() {
                format!("{}", u32::from(oid))
            } else {
                std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        let mut rebuilt = 0;
        for leaf in leaf_indexes(index) {
            if pg_sys::get_rel_relkind(leaf) as u8 != pg_sys::RELKIND_INDEX {
                error!("table_range: \"{}\" is not an index", name(leaf));
            }
            let heap_oid = pg_sys::IndexGetRelation(leaf, false);
            if !pg_sys::object_ownercheck(pg_sys::RelationRelationId, heap_oid, pg_sys::GetUserId())
            {
                error!("table_range: must be owner of table \"{}\"", name(heap_oid));
            }
            let heap = pg_sys::table_open(heap_oid, pg_sys::ShareUpdateExclusiveLock as i32);
            let irel = pg_sys::index_open(leaf, pg_sys::RowExclusiveLock as i32);
            if (*(*irel).rd_rel).relam != am {
                error!("table_range: \"{}\" is not a table_range index", name(leaf));
            }
            // Run index expressions as the table owner, as VACUUM and REINDEX do.
            let mut user = pg_sys::Oid::INVALID;
            let mut sec = 0;
            pg_sys::GetUserIdAndSecContext(&mut user, &mut sec);
            pg_sys::SetUserIdAndSecContext(
                (*(*heap).rd_rel).relowner,
                sec | pg_sys::SECURITY_RESTRICTED_OPERATION as i32,
            );
            let guc_level = pg_sys::NewGUCNestLevel();
            #[cfg(not(feature = "pg16"))]
            pg_sys::RestrictSearchPath();
            if resummarize(irel, heap, false) {
                rebuilt += 1;
            }
            pg_sys::AtEOXact_GUC(false, guc_level);
            pg_sys::SetUserIdAndSecContext(user, sec);
            // Release per leaf, so many partitions do not exhaust the lock table.
            pg_sys::index_close(irel, pg_sys::RowExclusiveLock as i32);
            pg_sys::table_close(heap, pg_sys::ShareUpdateExclusiveLock as i32);
        }
        rebuilt
    }
}

/// The index itself, or the leaves of a partitioned index.
unsafe fn leaf_indexes(index: pg_sys::Oid) -> Vec<pg_sys::Oid> {
    if pg_sys::get_rel_relkind(index) as u8 != pg_sys::RELKIND_PARTITIONED_INDEX {
        return vec![index];
    }
    pg_sys::SPI_connect();
    let leaves = crate::summary_build::spi_column(&format!(
        "SELECT relid::pg_catalog.oid FROM pg_catalog.pg_partition_tree({}::pg_catalog.oid::pg_catalog.regclass) WHERE isleaf",
        u32::from(index)
    ));
    pg_sys::SPI_finish();
    leaves
        .into_iter()
        .flatten()
        .filter_map(|t| t.parse::<u32>().ok().map(pg_sys::Oid::from))
        .collect()
}

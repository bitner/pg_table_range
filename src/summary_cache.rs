// Backend-lifetime cache of deserialized per-index summaries, so warm/repeated planning
// does not re-open each partition's index and re-read+deserialize its metapage on every
// plan. Keyed by index OID; values are shared (`Rc`) with the per-plan cache.
//
// Correctness rests on the over-inclusive invariant: a cached summary is safe as long as
// it is never *narrower* than the real data. Only `aminsert` widens a summary, and it
// calls `note_widened`. Narrowing (deletes) needs no invalidation: an over-wide cached
// summary still prunes correctly.

use crate::index_storage::ColSummary;
use pgrx::pg_sys;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

thread_local! {
    /// indexoid -> its deserialized summary (empty vec = "no summary / not a leaf").
    static CACHE: RefCell<HashMap<u32, Rc<Vec<ColSummary>>>> = RefCell::new(HashMap::new());
}

/// Look up a cached summary for `indexoid`, if present.
pub fn get(indexoid: pg_sys::Oid) -> Option<Rc<Vec<ColSummary>>> {
    let key: u32 = indexoid.into();
    CACHE.with(|c| c.borrow().get(&key).cloned())
}

/// Store a deserialized summary for `indexoid`.
pub fn put(indexoid: pg_sys::Oid, summary: Rc<Vec<ColSummary>>) {
    let key: u32 = indexoid.into();
    CACHE.with(|c| {
        c.borrow_mut().insert(key, summary);
    });
}

/// The heap invalidation is what replans cached plans: they depend on tables, not indexes.
/// Not deduplicated: PostgreSQL coalesces per command, and coarser dedup loses invalidations.
pub unsafe fn note_widened(indexoid: pg_sys::Oid, heapoid: pg_sys::Oid) {
    pg_sys::CacheInvalidateRelcacheByRelid(indexoid);
    pg_sys::CacheInvalidateRelcacheByRelid(heapoid);
    // Pre-expansion pruning leaves pruned leaves out of a plan, so its plans depend only
    // on the ancestors.
    if (*heap_rel(heapoid)).relispartition {
        pg_sys::SPI_connect();
        for ancestor in crate::summary_build::spi_column(&format!(
            "SELECT relid::pg_catalog.oid FROM pg_catalog.pg_partition_ancestors({}) WHERE relid <> {0}",
            u32::from(heapoid)
        )) {
            if let Some(oid) = ancestor.and_then(|t| t.parse::<u32>().ok()) {
                pg_sys::CacheInvalidateRelcacheByRelid(pg_sys::Oid::from(oid));
            }
        }
        pg_sys::SPI_finish();
    }
}

unsafe fn heap_rel(relid: pg_sys::Oid) -> *mut pg_sys::FormData_pg_class {
    let rel = pg_sys::RelationIdGetRelation(relid);
    let form = (*rel).rd_rel;
    pg_sys::RelationClose(rel);
    form
}

/// Relcache invalidation callback: drop the cached summary for `relid` (or all of them
/// when `relid` is invalid, PG's "flush everything" signal).
#[pgrx::pg_guard]
unsafe extern "C-unwind" fn relcache_callback(_arg: pg_sys::Datum, relid: pg_sys::Oid) {
    crate::pre_expansion::invalidate(relid);
    if relid == pg_sys::Oid::INVALID {
        CACHE.with(|c| c.borrow_mut().clear());
    } else {
        let key: u32 = relid.into();
        CACHE.with(|c| {
            c.borrow_mut().remove(&key);
        });
    }
}

/// Register the relcache invalidation callback. Called once from `_PG_init`.
pub fn register() {
    unsafe {
        pg_sys::CacheRegisterRelcacheCallback(Some(relcache_callback), pg_sys::Datum::from(0));
    }
}

// Backend-lifetime cache of deserialized per-index summaries, so warm/repeated planning
// does not re-open each partition's index and re-read+deserialize its metapage on every
// plan. Keyed by index OID; values are shared (`Rc`) with the per-plan cache.
//
// Correctness rests on the over-inclusive invariant: a cached summary is safe as long as
// it is never *narrower* than the real data. Only `aminsert` widens a summary, and it
// calls `note_widened`, which sends a relcache invalidation for the index. That clears the
// cache in every backend (via `relcache_callback`) — locally at the next command boundary,
// and in other backends when the widening transaction commits, matching row visibility.
// Operations that only *narrow* a summary (deletes, vacuum re-tighten) need no
// invalidation, because an over-wide cached summary still prunes correctly.

use crate::index_storage::ColSummary;
use pgrx::pg_sys;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

thread_local! {
    /// indexoid -> its deserialized summary (empty vec = "no summary / not a leaf").
    static CACHE: RefCell<HashMap<u32, Rc<Vec<ColSummary>>>> = RefCell::new(HashMap::new());
    /// Indexes already invalidated in the current transaction, to coalesce a bulk insert's
    /// repeated widenings into one invalidation per index per transaction.
    static INVALIDATED: RefCell<HashSet<u32>> = RefCell::new(HashSet::new());
    /// TransactionId the `INVALIDATED` set belongs to; on change we clear the set.
    static INVALIDATED_XID: Cell<u32> = const { Cell::new(0) };
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

/// Called by `aminsert` when it widens an index's on-page summary. Invalidates the cached
/// copy everywhere (once per index per transaction).
pub unsafe fn note_widened(indexoid: pg_sys::Oid) {
    let key: u32 = indexoid.into();
    let xid: u32 = pg_sys::GetTopTransactionIdIfAny().into();
    if xid == 0 {
        // No assigned xid to scope dedup to; invalidate unconditionally (safe).
        pg_sys::CacheInvalidateRelcacheByRelid(indexoid);
        return;
    }
    if INVALIDATED_XID.with(|x| x.get()) != xid {
        INVALIDATED_XID.with(|x| x.set(xid));
        INVALIDATED.with(|s| s.borrow_mut().clear());
    }
    let first = INVALIDATED.with(|s| s.borrow_mut().insert(key));
    if first {
        pg_sys::CacheInvalidateRelcacheByRelid(indexoid);
    }
}

/// Relcache invalidation callback: drop the cached summary for `relid` (or all of them
/// when `relid` is invalid, PG's "flush everything" signal).
unsafe extern "C-unwind" fn relcache_callback(_arg: pg_sys::Datum, relid: pg_sys::Oid) {
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

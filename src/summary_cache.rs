// Backend-lifetime cache of deserialized per-index summaries, so warm/repeated planning
// does not re-open each partition's index and re-read+deserialize its metapage on every
// plan. Keyed by index OID; values are shared (`Rc`) with the per-plan cache.
//
// Correctness rests on the over-inclusive invariant: a cached summary is safe as long as
// it is never *narrower* than the real data. Every page write calls `note_changed`.

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

/// Invalidates cached copies of the summary and cached plans on the heap (plans depend on
/// tables, not indexes) in every backend.
pub unsafe fn note_changed(indexoid: pg_sys::Oid, heapoid: pg_sys::Oid) {
    // Transactional: reaches this backend at its next command.
    pg_sys::CacheInvalidateRelcacheByRelid(indexoid);
    pg_sys::CacheInvalidateRelcacheByRelid(heapoid);
    // Immediate: the page write is not transactional, so other backends must hear of it
    // even if this transaction aborts.
    let message = |relid: pg_sys::Oid| pg_sys::SharedInvalidationMessage {
        rc: pg_sys::SharedInvalRelcacheMsg {
            id: pg_sys::SHAREDINVALRELCACHE_ID as i8,
            dbId: pg_sys::MyDatabaseId,
            relId: relid,
        },
    };
    let mut messages = [message(indexoid), message(heapoid)];
    pg_sys::SendSharedInvalidMessages(messages.as_ptr(), messages.len() as i32);
    // Hot standbys only see invalidations that are in the WAL.
    if pg_sys::wal_level >= pg_sys::WalLevel::WAL_LEVEL_REPLICA as i32 {
        pg_sys::LogStandbyInvalidations(messages.len() as i32, messages.as_mut_ptr(), false);
    }
}

/// Relcache invalidation callback: drop the cached summary for `relid` (or all of them
/// when `relid` is invalid, PG's "flush everything" signal).
#[pgrx::pg_guard]
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

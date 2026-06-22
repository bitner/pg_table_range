use pgrx::pg_sys;
use pgrx::prelude::*;

// Low-level storage for a per-index summary, kept in the index's own metapage (block 0)
// and updated in place with the Generic WAL API — the same model BRIN uses. Because a
// table_range summary only ever needs to be *over-inclusive*, these in-place page
// updates need no MVCC/transactionality: a rolled-back or concurrent widening that
// over-covers is always safe.
//
// This module currently provides the raw page round-trip (length-prefixed byte blob in
// the metapage). Typed summary (de)serialization and the ambuild/aminsert/planner wiring
// build on top of it.

// Usable bytes in the metapage content area (after the page header + length prefix). A
// generous margin below BLCKSZ keeps us clear of the page header regardless of alignment.
fn max_blob_len() -> usize {
    pg_sys::BLCKSZ as usize - 64
}

/// Write a byte blob into the index's metapage (block 0), creating the block if needed.
/// WAL-logged via Generic WAL. Caller must hold a lock on the index relation appropriate
/// for the calling context (ambuild owns it; aminsert holds the row's locks).
pub unsafe fn write_blob(index: pg_sys::Relation, data: &[u8]) -> Result<(), &'static str> {
    if data.len() > max_blob_len() {
        return Err("table_range: summary blob exceeds one page");
    }
    let nblocks = pg_sys::RelationGetNumberOfBlocksInFork(index, pg_sys::ForkNumber::MAIN_FORKNUM);
    let is_new = nblocks == 0;
    let buffer = if is_new {
        pg_sys::ReadBuffer(index, pg_sys::InvalidBlockNumber) // P_NEW -> extend
    } else {
        pg_sys::ReadBuffer(index, 0)
    };
    pg_sys::LockBuffer(buffer, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);

    let state = pg_sys::GenericXLogStart(index);
    let page =
        pg_sys::GenericXLogRegisterBuffer(state, buffer, pg_sys::GENERIC_XLOG_FULL_IMAGE as i32);
    if is_new || pg_sys::PageIsNew(page) {
        pg_sys::PageInit(page, pg_sys::BLCKSZ as usize, 0);
    }

    let contents = pg_sys::PageGetContents(page) as *mut u8;
    let len = data.len() as u32;
    std::ptr::copy_nonoverlapping(len.to_ne_bytes().as_ptr(), contents, 4);
    std::ptr::copy_nonoverlapping(data.as_ptr(), contents.add(4), data.len());

    // Eliminate the page "hole": Generic WAL (and standard page logging) treats the
    // region between pd_lower and pd_upper as empty and zeroes it, which would clobber
    // our content. Setting pd_lower = pd_upper means the whole page is logged verbatim.
    let header = page as *mut pg_sys::PageHeaderData;
    (*header).pd_lower = (*header).pd_upper;

    pg_sys::GenericXLogFinish(state);
    pg_sys::UnlockReleaseBuffer(buffer);
    Ok(())
}

/// Read the byte blob from the index's metapage, or `None` if the index has no metapage.
pub unsafe fn read_blob(index: pg_sys::Relation) -> Option<Vec<u8>> {
    let nblocks = pg_sys::RelationGetNumberOfBlocksInFork(index, pg_sys::ForkNumber::MAIN_FORKNUM);
    if nblocks == 0 {
        return None;
    }
    let buffer = pg_sys::ReadBuffer(index, 0);
    pg_sys::LockBuffer(buffer, pg_sys::BUFFER_LOCK_SHARE as i32);
    let page = pg_sys::BufferGetPage(buffer);
    let result = if pg_sys::PageIsNew(page) {
        None
    } else {
        let contents = pg_sys::PageGetContents(page) as *const u8;
        let mut len_bytes = [0u8; 4];
        std::ptr::copy_nonoverlapping(contents, len_bytes.as_mut_ptr(), 4);
        let len = u32::from_ne_bytes(len_bytes) as usize;
        if len == 0 || len > max_blob_len() {
            None
        } else {
            let mut data = vec![0u8; len];
            std::ptr::copy_nonoverlapping(contents.add(4), data.as_mut_ptr(), len);
            Some(data)
        }
    };
    pg_sys::UnlockReleaseBuffer(buffer);
    result
}

// ---- test-only round-trip harness -------------------------------------------------

#[cfg(any(test, feature = "pg_test"))]
#[pg_extern]
fn table_range_test_page_roundtrip(index: pg_sys::Oid, payload: String) -> String {
    unsafe {
        let rel = pg_sys::index_open(index, pg_sys::AccessExclusiveLock as i32);
        write_blob(rel, payload.as_bytes()).unwrap_or_else(|e| error!("write_blob: {e}"));
        let back = read_blob(rel);
        pg_sys::index_close(rel, pg_sys::AccessExclusiveLock as i32);
        match back {
            Some(b) => String::from_utf8_lossy(&b).into_owned(),
            None => error!("no blob read back"),
        }
    }
}

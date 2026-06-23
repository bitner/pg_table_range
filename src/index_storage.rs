use pgrx::pg_sys;
#[cfg(any(test, feature = "pg_test"))]
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

// ---- typed summary (de)serialization ----------------------------------------------

/// One column's summary, as stored in the index metapage and used by the planner.
#[derive(Clone, Debug, PartialEq)]
pub struct ColSummary {
    /// Heap attnum the summary is for (matched against `Var.varattno` at plan time).
    pub attnum: i16,
    /// `true` -> `min` holds a covering extent for `&&` pruning (range/geometry);
    /// `false` -> `min`/`max` hold the column's btree min/max.
    pub overlap: bool,
    /// SQL type name (for casting in overlap evaluation).
    pub type_name: String,
    pub min: Option<String>,
    pub max: Option<String>,
    pub has_nulls: bool,
    pub all_nulls: bool,
}

/// The whole index's summary: one entry per indexed column.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct IndexSummary {
    pub cols: Vec<ColSummary>,
}

const SUMMARY_VERSION: u8 = 1;

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn put_opt_str(out: &mut Vec<u8>, s: &Option<String>) {
    match s {
        Some(s) => {
            out.push(1);
            put_str(out, s);
        }
        None => out.push(0),
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn u8(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
    fn u16(&mut self) -> Option<u16> {
        let bytes = self.buf.get(self.pos..self.pos + 2)?;
        self.pos += 2;
        Some(u16::from_le_bytes([bytes[0], bytes[1]]))
    }
    fn i16(&mut self) -> Option<i16> {
        self.u16().map(|v| v as i16)
    }
    fn str(&mut self) -> Option<String> {
        let len = self.u16()? as usize;
        let bytes = self.buf.get(self.pos..self.pos + len)?;
        self.pos += len;
        String::from_utf8(bytes.to_vec()).ok()
    }
    fn opt_str(&mut self) -> Option<Option<String>> {
        match self.u8()? {
            0 => Some(None),
            _ => Some(Some(self.str()?)),
        }
    }
}

pub fn serialize(summary: &IndexSummary) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(SUMMARY_VERSION);
    out.extend_from_slice(&(summary.cols.len() as u16).to_le_bytes());
    for c in &summary.cols {
        out.extend_from_slice(&c.attnum.to_le_bytes());
        out.push(c.overlap as u8);
        out.push((c.has_nulls as u8) | ((c.all_nulls as u8) << 1));
        put_str(&mut out, &c.type_name);
        put_opt_str(&mut out, &c.min);
        put_opt_str(&mut out, &c.max);
    }
    out
}

pub fn deserialize(buf: &[u8]) -> Option<IndexSummary> {
    let mut r = Reader { buf, pos: 0 };
    if r.u8()? != SUMMARY_VERSION {
        return None;
    }
    let ncols = r.u16()? as usize;
    let mut cols = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        let attnum = r.i16()?;
        let overlap = r.u8()? != 0;
        let flags = r.u8()?;
        let type_name = r.str()?;
        let min = r.opt_str()?;
        let max = r.opt_str()?;
        cols.push(ColSummary {
            attnum,
            overlap,
            type_name,
            min,
            max,
            has_nulls: flags & 1 != 0,
            all_nulls: flags & 2 != 0,
        });
    }
    Some(IndexSummary { cols })
}

/// Persist the typed summary into the index metapage.
pub unsafe fn write_summary(
    index: pg_sys::Relation,
    summary: &IndexSummary,
) -> Result<(), &'static str> {
    write_blob(index, &serialize(summary))
}

/// Read the typed summary from the index metapage, if present.
pub unsafe fn read_summary(index: pg_sys::Relation) -> Option<IndexSummary> {
    deserialize(&read_blob(index)?)
}

#[cfg(test)]
mod serde_tests {
    use super::*;

    #[test]
    fn summary_roundtrips() {
        let s = IndexSummary {
            cols: vec![
                ColSummary {
                    attnum: 2,
                    overlap: false,
                    type_name: "bigint".into(),
                    min: Some("0".into()),
                    max: Some("99".into()),
                    has_nulls: true,
                    all_nulls: false,
                },
                ColSummary {
                    attnum: 3,
                    overlap: true,
                    type_name: "int8range".into(),
                    min: Some("[0,100)".into()),
                    max: None,
                    has_nulls: false,
                    all_nulls: false,
                },
            ],
        };
        assert_eq!(deserialize(&serialize(&s)), Some(s));
    }

    #[test]
    fn rejects_garbage_and_wrong_version() {
        assert_eq!(deserialize(&[]), None);
        assert_eq!(deserialize(&[99, 0, 0]), None);
    }
}

// ---- test-only round-trip harness -------------------------------------------------

/// Read an index's metapage summary and render it for tests.
#[cfg(any(test, feature = "pg_test"))]
#[pg_extern]
fn table_range_test_read_summary(index: pg_sys::Oid) -> String {
    unsafe {
        let rel = pg_sys::index_open(index, pg_sys::AccessShareLock as i32);
        let s = read_summary(rel);
        pg_sys::index_close(rel, pg_sys::AccessShareLock as i32);
        match s {
            None => "none".to_string(),
            Some(s) => s
                .cols
                .iter()
                .map(|c| {
                    format!(
                        "attnum={} overlap={} min={:?} max={:?} has_nulls={} all_nulls={}",
                        c.attnum, c.overlap, c.min, c.max, c.has_nulls, c.all_nulls
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
        }
    }
}

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

use pgrx::pg_sys;
#[cfg(any(test, feature = "pg_test"))]
use pgrx::prelude::*;

// Low-level storage for a per-index summary, kept in the index's own metapage (block 0)
// and updated in place with the Generic WAL API — the same model BRIN uses. Because a
// table_range summary only ever needs to be *over-inclusive*, these in-place page
// updates need no MVCC/transactionality: a rolled-back widening that over-covers is
// always safe.

// Usable bytes in the metapage content area (after the page header + length prefix). A
// generous margin below BLCKSZ keeps us clear of the page header regardless of alignment.
pub(crate) fn max_blob_len() -> usize {
    pg_sys::BLCKSZ as usize - 64
}

unsafe fn page_blob(page: pg_sys::Page) -> Option<Vec<u8>> {
    if pg_sys::PageIsNew(page) {
        return None;
    }
    let contents = pg_sys::PageGetContents(page) as *const u8;
    let mut len_bytes = [0u8; 4];
    std::ptr::copy_nonoverlapping(contents, len_bytes.as_mut_ptr(), 4);
    let len = u32::from_ne_bytes(len_bytes) as usize;
    if len == 0 || len > max_blob_len() {
        return None;
    }
    let mut data = vec![0u8; len];
    std::ptr::copy_nonoverlapping(contents.add(4), data.as_mut_ptr(), len);
    Some(data)
}

unsafe fn put_blob(index: pg_sys::Relation, buffer: pg_sys::Buffer, is_new: bool, data: &[u8]) {
    let state = pg_sys::GenericXLogStart(index);
    // Delta mode: a widening logs the bytes that changed, not the whole page.
    let page = pg_sys::GenericXLogRegisterBuffer(state, buffer, 0);
    if is_new || pg_sys::PageIsNew(page) {
        pg_sys::PageInit(page, pg_sys::BLCKSZ as usize, 0);
    }

    let contents = pg_sys::PageGetContents(page) as *mut u8;
    let len = data.len() as u32;
    std::ptr::copy_nonoverlapping(len.to_ne_bytes().as_ptr(), contents, 4);
    std::ptr::copy_nonoverlapping(data.as_ptr(), contents.add(4), data.len());

    // WAL replay zeroes the "hole" between pd_lower and pd_upper, so pd_lower must cover
    // the data.
    let header = page as *mut pg_sys::PageHeaderData;
    (*header).pd_lower = (contents.add(4 + data.len()) as usize - page as usize) as u16;

    pg_sys::GenericXLogFinish(state);
}

/// For `ambuild`, which holds the index exclusively; `aminsert` uses `replace_blob_if`.
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
    put_blob(index, buffer, is_new, data);
    pg_sys::UnlockReleaseBuffer(buffer);
    Ok(())
}

/// Writes only if the blob still equals `expected`; `Ok(false)` means re-read and retry.
pub unsafe fn replace_blob_if(
    index: pg_sys::Relation,
    expected: &[u8],
    data: &[u8],
) -> Result<bool, &'static str> {
    if data.len() > max_blob_len() {
        return Err("table_range: summary blob exceeds one page");
    }
    let nblocks = pg_sys::RelationGetNumberOfBlocksInFork(index, pg_sys::ForkNumber::MAIN_FORKNUM);
    if nblocks == 0 {
        return Ok(false);
    }
    let buffer = pg_sys::ReadBuffer(index, 0);
    pg_sys::LockBuffer(buffer, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
    let current = page_blob(pg_sys::BufferGetPage(buffer));
    let matches = current.as_deref() == Some(expected);
    if matches {
        put_blob(index, buffer, false, data);
    }
    pg_sys::UnlockReleaseBuffer(buffer);
    Ok(matches)
}

/// Read the byte blob from the index's metapage, or `None` if the index has no metapage.
pub unsafe fn read_blob(index: pg_sys::Relation) -> Option<Vec<u8>> {
    let nblocks = pg_sys::RelationGetNumberOfBlocksInFork(index, pg_sys::ForkNumber::MAIN_FORKNUM);
    if nblocks == 0 {
        return None;
    }
    let buffer = pg_sys::ReadBuffer(index, 0);
    pg_sys::LockBuffer(buffer, pg_sys::BUFFER_LOCK_SHARE as i32);
    let result = page_blob(pg_sys::BufferGetPage(buffer));
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
    /// Schema-qualified, so the SQL that maintains extents ignores the caller's search_path.
    pub type_name: String,
    /// Min/max order; only comparisons under this collation are pruned.
    pub collation: pg_sys::Oid,
    pub min: Option<String>,
    pub max: Option<String>,
    pub has_nulls: bool,
    pub all_nulls: bool,
    /// Bounds unknown (too large or not representable): only null flags prune.
    pub unbounded: bool,
}

impl ColSummary {
    pub fn make_unbounded(&mut self) {
        self.unbounded = true;
        self.min = None;
        self.max = None;
    }
}

/// The whole index's summary: one entry per indexed column.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct IndexSummary {
    pub cols: Vec<ColSummary>,
    /// Mid-rebuild: `cols` holds only values inserted since it started, so never prune.
    pub summarizing: bool,
}

// Formats older than version 2 are not read on purpose (they may not cover their data);
// VACUUM and table_range_summarize rebuild them.
const FORMAT_PLAIN: u8 = 2;
/// FORMAT_PLAIN plus a header flags byte. Written only while summarizing, so pages stay
/// readable by binaries that know only FORMAT_PLAIN.
const FORMAT_WITH_HEADER: u8 = 3;

const HEADER_SUMMARIZING: u8 = 1;

const FLAG_HAS_NULLS: u8 = 1;
const FLAG_ALL_NULLS: u8 = 2;
const FLAG_UNBOUNDED: u8 = 4;

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
    fn u32(&mut self) -> Option<u32> {
        let bytes = self.buf.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
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
    if summary.summarizing {
        out.extend_from_slice(&[FORMAT_WITH_HEADER, HEADER_SUMMARIZING]);
    } else {
        out.push(FORMAT_PLAIN);
    }
    out.extend_from_slice(&(summary.cols.len() as u16).to_le_bytes());
    for c in &summary.cols {
        out.extend_from_slice(&c.attnum.to_le_bytes());
        out.push(c.overlap as u8);
        let mut flags = 0;
        if c.has_nulls {
            flags |= FLAG_HAS_NULLS;
        }
        if c.all_nulls {
            flags |= FLAG_ALL_NULLS;
        }
        if c.unbounded {
            flags |= FLAG_UNBOUNDED;
        }
        out.push(flags);
        out.extend_from_slice(&u32::from(c.collation).to_le_bytes());
        put_str(&mut out, &c.type_name);
        put_opt_str(&mut out, &c.min);
        put_opt_str(&mut out, &c.max);
    }
    out
}

pub fn deserialize(buf: &[u8]) -> Option<IndexSummary> {
    let mut r = Reader { buf, pos: 0 };
    let header = match r.u8()? {
        FORMAT_WITH_HEADER => r.u8()?,
        FORMAT_PLAIN => 0,
        _ => return None,
    };
    let ncols = r.u16()? as usize;
    let mut cols = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        let attnum = r.i16()?;
        let overlap = r.u8()? != 0;
        let flags = r.u8()?;
        let collation = pg_sys::Oid::from(r.u32()?);
        let type_name = r.str()?;
        let min = r.opt_str()?;
        let max = r.opt_str()?;
        cols.push(ColSummary {
            attnum,
            overlap,
            type_name,
            collation,
            min,
            max,
            has_nulls: flags & FLAG_HAS_NULLS != 0,
            all_nulls: flags & FLAG_ALL_NULLS != 0,
            unbounded: flags & FLAG_UNBOUNDED != 0,
        });
    }
    Some(IndexSummary {
        cols,
        summarizing: header & HEADER_SUMMARIZING != 0,
    })
}

/// Drops the largest columns' bounds until it fits: less selective, never too narrow.
pub fn encode_fitting(summary: &mut IndexSummary) -> Vec<u8> {
    loop {
        let bytes = serialize(summary);
        if bytes.len() <= max_blob_len() {
            return bytes;
        }
        let bound_len = |c: &ColSummary| {
            c.min.as_ref().map_or(0, String::len) + c.max.as_ref().map_or(0, String::len)
        };
        match summary
            .cols
            .iter_mut()
            .filter(|c| bound_len(c) > 0)
            .max_by_key(|c| bound_len(c))
        {
            Some(c) => c.make_unbounded(),
            // Nothing left to drop; the caller's write will report the size error.
            None => return bytes,
        }
    }
}

/// Persist the typed summary into the index metapage.
pub unsafe fn write_summary(
    index: pg_sys::Relation,
    summary: &mut IndexSummary,
) -> Result<(), &'static str> {
    write_blob(index, &encode_fitting(summary))
}

/// Read the typed summary from the index metapage, if present.
pub unsafe fn read_summary(index: pg_sys::Relation) -> Option<IndexSummary> {
    deserialize(&read_blob(index)?)
}

#[cfg(test)]
mod serde_tests {
    use super::*;

    fn col(attnum: i16, min: &str, max: &str) -> ColSummary {
        ColSummary {
            attnum,
            overlap: false,
            type_name: "pg_catalog.text".into(),
            collation: pg_sys::Oid::from(100),
            min: Some(min.into()),
            max: Some(max.into()),
            has_nulls: false,
            all_nulls: false,
            unbounded: false,
        }
    }

    #[test]
    fn summary_roundtrips() {
        let s = IndexSummary {
            cols: vec![
                ColSummary {
                    attnum: 2,
                    overlap: false,
                    type_name: "pg_catalog.int8".into(),
                    collation: pg_sys::Oid::INVALID,
                    min: Some("0".into()),
                    max: Some("99".into()),
                    has_nulls: true,
                    all_nulls: false,
                    unbounded: false,
                },
                ColSummary {
                    attnum: 3,
                    overlap: true,
                    type_name: "pg_catalog.int8range".into(),
                    collation: pg_sys::Oid::INVALID,
                    min: Some("[0,100)".into()),
                    max: None,
                    has_nulls: false,
                    all_nulls: false,
                    unbounded: false,
                },
                ColSummary {
                    unbounded: true,
                    min: None,
                    max: None,
                    has_nulls: true,
                    ..col(4, "", "")
                },
            ],
            summarizing: true,
        };
        assert_eq!(deserialize(&serialize(&s)), Some(s));
    }

    #[test]
    fn reads_and_writes_plain_format() {
        // A format-2 page: a bigint column with bounds [0, 99].
        let plain = [
            2, 1, 0, // format, ncols
            2, 0, 0, 0, // attnum 2, minmax, no flags
            0, 0, 0, 0, // collation
            15, 0, b'p', b'g', b'_', b'c', b'a', b't', b'a', b'l', b'o', b'g', b'.', b'i', b'n',
            b't', b'8', // type name
            1, 1, 0, b'0', // min
            1, 2, 0, b'9', b'9', // max
        ];
        let s = IndexSummary {
            cols: vec![ColSummary {
                attnum: 2,
                overlap: false,
                type_name: "pg_catalog.int8".into(),
                collation: pg_sys::Oid::INVALID,
                min: Some("0".into()),
                max: Some("99".into()),
                has_nulls: false,
                all_nulls: false,
                unbounded: false,
            }],
            summarizing: false,
        };
        assert_eq!(deserialize(&plain), Some(s.clone()));
        assert_eq!(
            serialize(&s),
            plain,
            "a summary that is not summarizing stays plain"
        );
    }

    #[test]
    fn rejects_garbage_and_wrong_version() {
        assert_eq!(deserialize(&[]), None);
        assert_eq!(deserialize(&[99, 0, 0]), None);
        // Any other format version is never trusted, even if well-formed.
        let s = IndexSummary {
            cols: vec![col(1, "a", "b")],
            summarizing: true,
        };
        for version in [1, 4] {
            let mut bytes = serialize(&s);
            bytes[0] = version;
            assert_eq!(deserialize(&bytes), None, "format {version}");
        }
    }

    #[test]
    fn oversized_summary_drops_largest_bounds_first() {
        let big = "x".repeat(max_blob_len());
        let mut s = IndexSummary {
            cols: vec![col(1, "a", "b"), col(2, "a", &big)],
            summarizing: false,
        };
        let bytes = encode_fitting(&mut s);
        assert!(bytes.len() <= max_blob_len());
        assert!(!s.cols[0].unbounded, "small column keeps its bounds");
        assert!(s.cols[1].unbounded && s.cols[1].max.is_none());
        assert_eq!(deserialize(&bytes), Some(s));
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
            Some(s) => {
                format!("summarizing={} ", s.summarizing)
                    + &s.cols
                        .iter()
                        .map(|c| {
                            format!(
                                "attnum={} overlap={} min={:?} max={:?} has_nulls={} all_nulls={} \
                         unbounded={}",
                                c.attnum,
                                c.overlap,
                                c.min,
                                c.max,
                                c.has_nulls,
                                c.all_nulls,
                                c.unbounded
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; ")
            }
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

#[cfg(any(test, feature = "pg_test"))]
#[pg_extern]
fn table_range_test_write_blob(index: pg_sys::Oid, payload: &[u8]) {
    unsafe {
        let rel = pg_sys::index_open(index, pg_sys::AccessExclusiveLock as i32);
        write_blob(rel, payload).unwrap_or_else(|e| error!("write_blob: {e}"));
        crate::summary_cache::note_changed(index, pg_sys::IndexGetRelation(index, false));
        pg_sys::index_close(rel, pg_sys::AccessExclusiveLock as i32);
    }
}

/// Overwrite one column's stored bounds (e.g. wider than the data).
#[cfg(any(test, feature = "pg_test"))]
#[pg_extern]
fn table_range_test_set_bounds(index: pg_sys::Oid, attnum: i16, min: String, max: String) {
    unsafe {
        let rel = pg_sys::index_open(index, pg_sys::AccessExclusiveLock as i32);
        let mut summary = read_summary(rel).unwrap_or_else(|| error!("no readable summary"));
        let col = summary
            .cols
            .iter_mut()
            .find(|c| c.attnum == attnum)
            .unwrap_or_else(|| error!("no summary for column {attnum}"));
        col.min = Some(min);
        col.max = Some(max);
        col.unbounded = false;
        write_summary(rel, &mut summary).unwrap_or_else(|e| error!("{e}"));
        crate::summary_cache::note_changed(index, pg_sys::IndexGetRelation(index, false));
        pg_sys::index_close(rel, pg_sys::AccessExclusiveLock as i32);
    }
}

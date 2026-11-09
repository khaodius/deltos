//! The on-disk store for a statement's rows and its binary row encoding.
//!
//! A [`SpillStore`] is one anonymous file in the temporary directory that rows are
//! appended to as they arrive and read back by page. On Unix the file is unlinked right
//! after it is created, so the operating system reclaims it even if the process dies; on
//! other systems it is removed when the store is dropped. The only thing kept in memory
//! is a sparse index: one file offset per page, 8 bytes for every `page_rows` rows, so a
//! result of a hundred million rows indexes in under two megabytes.
//!
//! # Encoding
//!
//! All integers are unsigned LEB128 varints unless stated. A file is a sequence of rows
//! with nothing before or between them:
//!
//! ```text
//! row    := varint(len) body            len is the byte length of body
//! body   := varint(count) value*        count values follow
//! value  := 0x00                        Null
//!         | 0x01 | 0x02                 Bool false | Bool true
//!         | 0x03 varint(zigzag(i64))    Int
//!         | 0x04 f64 little-endian      Float, 8 bytes
//!         | 0x05 varint(len) utf8       Text
//!         | 0x06 varint(len) bytes      Bytes
//!         | 0x07 varint(len) utf8       Json
//! ```
//!
//! The row length prefix lets a reader skip rows and lets the decoder check that every
//! row ends where it said it would. Decoding is strict: a bad tag, a short buffer or
//! invalid UTF-8 is an error, never a panic, since the file is the app's own but disks
//! and temporary directories are not under its control.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use deltos_data::{Error, Result, Row, Value};

const TAG_NULL: u8 = 0;
const TAG_FALSE: u8 = 1;
const TAG_TRUE: u8 = 2;
const TAG_INT: u8 = 3;
const TAG_FLOAT: u8 = 4;
const TAG_TEXT: u8 = 5;
const TAG_BYTES: u8 = 6;
const TAG_JSON: u8 = 7;

/// A write buffer this large is given back after a huge batch.
const BUFFER_KEEP: usize = 16 << 20;

static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

/// Where one page's rows lie in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageSpan {
    pub offset: u64,
    pub len: usize,
    /// Rows in the span: `page_rows`, or fewer for the last page so far.
    pub rows: usize,
}

/// Rows appended to a temporary file, readable by page.
pub struct SpillStore {
    file: Arc<File>,
    path: PathBuf,
    linked: bool,
    len: u64,
    rows: u64,
    page_rows: u32,
    /// File offset where each page starts.
    index: Vec<u64>,
    buf: Vec<u8>,
}

impl std::fmt::Debug for SpillStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpillStore")
            .field("rows", &self.rows)
            .field("bytes", &self.len)
            .finish_non_exhaustive()
    }
}

impl SpillStore {
    /// Creates the file under `dir`, or the system's temporary directory.
    pub fn create(page_rows: u32, dir: Option<&Path>) -> io::Result<Self> {
        let dir = dir
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        for _ in 0..32 {
            let serial = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!(
                "deltos-spill-{}-{serial}-{nanos:x}.bin",
                std::process::id()
            ));
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    let linked = !unlink_early(&path);
                    return Ok(Self {
                        file: Arc::new(file),
                        path,
                        linked,
                        len: 0,
                        rows: 0,
                        page_rows: page_rows.max(1),
                        index: Vec::new(),
                        buf: Vec::new(),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not find a free name for the spill file",
        ))
    }

    /// Where the file is, or was before it was unlinked.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the file still has a name in the directory. On Unix it does not; it is
    /// removed as soon as it is open.
    pub fn is_linked(&self) -> bool {
        self.linked
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    pub fn bytes(&self) -> u64 {
        self.len
    }

    pub fn page_rows(&self) -> u32 {
        self.page_rows
    }

    /// The shared handle for reading outside the store's lock.
    pub fn file(&self) -> Arc<File> {
        self.file.clone()
    }

    /// Encodes and writes `rows` at the end. Nothing is recorded until the write
    /// succeeded, so a failed write leaves the store as it was.
    pub fn append(&mut self, rows: &[Row]) -> io::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let page_rows = u64::from(self.page_rows);
        let mut buf = std::mem::take(&mut self.buf);
        buf.clear();
        let mut new_index = Vec::new();
        let mut count = self.rows;
        for row in rows {
            if count.is_multiple_of(page_rows) {
                new_index.push(self.len + buf.len() as u64);
            }
            encode_row(&mut buf, row);
            count += 1;
        }
        let written = write_all_at(&self.file, &buf, self.len);
        let added = buf.len() as u64;
        if buf.capacity() <= BUFFER_KEEP {
            self.buf = buf;
        }
        written?;
        self.len += added;
        self.rows = count;
        self.index.extend(new_index);
        Ok(())
    }

    /// Where page `page` lies, or `None` when none of its rows have been written.
    pub fn page_span(&self, page: u64) -> Option<PageSpan> {
        let page_rows = u64::from(self.page_rows);
        let start = page.checked_mul(page_rows)?;
        if start >= self.rows {
            return None;
        }
        let index = usize::try_from(page).ok()?;
        let offset = *self.index.get(index)?;
        let end = self.index.get(index + 1).copied().unwrap_or(self.len);
        Some(PageSpan {
            offset,
            len: usize::try_from(end - offset).ok()?,
            rows: (self.rows - start).min(page_rows) as usize,
        })
    }

    /// Reads and decodes one page under the store's lock. [`read_span`] does the same
    /// from a cloned handle without holding the lock.
    pub fn read_page(&self, page: u64) -> Result<Vec<Row>> {
        match self.page_span(page) {
            Some(span) => read_span(&self.file, &span),
            None => Ok(Vec::new()),
        }
    }
}

impl Drop for SpillStore {
    fn drop(&mut self) {
        if self.linked {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Removes the name of a freshly opened file where the open handle keeps it alive.
/// Returns whether the name is gone.
fn unlink_early(path: &Path) -> bool {
    #[cfg(unix)]
    {
        std::fs::remove_file(path).is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

#[cfg(test)]
fn encoded_len(rows: &[Row]) -> u64 {
    let mut n = 0u64;
    for row in rows {
        let body = body_len(row);
        n += varint_len(body as u64) as u64 + body as u64;
    }
    n
}

fn body_len(row: &[Value]) -> usize {
    let mut n = varint_len(row.len() as u64);
    for value in row {
        n += 1 + match value {
            Value::Null | Value::Bool(_) => 0,
            Value::Int(i) => varint_len(zigzag(*i)),
            Value::Float(_) => 8,
            Value::Text(s) | Value::Json(s) => varint_len(s.len() as u64) + s.len(),
            Value::Bytes(b) => varint_len(b.len() as u64) + b.len(),
        };
    }
    n
}

fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

fn zigzag(i: i64) -> u64 {
    ((i << 1) ^ (i >> 63)) as u64
}

fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

/// Appends one row in the file format.
pub fn encode_row(buf: &mut Vec<u8>, row: &[Value]) {
    put_varint(buf, body_len(row) as u64);
    put_varint(buf, row.len() as u64);
    for value in row {
        match value {
            Value::Null => buf.push(TAG_NULL),
            Value::Bool(false) => buf.push(TAG_FALSE),
            Value::Bool(true) => buf.push(TAG_TRUE),
            Value::Int(i) => {
                buf.push(TAG_INT);
                put_varint(buf, zigzag(*i));
            }
            Value::Float(f) => {
                buf.push(TAG_FLOAT);
                buf.extend_from_slice(&f.to_le_bytes());
            }
            Value::Text(s) => {
                buf.push(TAG_TEXT);
                put_varint(buf, s.len() as u64);
                buf.extend_from_slice(s.as_bytes());
            }
            Value::Bytes(b) => {
                buf.push(TAG_BYTES);
                put_varint(buf, b.len() as u64);
                buf.extend_from_slice(b);
            }
            Value::Json(s) => {
                buf.push(TAG_JSON);
                put_varint(buf, s.len() as u64);
                buf.extend_from_slice(s.as_bytes());
            }
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn corrupt(what: &str) -> Error {
        Error::Other(format!("the spill file is damaged: {what}"))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| Self::corrupt("a value runs past the end"))?;
        let out = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn varint(&mut self) -> Result<u64> {
        let mut out = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            out |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Ok(out);
            }
        }
        Err(Self::corrupt("a length does not fit in 64 bits"))
    }

    fn len(&mut self) -> Result<usize> {
        usize::try_from(self.varint()?).map_err(|_| Self::corrupt("a length is too large"))
    }

    fn str(&mut self) -> Result<Box<str>> {
        let n = self.len()?;
        std::str::from_utf8(self.take(n)?)
            .map(Box::from)
            .map_err(|_| Self::corrupt("text is not UTF-8"))
    }

    fn value(&mut self) -> Result<Value> {
        Ok(match self.byte()? {
            TAG_NULL => Value::Null,
            TAG_FALSE => Value::Bool(false),
            TAG_TRUE => Value::Bool(true),
            TAG_INT => Value::Int(unzigzag(self.varint()?)),
            TAG_FLOAT => {
                let bytes: [u8; 8] = self
                    .take(8)?
                    .try_into()
                    .map_err(|_| Self::corrupt("a float is short"))?;
                Value::Float(f64::from_le_bytes(bytes))
            }
            TAG_TEXT => Value::Text(self.str()?),
            TAG_BYTES => {
                let n = self.len()?;
                Value::Bytes(Box::from(self.take(n)?))
            }
            TAG_JSON => Value::Json(self.str()?),
            tag => return Err(Self::corrupt(&format!("unknown value tag {tag}"))),
        })
    }

    fn row(&mut self) -> Result<Row> {
        let len = self.len()?;
        let body = self.take(len)?;
        let mut inner = Reader {
            bytes: body,
            pos: 0,
        };
        let count = inner.len()?;
        if count > body.len() {
            return Err(Self::corrupt("a row claims more values than it has bytes"));
        }
        let mut row = Vec::with_capacity(count);
        for _ in 0..count {
            row.push(inner.value()?);
        }
        if inner.pos != body.len() {
            return Err(Self::corrupt("a row has trailing bytes"));
        }
        Ok(row.into_boxed_slice())
    }
}

/// Decodes exactly `count` rows that fill `bytes`.
pub fn decode_rows(bytes: &[u8], count: usize) -> Result<Vec<Row>> {
    let mut reader = Reader { bytes, pos: 0 };
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        rows.push(reader.row()?);
    }
    if reader.pos != bytes.len() {
        return Err(Reader::corrupt("a page has trailing bytes"));
    }
    Ok(rows)
}

/// Reads one page's bytes from the file and decodes them. Safe to call from any thread
/// while another appends, since reads are positioned and never touch a shared cursor.
pub fn read_span(file: &File, span: &PageSpan) -> Result<Vec<Row>> {
    let mut buf = vec![0u8; span.len];
    read_exact_at(file, &mut buf, span.offset)
        .map_err(|e| Error::Other(format!("could not read the spill file: {e}")))?;
    decode_rows(&buf, span.rows)
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(unix)]
fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn read_exact_at(mut file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buf)
}

#[cfg(not(any(unix, windows)))]
fn write_all_at(mut file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::fake_row;

    fn same(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
            _ => a == b,
        }
    }

    fn every_variant() -> Vec<Row> {
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let long_text: String = "zürich→東京 🌍 ".repeat(20_000);
        vec![
            vec![
                Value::Null,
                Value::Bool(true),
                Value::Bool(false),
                Value::Int(0),
                Value::Int(-1),
                Value::Int(i64::MIN),
                Value::Int(i64::MAX),
                Value::Float(0.0),
                Value::Float(-0.0),
                Value::Float(f64::NAN),
                Value::Float(f64::NEG_INFINITY),
                Value::Float(1.0e-300),
                Value::text(""),
                Value::text("plain"),
                Value::text("tab\tnew\nline\r\n\"quote\" ünïcödé 日本語 🙂"),
                Value::Bytes(Box::new([])),
                Value::Bytes(Box::new([0, 1, 2, 254, 255])),
                Value::Json("".into()),
                Value::Json("{\"a\":[1,2,{\"b\":null}]}".into()),
            ]
            .into_boxed_slice(),
            Vec::new().into_boxed_slice(),
            vec![Value::Bytes(big.into()), Value::text(long_text)].into_boxed_slice(),
            fake_row(123_456_789, 6),
        ]
    }

    #[test]
    fn round_trips_every_variant() {
        let rows = every_variant();
        let mut buf = Vec::new();
        for row in &rows {
            encode_row(&mut buf, row);
        }
        assert_eq!(buf.len() as u64, encoded_len(&rows));
        let decoded = decode_rows(&buf, rows.len()).unwrap();
        assert_eq!(decoded.len(), rows.len());
        for (a, b) in rows.iter().zip(&decoded) {
            assert_eq!(a.len(), b.len());
            assert!(
                a.iter().zip(b.iter()).all(|(x, y)| same(x, y)),
                "{a:?} != {b:?}"
            );
        }
    }

    #[test]
    fn store_pages_and_removes_its_file() {
        let path = {
            let mut store = SpillStore::create(3, None).unwrap();
            assert_eq!(store.path().exists(), store.is_linked());
            let rows: Vec<Row> = (0..10).map(|i| fake_row(i, 4)).collect();
            store.append(&rows[..2]).unwrap();
            store.append(&rows[2..7]).unwrap();
            store.append(&[]).unwrap();
            store.append(&rows[7..]).unwrap();
            assert_eq!(store.rows(), 10);
            assert_eq!(store.index.len(), 4);
            assert_eq!(store.bytes(), encoded_len(&rows));

            for page in 0..4u64 {
                let expect: Vec<Row> =
                    rows[page as usize * 3..(page as usize * 3 + 3).min(10)].to_vec();
                assert_eq!(store.read_page(page).unwrap(), expect, "page {page}");
                let span = store.page_span(page).unwrap();
                assert_eq!(read_span(&store.file(), &span).unwrap(), expect);
            }
            assert!(store.page_span(4).is_none());
            assert!(store.read_page(99).unwrap().is_empty());
            assert!(store.page_span(u64::MAX).is_none());
            store.path().to_path_buf()
        };
        assert!(!path.exists(), "the spill file outlived its store");
    }

    #[test]
    fn damaged_data_is_an_error_not_a_panic() {
        let mut buf = Vec::new();
        encode_row(&mut buf, &fake_row(7, 4));
        assert!(
            decode_rows(&buf, 2).is_err(),
            "asking for more rows than there are"
        );
        assert!(decode_rows(&buf, 0).is_err(), "trailing bytes");
        assert!(decode_rows(&buf[..buf.len() - 1], 1).is_err(), "cut short");
        let mut bad_tag = buf.clone();
        bad_tag[2] = 0x7f;
        assert!(decode_rows(&bad_tag, 1).is_err());
        let mut bad_utf8 = Vec::new();
        encode_row(&mut bad_utf8, &[Value::text("ab")]);
        let n = bad_utf8.len();
        bad_utf8[n - 1] = 0xff;
        assert!(decode_rows(&bad_utf8, 1).is_err());
        assert!(
            decode_rows(
                &[
                    0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01
                ],
                1
            )
            .is_err()
        );
        assert!(
            decode_rows(&[2, 0xff, 0xff], 1).is_err(),
            "count larger than body"
        );
    }

    #[test]
    fn zigzag_covers_the_range() {
        for i in [0, 1, -1, 2, -2, i64::MAX, i64::MIN, 1 << 40, -(1 << 40)] {
            assert_eq!(unzigzag(zigzag(i)), i);
        }
        assert_eq!(zigzag(-1), 1);
        assert_eq!(zigzag(1), 2);
    }

    /// `cargo test -p deltos-grid --release -- --ignored --nocapture spill_decode_rate`
    #[test]
    #[ignore = "benchmark; run by hand"]
    fn spill_decode_rate() {
        let rows_total = 2_000_000u64;
        let mut store = SpillStore::create(500, None).unwrap();
        let mut batch = Vec::with_capacity(1000);
        let write_started = std::time::Instant::now();
        for start in (0..rows_total).step_by(1000) {
            batch.clear();
            batch.extend((start..start + 1000).map(|i| fake_row(i, 6)));
            store.append(&batch).unwrap();
        }
        let write_secs = write_started.elapsed().as_secs_f64();
        let started = std::time::Instant::now();
        let mut decoded = 0u64;
        for page in 0..rows_total / 500 {
            decoded += store.read_page(page).unwrap().len() as u64;
        }
        let secs = started.elapsed().as_secs_f64();
        assert_eq!(decoded, rows_total);
        println!(
            "spill: {:.0} rows/s encode+write, {:.0} rows/s read+decode, {:.1} bytes/row",
            rows_total as f64 / write_secs,
            rows_total as f64 / secs,
            store.bytes() as f64 / rows_total as f64
        );
    }
}

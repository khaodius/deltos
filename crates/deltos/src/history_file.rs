//! The query history on disk: one JSON object per line in the data folder, the newest
//! last. A thread of its own reads it at start and writes every change, in the order the
//! window made them, and cuts the file back to the last [`LIMIT`] entries (and at most
//! [`MAX_BYTES`] of them) now and then. Removing entries is done to the file by that
//! thread too, so the window never writes the whole history itself.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deltos_settings::Paths;
use futures::channel::oneshot;
use gpui::{App, Entity, Global};
use serde::{Deserialize, Serialize};

use crate::state::{AppState, HistoryEntry, StateEvent};

/// Entries kept, in memory and in the file.
pub const LIMIT: usize = 5_000;
/// Lines the file may hold beyond [`LIMIT`] before it is rewritten.
const SLACK: usize = 1_000;
/// Bytes of entries kept, however few they are: long statements fill it sooner.
const MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Bytes the file may hold beyond [`MAX_BYTES`] before it is rewritten.
const SLACK_BYTES: u64 = 8 * 1024 * 1024;
/// A statement longer than this stays in this session's history only.
const MAX_STATEMENT: usize = 256 * 1024;
/// Error messages are cut to this length in the file.
const MAX_ERROR: usize = 4 * 1024;
/// A line longer than this is not an entry Deltos wrote; it is skipped unread.
const MAX_LINE: usize = MAX_STATEMENT + MAX_ERROR + 64 * 1024;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Line {
    /// Milliseconds since 1970.
    at: u64,
    connection: String,
    statement: String,
    /// How long the statement took, in microseconds.
    micros: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    rows: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// The entry as a line of the file, without the newline. `None` for a statement too long
/// to keep.
fn to_line(entry: &HistoryEntry) -> Option<String> {
    if entry.statement.len() > MAX_STATEMENT {
        return None;
    }
    let line = Line {
        at: entry
            .at
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
        connection: entry.connection.clone(),
        statement: entry.statement.clone(),
        micros: entry.duration.as_micros().min(u128::from(u64::MAX)) as u64,
        rows: entry.rows,
        error: entry.error.as_deref().map(|e| cut(e, MAX_ERROR).to_owned()),
    };
    serde_json::to_string(&line).ok()
}

fn from_line(text: &str) -> Option<HistoryEntry> {
    let line: Line = serde_json::from_str(text).ok()?;
    if line.statement.is_empty() {
        return None;
    }
    Some(HistoryEntry {
        at: UNIX_EPOCH + Duration::from_millis(line.at),
        connection: line.connection,
        statement: line.statement,
        duration: Duration::from_micros(line.micros),
        rows: line.rows,
        error: line.error,
    })
}

fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The next line of `reader` into `buf`, without its newline. `Some(false)` for a line
/// longer than [`MAX_LINE`], which is skipped without holding it in memory; `None` at the
/// end or on an error.
fn next_line(reader: &mut impl BufRead, buf: &mut Vec<u8>) -> Option<bool> {
    buf.clear();
    let read = reader
        .by_ref()
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', buf)
        .ok()?;
    if read == 0 {
        return None;
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        return Some(true);
    }
    if buf.len() <= MAX_LINE {
        // The last line, without a newline.
        return Some(true);
    }
    // Too long: drop the rest of it.
    loop {
        let chunk = reader.fill_buf().ok()?;
        if chunk.is_empty() {
            return Some(false);
        }
        match chunk.iter().position(|b| *b == b'\n') {
            Some(at) => {
                reader.consume(at + 1);
                return Some(false);
            }
            None => {
                let n = chunk.len();
                reader.consume(n);
            }
        }
    }
}

/// The last `limit` lines that hold an entry and together take at most `max_bytes`, and
/// how many lines the file has.
fn read_lines(reader: impl Read, limit: usize, max_bytes: u64) -> (VecDeque<String>, usize) {
    let mut reader = BufReader::new(reader);
    let mut out: VecDeque<String> = VecDeque::new();
    let mut bytes = 0u64;
    let mut count = 0;
    let mut buf = Vec::new();
    while let Some(ok) = next_line(&mut reader, &mut buf) {
        count += 1;
        if !ok {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&buf) else {
            continue;
        };
        if text.trim().is_empty() || from_line(text).is_none() {
            continue;
        }
        out.push_back(text.to_owned());
        bytes += text.len() as u64 + 1;
        while out.len() > limit || (bytes > max_bytes && out.len() > 1) {
            if let Some(first) = out.pop_front() {
                bytes -= first.len() as u64 + 1;
            }
        }
    }
    (out, count)
}

/// Entries to take out of the history.
#[derive(Debug, Clone, PartialEq)]
enum Filter {
    /// The one that ran at this millisecond with this statement.
    One {
        at_ms: u64,
        statement: String,
    },
    Connection(String),
    All,
}

impl Filter {
    fn removes(&self, entry: &HistoryEntry) -> bool {
        match self {
            Filter::One { at_ms, statement } => {
                millis(entry.at) == *at_ms && entry.statement == *statement
            }
            Filter::Connection(id) => entry.connection == *id,
            Filter::All => true,
        }
    }
}

fn millis(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

enum Op {
    Load(oneshot::Sender<VecDeque<HistoryEntry>>),
    Append(String),
    Remove(Filter),
}

struct Writer {
    ops: mpsc::Sender<Op>,
    /// Whether the entries read at start are in the window's history yet.
    loaded: bool,
    /// What was taken out before then, to take out of them as they arrive.
    removed: Vec<Filter>,
}

impl Global for Writer {}

/// The entries of the file as lines, within the limits, and how many lines it has. `None`
/// when it cannot be read, so that nothing is written over it.
fn read_file(path: &Path) -> Option<(VecDeque<String>, usize)> {
    match std::fs::File::open(path) {
        Ok(f) => Some(read_lines(f, LIMIT, MAX_BYTES)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some((VecDeque::new(), 0)),
        Err(e) => {
            tracing::warn!("could not read the query history: {e}");
            None
        }
    }
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// What the history thread does with each request, against the file at `path`.
fn serve(path: &Path, ops: mpsc::Receiver<Op>) {
    // Lines and bytes in the file, to know when to cut it back.
    let mut lines = 0;
    let mut bytes = 0;
    for op in ops {
        match op {
            Op::Load(reply) => {
                let (kept, count) = read_file(path).unwrap_or_default();
                lines = count;
                bytes = file_size(path);
                let entries = kept.iter().filter_map(|l| from_line(l)).collect();
                let _ = reply.send(entries);
            }
            Op::Append(line) => {
                if let Err(e) = append(path, &line) {
                    tracing::warn!("could not write the query history: {e}");
                    continue;
                }
                lines += 1;
                bytes += line.len() as u64 + 1;
                if (lines > LIMIT + SLACK || bytes > MAX_BYTES + SLACK_BYTES)
                    && let Some((kept, _)) = read_file(path)
                {
                    lines = kept.len();
                    rewrite(path, kept.into_iter());
                    bytes = file_size(path);
                }
            }
            Op::Remove(filter) => {
                let kept = if filter == Filter::All {
                    VecDeque::new()
                } else {
                    let Some((kept, _)) = read_file(path) else {
                        continue;
                    };
                    kept.into_iter()
                        .filter(|l| from_line(l).is_none_or(|e| !filter.removes(&e)))
                        .collect()
                };
                lines = kept.len();
                rewrite(path, kept.into_iter());
                bytes = file_size(path);
            }
        }
    }
}

fn append(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut bytes = Vec::with_capacity(line.len() + 1);
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    file.write_all(&bytes)
}

fn rewrite(path: &Path, lines: impl Iterator<Item = String>) {
    let mut bytes = Vec::new();
    for line in lines {
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
    }
    if let Err(e) = deltos_settings::write_atomic(path, &bytes) {
        tracing::warn!("could not write the query history: {e}");
    }
}

/// Starts the history thread and reads the file into `state` in the background. The
/// statements that run meanwhile come after the ones read.
pub fn start(state: &Entity<AppState>, cx: &mut App) {
    let Some(path) = state.read(cx).paths.as_ref().map(Paths::history_file) else {
        return;
    };
    start_at(path, state, cx);
}

fn start_at(path: PathBuf, state: &Entity<AppState>, cx: &mut App) {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("deltos-history".into())
        .spawn(move || serve(&path, rx));
    if let Err(e) = spawned {
        tracing::warn!("no query history: {e}");
        return;
    }
    let (reply, loaded) = oneshot::channel();
    let _ = tx.send(Op::Load(reply));
    cx.set_global(Writer {
        ops: tx,
        loaded: false,
        removed: Vec::new(),
    });
    let state = state.downgrade();
    cx.spawn(async move |cx| {
        let mut entries = loaded.await.unwrap_or_default();
        state
            .update(cx, |s, cx| {
                // What was removed while the file was read stays removed.
                if cx.has_global::<Writer>() {
                    let writer = cx.global_mut::<Writer>();
                    writer.loaded = true;
                    for filter in std::mem::take(&mut writer.removed) {
                        entries.retain(|e| !filter.removes(e));
                    }
                }
                entries.extend(s.history.drain(..));
                while entries.len() > LIMIT {
                    entries.pop_front();
                }
                s.history = entries;
                cx.emit(StateEvent::History);
            })
            .ok();
    })
    .detach();
}

/// Adds `entry` to the end of the file.
pub fn append_entry(entry: &HistoryEntry, cx: &App) {
    if let Some(w) = cx.try_global::<Writer>()
        && let Some(line) = to_line(entry)
    {
        let _ = w.ops.send(Op::Append(line));
    }
}

/// Takes what `filter` names out of the file, and out of what is still being read.
fn remove(filter: Filter, cx: &mut App) {
    if cx.has_global::<Writer>() {
        let w = cx.global_mut::<Writer>();
        if !w.loaded {
            w.removed.push(filter.clone());
        }
        let _ = w.ops.send(Op::Remove(filter));
    }
}

/// Takes one entry out of the history, found by when it ran and what it was.
pub fn delete(state: &Entity<AppState>, at: SystemTime, statement: &str, cx: &mut App) {
    let removed = state.update(cx, |s, cx| {
        let before = s.history.len();
        s.history
            .retain(|e| !(e.at == at && e.statement == statement));
        let removed = s.history.len() != before;
        if removed {
            cx.emit(StateEvent::History);
        }
        removed
    });
    if removed {
        let filter = Filter::One {
            at_ms: millis(at),
            statement: statement.to_owned(),
        };
        remove(filter, cx);
    }
}

/// Empties the history of one connection, or all of it.
pub fn clear(state: &Entity<AppState>, connection: Option<&str>, cx: &mut App) {
    state.update(cx, |s, cx| {
        match connection {
            Some(id) => s.history.retain(|e| e.connection != id),
            None => s.history.clear(),
        }
        cx.emit(StateEvent::History);
    });
    let filter = connection.map_or(Filter::All, |id| Filter::Connection(id.to_owned()));
    remove(filter, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(at_ms: u64, statement: &str) -> HistoryEntry {
        HistoryEntry {
            at: UNIX_EPOCH + Duration::from_millis(at_ms),
            connection: "local".into(),
            statement: statement.into(),
            duration: Duration::from_micros(1_234),
            rows: Some(7),
            error: None,
        }
    }

    #[test]
    fn entries_survive_the_file() {
        let mut e = entry(1_700_000_000_123, "select 'a\nb' -- é");
        e.error = Some("boom".into());
        let back = from_line(&to_line(&e).unwrap()).unwrap();
        assert_eq!(back.at, e.at);
        assert_eq!(back.statement, e.statement);
        assert_eq!(back.duration, e.duration);
        assert_eq!(back.rows, Some(7));
        assert_eq!(back.error.as_deref(), Some("boom"));
        assert!(!to_line(&e).unwrap().contains('\n'));
    }

    #[test]
    fn long_statements_and_errors_are_kept_out() {
        let long = "x".repeat(MAX_STATEMENT + 1);
        assert!(to_line(&entry(1, &long)).is_none());
        let mut e = entry(1, "select 1");
        e.error = Some("é".repeat(MAX_ERROR));
        let back = from_line(&to_line(&e).unwrap()).unwrap();
        assert!(back.error.unwrap().len() <= MAX_ERROR);
    }

    #[test]
    fn reading_keeps_the_last_entries_and_skips_damage() {
        let mut file = Vec::new();
        for i in 0..10 {
            file.extend_from_slice(
                to_line(&entry(i, &format!("select {i}")))
                    .unwrap()
                    .as_bytes(),
            );
            file.push(b'\n');
            if i == 4 {
                file.extend_from_slice(b"{not json\n\xff\xfe\n\n{\"statement\":\"\"}\n");
            }
        }
        // A line far too long, and a last line cut short by a crash.
        file.extend_from_slice(&vec![b'a'; MAX_LINE * 2]);
        file.push(b'\n');
        file.extend_from_slice(br#"{"at":11,"statement":"select 11""#);
        let (kept, count) = read_lines(&file[..], 4, MAX_BYTES);
        let statements: Vec<String> = kept
            .iter()
            .map(|l| from_line(l).unwrap().statement)
            .collect();
        assert_eq!(statements, ["select 6", "select 7", "select 8", "select 9"]);
        assert_eq!(count, 10 + 4 + 2);
        let (all, _) = read_lines(&file[..], 100, MAX_BYTES);
        assert_eq!(all.len(), 10);
    }

    #[test]
    fn a_last_line_without_newline_is_read() {
        let line = to_line(&entry(5, "select 5")).unwrap();
        let (kept, count) = read_lines(line.as_bytes(), 10, MAX_BYTES);
        assert_eq!(kept.len(), 1);
        assert_eq!(count, 1);
        assert_eq!(read_lines(&b""[..], 10, MAX_BYTES).1, 0);
    }

    #[test]
    fn the_file_is_cut_back_to_the_limit() {
        let dir = std::env::temp_dir().join(format!(
            "deltos-history-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let path = dir.join("history.jsonl");
        let (tx, rx) = mpsc::channel();
        let p = path.clone();
        let worker = std::thread::spawn(move || serve(&p, rx));
        let (reply, loaded) = oneshot::channel();
        tx.send(Op::Load(reply)).unwrap();
        for i in 0..(LIMIT + SLACK + 1) as u64 {
            tx.send(Op::Append(
                to_line(&entry(i, &format!("select {i}"))).unwrap(),
            ))
            .unwrap();
        }
        drop(tx);
        worker.join().unwrap();
        assert!(futures::executor::block_on(loaded).unwrap().is_empty());
        let (kept, count) = read_lines(std::fs::File::open(&path).unwrap(), usize::MAX, u64::MAX);
        assert_eq!(count, LIMIT);
        let last = from_line(kept.back().unwrap()).unwrap();
        assert_eq!(last.statement, format!("select {}", LIMIT + SLACK));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reading_keeps_within_the_byte_budget() {
        let mut file = Vec::new();
        for i in 0..10 {
            file.extend_from_slice(to_line(&entry(i, &"x".repeat(100))).unwrap().as_bytes());
            file.push(b'\n');
        }
        let line = to_line(&entry(0, &"x".repeat(100))).unwrap().len() as u64 + 1;
        let (kept, count) = read_lines(&file[..], 100, line * 3);
        assert_eq!(kept.len(), 3);
        assert_eq!(count, 10);
        assert_eq!(from_line(kept.back().unwrap()).unwrap().at, entry(9, "").at);
    }

    #[test]
    fn removing_rewrites_the_file_without_the_entries() {
        let dir = std::env::temp_dir().join(format!(
            "deltos-history-remove-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let path = dir.join("history.jsonl");
        let (tx, rx) = mpsc::channel();
        let p = path.clone();
        let worker = std::thread::spawn(move || serve(&p, rx));
        let mut other = entry(3, "select 3");
        other.connection = "other".into();
        for e in [entry(1, "select 1"), entry(2, "select 2"), other] {
            tx.send(Op::Append(to_line(&e).unwrap())).unwrap();
        }
        tx.send(Op::Remove(Filter::One {
            at_ms: 2,
            statement: "select 2".into(),
        }))
        .unwrap();
        tx.send(Op::Remove(Filter::Connection("other".into())))
            .unwrap();
        drop(tx);
        worker.join().unwrap();
        let (kept, _) = read_file(&path).unwrap();
        let left: Vec<String> = kept
            .iter()
            .map(|l| from_line(l).unwrap().statement)
            .collect();
        assert_eq!(left, ["select 1"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

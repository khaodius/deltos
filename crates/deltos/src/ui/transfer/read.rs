//! Streaming readers for the files an import takes: CSV (any delimiter), JSON (an array
//! of objects or one object per line) and SQL scripts. Each reads from a [`BufRead`] one
//! record, object or statement at a time and keeps only that much in memory, so a file of
//! any size imports in a fixed amount of memory. A single record past [`MAX_RECORD`]
//! bytes is an error rather than a reason to grow without end.
//!
//! Errors carry the line the record starts on, counted from 1.

use std::fmt;
use std::io::{BufRead, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use deltos_drivers::sql::{Dialect, split_statements};

/// The largest record, JSON object or statement a reader holds at once.
pub const MAX_RECORD: usize = 64 * 1024 * 1024;

/// The most fields a CSV record may have. Every field costs a `String` whatever its
/// length, so a line of nothing but delimiters would otherwise take many times its size.
pub const MAX_FIELDS: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadError {
    pub line: u64,
    pub message: String,
}

impl ReadError {
    fn new(line: u64, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Line {}: {}", self.line, self.message)
    }
}

fn io_error(line: u64, e: std::io::Error) -> ReadError {
    ReadError::new(line, format!("could not read the file: {e}"))
}

fn too_long(line: u64) -> ReadError {
    ReadError::new(
        line,
        format!(
            "a single record is larger than {} MB",
            MAX_RECORD / (1024 * 1024)
        ),
    )
}

/// Counts the bytes read through it, for a progress bar against the file's size.
#[derive(Debug)]
pub struct Counting<R> {
    inner: R,
    count: Arc<AtomicU64>,
}

impl<R> Counting<R> {
    pub fn new(inner: R, count: Arc<AtomicU64>) -> Self {
        Self { inner, count }
    }
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// Skips a UTF-8 byte order mark at the start of a file.
fn skip_bom<R: BufRead>(reader: &mut R) -> std::io::Result<()> {
    let buf = reader.fill_buf()?;
    if buf.starts_with(b"\xEF\xBB\xBF") {
        reader.consume(3);
    }
    Ok(())
}

/// One CSV record and the line it starts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub line: u64,
    pub fields: Vec<String>,
    /// Whether each field was in quotes. A quoted field is text even when it reads the
    /// same as the NULL text.
    pub quoted: Vec<bool>,
}

/// RFC 4180 CSV: fields separated by the delimiter, quoted fields with doubled quotes and
/// line breaks inside, records ended by CRLF, LF or CR. Lines with nothing on them are
/// skipped.
pub struct CsvReader<R> {
    reader: R,
    delimiter: u8,
    /// The line the reader is on, from 1.
    line: u64,
    started: bool,
    /// The last record ended with a CR, so an LF right after it belongs to that break.
    after_cr: bool,
}

impl<R: BufRead> CsvReader<R> {
    pub fn new(reader: R, delimiter: u8) -> Self {
        Self {
            reader,
            delimiter,
            line: 1,
            started: false,
            after_cr: false,
        }
    }

    pub fn next_record(&mut self) -> Result<Option<Record>, ReadError> {
        if !self.started {
            self.started = true;
            skip_bom(&mut self.reader).map_err(|e| io_error(1, e))?;
        }
        let mut fields: Vec<String> = Vec::new();
        let mut quoted_fields: Vec<bool> = Vec::new();
        let mut field: Vec<u8> = Vec::new();
        let mut size = 0usize;
        let mut start = self.line;
        // Inside quotes; just after a closing quote, where a quote doubles; whether the
        // field being read was quoted; and whether the record has anything yet.
        let mut quoted = false;
        let mut after_quote = false;
        let mut was_quoted = false;
        let mut any = false;
        loop {
            let buf = self.reader.fill_buf().map_err(|e| io_error(self.line, e))?;
            if buf.is_empty() {
                if quoted {
                    return Err(ReadError::new(start, "a quoted field never ends"));
                }
                if !any {
                    return Ok(None);
                }
                fields.push(into_text(field, start)?);
                quoted_fields.push(was_quoted);
                return Ok(Some(Record {
                    line: start,
                    fields,
                    quoted: quoted_fields,
                }));
            }
            let mut used = 0;
            let mut done = false;
            for &b in buf {
                used += 1;
                if std::mem::take(&mut self.after_cr) && b == b'\n' {
                    continue;
                }
                if quoted {
                    if b == b'"' {
                        quoted = false;
                        after_quote = true;
                    } else {
                        if b == b'\n' {
                            self.line += 1;
                        }
                        field.push(b);
                    }
                    continue;
                }
                match b {
                    b'"' if after_quote => {
                        field.push(b'"');
                        quoted = true;
                        after_quote = false;
                    }
                    b'"' if field.is_empty() && !was_quoted => {
                        quoted = true;
                        was_quoted = true;
                        any = true;
                    }
                    b'\n' | b'\r' => {
                        self.line += 1;
                        self.after_cr = b == b'\r';
                        if any {
                            done = true;
                            break;
                        }
                        start = self.line;
                    }
                    b if b == self.delimiter => {
                        if fields.len() >= MAX_FIELDS {
                            return Err(ReadError::new(
                                start,
                                format!("a record has more than {MAX_FIELDS} fields"),
                            ));
                        }
                        fields.push(into_text(std::mem::take(&mut field), start)?);
                        quoted_fields.push(was_quoted);
                        after_quote = false;
                        was_quoted = false;
                        any = true;
                    }
                    b => {
                        field.push(b);
                        after_quote = false;
                        any = true;
                    }
                }
            }
            self.reader.consume(used);
            size += used;
            if size > MAX_RECORD {
                return Err(too_long(start));
            }
            if done {
                fields.push(into_text(field, start)?);
                quoted_fields.push(was_quoted);
                return Ok(Some(Record {
                    line: start,
                    fields,
                    quoted: quoted_fields,
                }));
            }
        }
    }
}

fn into_text(bytes: Vec<u8>, line: u64) -> Result<String, ReadError> {
    String::from_utf8(bytes).map_err(|_| ReadError::new(line, "the text is not valid UTF-8"))
}

/// How a JSON file holds its objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonLayout {
    /// `[ {...}, {...} ]`
    Array,
    /// One object per line (JSON Lines, NDJSON).
    Lines,
}

/// Objects from a JSON array or a JSON Lines file, which it tells apart by the first
/// character. Each object is scanned to its end and parsed on its own, so only one is in
/// memory at a time.
pub struct JsonReader<R> {
    reader: R,
    layout: Option<JsonLayout>,
    line: u64,
    /// The array's closing bracket was read.
    ended: bool,
}

impl<R: BufRead> JsonReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            layout: None,
            line: 1,
            ended: false,
        }
    }

    /// Skips whitespace, counting lines, and returns the next byte without taking it.
    fn peek(&mut self) -> Result<Option<u8>, ReadError> {
        loop {
            let buf = self.reader.fill_buf().map_err(|e| io_error(self.line, e))?;
            let Some(&b) = buf.first() else {
                return Ok(None);
            };
            if !b.is_ascii_whitespace() {
                return Ok(Some(b));
            }
            if b == b'\n' {
                self.line += 1;
            }
            self.reader.consume(1);
        }
    }

    pub fn layout(&mut self) -> Result<Option<JsonLayout>, ReadError> {
        if self.layout.is_none() {
            skip_bom(&mut self.reader).map_err(|e| io_error(1, e))?;
            self.layout = match self.peek()? {
                None => None,
                Some(b'[') => {
                    self.reader.consume(1);
                    Some(JsonLayout::Array)
                }
                Some(b'{') => Some(JsonLayout::Lines),
                Some(_) => {
                    return Err(ReadError::new(
                        self.line,
                        "expected a JSON array of objects or one object per line",
                    ));
                }
            };
        }
        Ok(self.layout)
    }

    /// The next object and the line it starts on.
    pub fn next_object(
        &mut self,
    ) -> Result<Option<(u64, serde_json::Map<String, serde_json::Value>)>, ReadError> {
        let Some(layout) = self.layout()? else {
            return Ok(None);
        };
        if self.ended {
            return Ok(None);
        }
        match self.peek()? {
            None if layout == JsonLayout::Array => {
                return Err(ReadError::new(self.line, "the array is never closed"));
            }
            None => return Ok(None),
            Some(b']') if layout == JsonLayout::Array => {
                self.reader.consume(1);
                self.ended = true;
                return Ok(None);
            }
            Some(_) => {}
        }
        let start = self.line;
        let text = self.scan_value(start)?;
        let value: serde_json::Value = serde_json::from_slice(&text)
            .map_err(|e| ReadError::new(start, format!("invalid JSON: {e}")))?;
        let serde_json::Value::Object(object) = value else {
            return Err(ReadError::new(start, "expected an object"));
        };
        if layout == JsonLayout::Array {
            match self.peek()? {
                Some(b',') => self.reader.consume(1),
                Some(b']') => {
                    self.reader.consume(1);
                    self.ended = true;
                }
                Some(_) => {
                    return Err(ReadError::new(
                        self.line,
                        "expected a comma or the end of the array",
                    ));
                }
                None => return Err(ReadError::new(self.line, "the array is never closed")),
            }
        }
        Ok(Some((start, object)))
    }

    /// The bytes of one value, up to where it ends: the closing brace or bracket that
    /// balances the first, or the end of a bare value.
    fn scan_value(&mut self, start: u64) -> Result<Vec<u8>, ReadError> {
        let mut out = Vec::new();
        let mut depth = 0u32;
        let mut in_string = false;
        let mut escaped = false;
        loop {
            let buf = self.reader.fill_buf().map_err(|e| io_error(self.line, e))?;
            if buf.is_empty() {
                return if depth == 0 && !out.is_empty() {
                    Ok(out)
                } else {
                    Err(ReadError::new(start, "the object never ends"))
                };
            }
            let mut used = 0;
            let mut done = false;
            for &b in buf {
                if in_string {
                    used += 1;
                    out.push(b);
                    match b {
                        _ if escaped => escaped = false,
                        b'\\' => escaped = true,
                        b'"' => in_string = false,
                        b'\n' => self.line += 1,
                        _ => {}
                    }
                    continue;
                }
                match b {
                    b'"' => in_string = true,
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' if depth > 0 => depth -= 1,
                    b',' | b']' if depth == 0 => {
                        done = true;
                        break;
                    }
                    b'\n' if depth == 0 => {
                        done = true;
                        break;
                    }
                    b'\n' => self.line += 1,
                    _ => {}
                }
                used += 1;
                out.push(b);
                if depth == 0 && matches!(b, b'}' | b']') {
                    done = true;
                    break;
                }
            }
            self.reader.consume(used);
            if out.len() > MAX_RECORD {
                return Err(too_long(start));
            }
            if done {
                return Ok(out);
            }
        }
    }
}

/// One statement of a script and where it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub line: u64,
    pub text: String,
}

/// Statements of a SQL script, split by the drivers' splitter a chunk at a time. Every
/// statement but the last of a chunk ended at a terminator, so it is complete; the last
/// one may run on, and is carried into the next chunk. A MySQL `DELIMITER` in force is
/// carried along with it.
pub struct ScriptReader<R> {
    reader: R,
    dialect: Dialect,
    /// Text read but not yet split into complete statements.
    carry: String,
    /// Line number of the start of `carry`.
    carry_line: u64,
    /// Bytes of `carry` that came from a `DELIMITER` line put back in front of it.
    prefix: usize,
    ready: std::collections::VecDeque<Statement>,
    eof: bool,
    /// Bytes per read.
    chunk: usize,
}

impl<R: BufRead> ScriptReader<R> {
    pub fn new(reader: R, dialect: Dialect) -> Self {
        Self {
            reader,
            dialect,
            carry: String::new(),
            carry_line: 1,
            prefix: 0,
            ready: Default::default(),
            eof: false,
            chunk: 1 << 20,
        }
    }

    #[cfg(test)]
    fn with_chunk(mut self, chunk: usize) -> Self {
        self.chunk = chunk.max(1);
        self
    }

    pub fn next_statement(&mut self) -> Result<Option<Statement>, ReadError> {
        loop {
            if let Some(s) = self.ready.pop_front() {
                return Ok(Some(s));
            }
            if self.eof && self.carry.trim().is_empty() {
                return Ok(None);
            }
            if !self.eof {
                self.fill()?;
            }
            self.split();
        }
    }

    /// Reads one more chunk onto the carried text, up to the end of a line.
    fn fill(&mut self) -> Result<(), ReadError> {
        let mut bytes = Vec::with_capacity(self.chunk);
        (&mut self.reader)
            .take(self.chunk as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| io_error(self.carry_line, e))?;
        if bytes.is_empty() {
            self.eof = true;
            return Ok(());
        }
        if self.carry.is_empty() && self.carry_line == 1 && bytes.starts_with(b"\xEF\xBB\xBF") {
            bytes.drain(..3);
        }
        // Read on to the end of the line, so a chunk never ends inside a character or
        // a MySQL `DELIMITER` line.
        if bytes.last() != Some(&b'\n') {
            (&mut self.reader)
                .take(MAX_RECORD as u64)
                .read_until(b'\n', &mut bytes)
                .map_err(|e| io_error(self.carry_line, e))?;
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| ReadError::new(self.carry_line, "the script is not valid UTF-8"))?;
        self.carry.push_str(&text);
        if self.carry.len() > MAX_RECORD {
            return Err(too_long(self.carry_line));
        }
        Ok(())
    }

    fn split(&mut self) {
        let ranges = split_statements(&self.carry, self.dialect);
        // Before the end of the file the last statement may still run on.
        let complete = if self.eof {
            ranges.len()
        } else {
            ranges.len().saturating_sub(1)
        };
        let lines_in = |text: &str| text.bytes().filter(|b| *b == b'\n').count() as u64;
        let mut line = self.carry_line;
        let mut counted = self.prefix;
        for range in ranges.iter().take(complete) {
            let start = range.start.max(counted);
            line += lines_in(&self.carry[counted..start]);
            counted = start;
            self.ready.push_back(Statement {
                line,
                text: self.carry[range.clone()].to_string(),
            });
        }
        if self.eof {
            self.carry.clear();
            self.prefix = 0;
            return;
        }
        // Keep from the start of the statement still open. With none, keep everything:
        // the text may end inside a comment that the next chunk closes.
        let keep = ranges
            .get(complete)
            .map_or(self.prefix, |r| r.start)
            .max(counted);
        line += lines_in(&self.carry[counted..keep]);
        let delimiter = if self.dialect == Dialect::Mysql {
            last_delimiter(&self.carry[..keep])
        } else {
            None
        };
        let rest = self.carry.split_off(keep);
        self.carry = match delimiter {
            Some(d) if d != ";" => {
                let head = format!("DELIMITER {d}\n");
                self.prefix = head.len();
                head + &rest
            }
            _ => {
                self.prefix = 0;
                rest
            }
        };
        self.carry_line = line;
    }
}

/// The delimiter the last MySQL `DELIMITER` line in `text` set, if any.
fn last_delimiter(text: &str) -> Option<String> {
    text.lines().rev().find_map(|line| {
        let trimmed = line.trim();
        let (word, rest) = trimmed.split_at(trimmed.find(char::is_whitespace)?);
        word.eq_ignore_ascii_case("DELIMITER")
            .then(|| rest.trim().to_string())
            .filter(|d| !d.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(text: &str, delimiter: u8) -> Result<Vec<Record>, ReadError> {
        records_of(text.as_bytes(), delimiter)
    }

    fn records_of(bytes: &[u8], delimiter: u8) -> Result<Vec<Record>, ReadError> {
        // A tiny buffer so records cross buffer edges.
        let reader = std::io::BufReader::with_capacity(3, bytes);
        let mut csv = CsvReader::new(reader, delimiter);
        let mut out = Vec::new();
        while let Some(r) = csv.next_record()? {
            out.push(r);
        }
        Ok(out)
    }

    fn fields(records: &[Record]) -> Vec<Vec<&str>> {
        records
            .iter()
            .map(|r| r.fields.iter().map(String::as_str).collect())
            .collect()
    }

    #[test]
    fn reads_rfc_4180() {
        let text =
            "\u{feff}id,name,note\r\n1,\"O\"\"Brien\",\"two\r\nlines\"\r\n\r\n2,,\"a,b\"\n3,x,y";
        let out = records(text, b',').unwrap();
        assert_eq!(
            fields(&out),
            [
                vec!["id", "name", "note"],
                vec!["1", "O\"Brien", "two\r\nlines"],
                vec!["2", "", "a,b"],
                vec!["3", "x", "y"],
            ]
        );
        assert_eq!(
            out.iter().map(|r| r.line).collect::<Vec<_>>(),
            [1, 2, 5, 6],
            "lines count the break inside the quoted field and the blank line"
        );
        let out = records("a\tb\rc\td\r", b'\t').unwrap();
        assert_eq!(fields(&out), [vec!["a", "b"], vec!["c", "d"]]);
        assert_eq!(fields(&records("x,\n", b',').unwrap()), [vec!["x", ""]]);
    }

    #[test]
    fn csv_errors_name_the_line() {
        let e = records("a\n\"open,b\nc", b',').unwrap_err();
        assert_eq!(e.line, 2);
        let e = records_of(b"a\n\xff\n", b',').unwrap_err();
        assert_eq!(e.line, 2);
        assert!(records("", b',').unwrap().is_empty());
        // A line of delimiters is refused before it costs a String per byte.
        let e = records(&",".repeat(MAX_FIELDS + 5), b',').unwrap_err();
        assert!(e.message.contains("fields"), "{e}");
        assert_eq!(records(&",".repeat(MAX_FIELDS - 1), b',').unwrap().len(), 1);
    }

    #[test]
    fn csv_tells_quoted_fields_apart() {
        let out = records(",\"\",\"NULL\",NULL,\"a\"b\n", b',').unwrap();
        assert_eq!(fields(&out), [vec!["", "", "NULL", "NULL", "ab"]]);
        assert_eq!(out[0].quoted, [false, true, true, false, true]);
    }

    fn objects(text: &str) -> Result<Vec<(u64, String)>, ReadError> {
        let reader = std::io::BufReader::with_capacity(4, text.as_bytes());
        let mut json = JsonReader::new(reader);
        let mut out = Vec::new();
        while let Some((line, o)) = json.next_object()? {
            out.push((line, serde_json::Value::Object(o).to_string()));
        }
        Ok(out)
    }

    #[test]
    fn reads_json_arrays_and_lines() {
        let array = "[\n  {\"a\": 1, \"b\": \"x, ]}\"},\n  {\"a\": [2, {\"c\": null}]}\n]\n";
        assert_eq!(
            objects(array).unwrap(),
            [
                (2, "{\"a\":1,\"b\":\"x, ]}\"}".to_string()),
                (3, "{\"a\":[2,{\"c\":null}]}".to_string())
            ]
        );
        assert_eq!(objects("[]").unwrap(), []);
        let lines = "{\"a\":1}\n\n{\"a\":\"\\\"q\"}\r\n{\"a\":3}";
        assert_eq!(
            objects(lines).unwrap(),
            [
                (1, "{\"a\":1}".to_string()),
                (3, "{\"a\":\"\\\"q\"}".to_string()),
                (4, "{\"a\":3}".to_string())
            ]
        );
        assert_eq!(objects("[{\"a\":1},\n 5]").unwrap_err().line, 2);
        assert!(objects("[{\"a\":1}").is_err());
        assert!(objects("{\"a\":").is_err());
        assert!(objects("\"text\"").is_err());
    }

    fn statements(text: &str, dialect: Dialect, chunk: usize) -> Vec<(u64, String)> {
        let mut reader = ScriptReader::new(text.as_bytes(), dialect).with_chunk(chunk);
        let mut out = Vec::new();
        while let Some(s) = reader.next_statement().unwrap() {
            out.push((s.line, s.text));
        }
        out
    }

    #[test]
    fn splits_scripts_across_chunks() {
        let script = "CREATE TABLE t (a text);\n-- seed\nINSERT INTO t VALUES ('x;y');\n\nINSERT INTO t\nVALUES ('é');\nSELECT 1";
        let expected = vec![
            (1, "CREATE TABLE t (a text)".to_string()),
            (2, "INSERT INTO t VALUES ('x;y')".to_string()),
            (5, "INSERT INTO t\nVALUES ('é')".to_string()),
            (7, "SELECT 1".to_string()),
        ];
        for chunk in [1, 2, 5, 7, 16, 1000] {
            let got = statements(script, Dialect::Postgres, chunk);
            let got: Vec<(u64, String)> = got
                .into_iter()
                .map(|(l, s)| (l, s.trim_start_matches("-- seed\n").to_string()))
                .collect();
            assert_eq!(got, expected, "chunk of {chunk}");
        }
    }

    #[test]
    fn keeps_a_mysql_delimiter_across_chunks() {
        let script = "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END//\nDELIMITER ;\nSELECT 3;\n";
        for chunk in [4, 9, 30, 1000] {
            let got = statements(script, Dialect::Mysql, chunk);
            assert_eq!(
                got,
                [
                    (
                        2,
                        "CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END".to_string()
                    ),
                    (4, "SELECT 3".to_string())
                ],
                "chunk of {chunk}"
            );
        }
        assert_eq!(last_delimiter("x\ndelimiter //\ny"), Some("//".into()));
    }
}

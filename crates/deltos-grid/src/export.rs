//! Streaming export straight from a [`PageLoader`], one page in memory at a time, and
//! the row writers behind it: CSV (or any delimiter), JSON (an array or one object per
//! line) and Markdown. The app adds its own writers, such as SQL `INSERT` statements,
//! through the [`RowWriter`] trait, and copies a selection with the same writers through
//! [`write_rows`], so a format has one implementation whether it goes to a file or to
//! the clipboard.
//!
//! CSV follows RFC 4180: fields separated by the delimiter, records ended by CRLF (LF
//! for the clipboard), and a field quoted with double quotes when it holds the
//! delimiter, a quote, a carriage return or a line feed, with quotes inside doubled.
//! Null is the NULL text (empty by default) and never quoted, while text that reads the
//! same as the NULL text always is; booleans are `true` and `false`, bytes are `0x`
//! followed by hex.
//!
//! The columns are the first page's, or [`ExportOptions::columns`] when given. A document
//! collection's pages can bring fields no earlier page had; the writer hears of them
//! through [`RowWriter::columns_grew`]. CSV and Markdown cannot add a column to a header
//! already written and leave them out; JSON and SQL take them.
//!
//! The export future writes synchronously, so it belongs on a thread that may block,
//! such as the blocking hook or a dedicated thread.

use std::io::Write;
use std::sync::Arc;

use deltos_data::{Column, Error, Result, Value};

use crate::columns::ColumnSet;
use crate::loader::{PageLoader, PageRequest};

/// Turns rows into bytes in one format. Calls come in order: `begin` once with the
/// columns, `row` per row (with a value per column known so far), `columns_grew`
/// whenever later rows bring more columns, and `finish` once at the end.
pub trait RowWriter: Send {
    fn begin(&mut self, columns: &[Column], out: &mut Vec<u8>);

    fn row(&mut self, row: &[Value], out: &mut Vec<u8>);

    /// The column list grew at the end; `columns` is the whole new list.
    fn columns_grew(&mut self, columns: &[Column], out: &mut Vec<u8>) {
        let _ = (columns, out);
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        let _ = out;
    }
}

/// When a CSV field is put in quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QuoteStyle {
    /// Only when the field holds the delimiter, a quote or a line break.
    #[default]
    Necessary,
    /// Every field, NULL excepted.
    Always,
    /// Every field but numbers, booleans and NULL.
    NonNumeric,
}

/// How a [`CsvWriter`] writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvFormat {
    pub delimiter: u8,
    pub header: bool,
    pub quote: QuoteStyle,
    /// What a NULL is written as.
    pub null_text: String,
    /// `\r\n` as RFC 4180 has it, or `\n`.
    pub line_end: &'static str,
}

impl Default for CsvFormat {
    fn default() -> Self {
        Self {
            delimiter: b',',
            header: true,
            quote: QuoteStyle::Necessary,
            null_text: String::new(),
            line_end: "\r\n",
        }
    }
}

/// CSV, TSV or any other delimiter.
#[derive(Debug, Clone)]
pub struct CsvWriter {
    format: CsvFormat,
    /// Which columns are numeric, for [`QuoteStyle::NonNumeric`].
    numeric: Vec<bool>,
}

impl CsvWriter {
    pub fn new(format: CsvFormat) -> Self {
        Self {
            format,
            numeric: Vec::new(),
        }
    }

    /// Writes `text` as one field, quoted when the style asks for it or the text needs it.
    /// Text that reads the same as the NULL text is always quoted, so a reader can tell
    /// the two apart: with the default empty NULL text, an empty string is `""`.
    fn field(&self, out: &mut Vec<u8>, text: &str, numeric: bool) {
        let force = match self.format.quote {
            QuoteStyle::Always => true,
            QuoteStyle::NonNumeric => !numeric,
            QuoteStyle::Necessary => false,
        } || text == self.format.null_text;
        push_field(out, text, self.format.delimiter, force);
    }

    /// A number or boolean, quoted only in [`QuoteStyle::Always`].
    fn bare(&self, out: &mut Vec<u8>, write: impl FnOnce(&mut Vec<u8>)) {
        let quote = self.format.quote == QuoteStyle::Always;
        if quote {
            out.push(b'"');
        }
        write(out);
        if quote {
            out.push(b'"');
        }
    }
}

fn push_field(out: &mut Vec<u8>, text: &str, delimiter: u8, force: bool) {
    let quote = force
        || text
            .bytes()
            .any(|b| b == delimiter || b == b'"' || b == b'\n' || b == b'\r');
    if !quote {
        out.extend_from_slice(text.as_bytes());
        return;
    }
    out.push(b'"');
    for b in text.bytes() {
        if b == b'"' {
            out.push(b'"');
        }
        out.push(b);
    }
    out.push(b'"');
}

impl RowWriter for CsvWriter {
    fn begin(&mut self, columns: &[Column], out: &mut Vec<u8>) {
        self.numeric = columns.iter().map(|c| c.kind.is_numeric()).collect();
        if self.format.header {
            for (i, column) in columns.iter().enumerate() {
                if i > 0 {
                    out.push(self.format.delimiter);
                }
                self.field(out, &column.name, false);
            }
            out.extend_from_slice(self.format.line_end.as_bytes());
        }
    }

    fn row(&mut self, row: &[Value], out: &mut Vec<u8>) {
        for (i, value) in row.iter().take(self.numeric.len()).enumerate() {
            if i > 0 {
                out.push(self.format.delimiter);
            }
            let numeric = self.numeric.get(i).copied().unwrap_or(false);
            match value {
                // NULL is never quoted by the style, so it stays apart from the same text.
                Value::Null => {
                    push_field(out, &self.format.null_text, self.format.delimiter, false)
                }
                Value::Bool(b) => self.bare(out, |out| {
                    out.extend_from_slice(if *b { b"true" } else { b"false" })
                }),
                Value::Int(n) => self.bare(out, |out| {
                    let _ = write!(out, "{n}");
                }),
                Value::Float(f) => self.bare(out, |out| {
                    let _ = write!(out, "{f}");
                }),
                Value::Text(s) | Value::Json(s) => self.field(out, s, numeric),
                Value::Bytes(bytes) => self.bare(out, |out| {
                    out.extend_from_slice(b"0x");
                    push_hex(out, bytes);
                }),
            }
        }
        out.extend_from_slice(self.format.line_end.as_bytes());
    }
}

fn push_hex(out: &mut Vec<u8>, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for b in bytes {
        out.push(HEX[usize::from(b >> 4)]);
        out.push(HEX[usize::from(b & 15)]);
    }
}

/// How a [`JsonWriter`] lays rows out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JsonStyle {
    /// One array, an object per row.
    #[default]
    Array,
    /// JSON Lines: one object per line and nothing around them.
    Lines,
}

/// Rows as JSON objects keyed by column name, in column order. Nested documents and JSON
/// columns are written as JSON, not as strings.
#[derive(Debug, Clone)]
pub struct JsonWriter {
    style: JsonStyle,
    /// Objects spread over lines and indented, as people read them.
    pretty: bool,
    /// Each column name as a JSON string, ready to write.
    keys: Vec<String>,
    rows: u64,
}

impl JsonWriter {
    pub fn new(style: JsonStyle, pretty: bool) -> Self {
        Self {
            style,
            pretty,
            keys: Vec::new(),
            rows: 0,
        }
    }

    fn set_keys(&mut self, columns: &[Column]) {
        self.keys = columns
            .iter()
            .map(|c| serde_json::to_string(&c.name).unwrap_or_else(|_| "\"\"".into()))
            .collect();
    }
}

/// A cell as JSON. Text that is valid JSON in a JSON column is kept as JSON.
pub fn json_value(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or_else(
            || serde_json::Value::String(f.to_string()),
            serde_json::Value::Number,
        ),
        Value::Json(s) => serde_json::from_str(s).unwrap_or_else(|_| s.to_string().into()),
        Value::Text(s) => serde_json::Value::String(s.to_string()),
        Value::Bytes(_) => serde_json::Value::String(value.to_string()),
    }
}

/// One value of a compact row. A JSON column's text goes out as the database gave it,
/// once it is known to be valid JSON, so its numbers keep every digit and its keys their
/// order; going through `serde_json::Value` would round big numbers to a float and sort
/// the keys. Line breaks between its tokens become spaces, since JSON Lines needs one
/// line per row and a valid document has no raw line break inside a string.
fn push_compact(out: &mut Vec<u8>, value: &Value) {
    if let Value::Json(original) = value {
        let text = original.trim();
        if serde_json::from_str::<serde_json::Value>(text).is_ok() {
            out.extend(
                text.bytes()
                    .map(|b| if b == b'\n' || b == b'\r' { b' ' } else { b }),
            );
        } else {
            let _ = serde_json::to_writer(&mut *out, &**original);
        }
        return;
    }
    let _ = serde_json::to_writer(&mut *out, &json_value(value));
}

impl RowWriter for JsonWriter {
    fn begin(&mut self, columns: &[Column], out: &mut Vec<u8>) {
        self.set_keys(columns);
        if self.style == JsonStyle::Array {
            out.push(b'[');
        }
    }

    fn columns_grew(&mut self, columns: &[Column], _out: &mut Vec<u8>) {
        self.set_keys(columns);
    }

    fn row(&mut self, row: &[Value], out: &mut Vec<u8>) {
        // Inside an array the objects sit one level in.
        let indent = if self.style == JsonStyle::Array && self.pretty {
            "  "
        } else {
            ""
        };
        if self.style == JsonStyle::Array {
            out.extend_from_slice(if self.rows == 0 { b"\n" } else { b",\n" });
        }
        out.extend_from_slice(indent.as_bytes());
        out.push(b'{');
        for (i, (key, value)) in self.keys.iter().zip(row).enumerate() {
            if i > 0 {
                out.push(b',');
            }
            if self.pretty {
                out.push(b'\n');
                out.extend_from_slice(indent.as_bytes());
                out.extend_from_slice(b"  ");
            }
            out.extend_from_slice(key.as_bytes());
            out.extend_from_slice(if self.pretty { b": " } else { b":" });
            if self.pretty {
                let text = serde_json::to_string_pretty(&json_value(value)).unwrap_or_default();
                // Nested lines line up under the key.
                let mut lines = text.split('\n');
                if let Some(first) = lines.next() {
                    out.extend_from_slice(first.as_bytes());
                }
                for line in lines {
                    out.push(b'\n');
                    out.extend_from_slice(indent.as_bytes());
                    out.extend_from_slice(b"  ");
                    out.extend_from_slice(line.as_bytes());
                }
            } else {
                push_compact(out, value);
            }
        }
        if self.pretty && !self.keys.is_empty() && !row.is_empty() {
            out.push(b'\n');
            out.extend_from_slice(indent.as_bytes());
        }
        out.push(b'}');
        if self.style == JsonStyle::Lines {
            out.push(b'\n');
        }
        self.rows += 1;
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        if self.style == JsonStyle::Array {
            out.extend_from_slice(if self.rows == 0 { b"]\n" } else { b"\n]\n" });
        }
    }
}

/// A Markdown table: a header row, the divider, a row per row. Pipes are escaped and line
/// breaks become spaces, so every row stays on one line.
#[derive(Debug, Clone, Default)]
pub struct MarkdownWriter {
    width: usize,
}

impl MarkdownWriter {
    pub fn new() -> Self {
        Self::default()
    }
}

fn markdown_cell(out: &mut Vec<u8>, text: &str) {
    for c in text.chars() {
        match c {
            '|' => out.extend_from_slice(b"\\|"),
            '\n' | '\r' => out.push(b' '),
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
}

impl RowWriter for MarkdownWriter {
    fn begin(&mut self, columns: &[Column], out: &mut Vec<u8>) {
        self.width = columns.len();
        out.extend_from_slice(b"| ");
        for (i, column) in columns.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(b" | ");
            }
            markdown_cell(out, &column.name);
        }
        out.extend_from_slice(b" |\n|");
        for i in 0..columns.len() {
            if i > 0 {
                out.push(b'|');
            }
            out.extend_from_slice(b" --- ");
        }
        out.extend_from_slice(b"|\n");
    }

    fn row(&mut self, row: &[Value], out: &mut Vec<u8>) {
        out.extend_from_slice(b"| ");
        for (i, value) in row.iter().take(self.width).enumerate() {
            if i > 0 {
                out.extend_from_slice(b" | ");
            }
            markdown_cell(out, &value.to_string());
        }
        out.extend_from_slice(b" |\n");
    }
}

/// Rows already in memory through `writer`, for the clipboard. Trailing line breaks are
/// trimmed.
pub fn write_rows(writer: &mut dyn RowWriter, columns: &[Column], rows: &[Vec<Value>]) -> String {
    let mut out = Vec::new();
    writer.begin(columns, &mut out);
    for row in rows {
        writer.row(row, &mut out);
    }
    writer.finish(&mut out);
    while matches!(out.last(), Some(b'\n' | b'\r')) {
        out.pop();
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// Rows asked for per load.
    pub page_rows: u32,
    /// The columns to start with, in order. `None` takes the first page's.
    pub columns: Option<Arc<[Column]>>,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            page_rows: 1000,
            columns: None,
        }
    }
}

/// Writes every row the loader has through `writer` and returns how many. `progress` is
/// called after each page with the running total; returning `false` stops the export
/// with [`Error::Cancelled`]. At most one page of rows and its output are in memory.
pub async fn export<W: Write>(
    loader: &dyn PageLoader,
    writer: &mut dyn RowWriter,
    options: &ExportOptions,
    mut out: W,
    mut progress: impl FnMut(u64) -> bool,
) -> Result<u64> {
    let page_rows = options.page_rows.max(1);
    let mut set = ColumnSet::default();
    let mut width = 0usize;
    let mut started = false;
    let mut written = 0u64;
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut page = 0u64;
    loop {
        let loaded = loader.load(PageRequest::new(page, page_rows)).await?;
        if !started {
            started = true;
            let header = options
                .columns
                .clone()
                .unwrap_or_else(|| loaded.columns.clone());
            set.merge(&header);
            width = set.len();
            writer.begin(&header, &mut buf);
        }
        let map = set.merge(&loaded.columns);
        if set.len() > width {
            width = set.len();
            if let Some(columns) = set.shared() {
                writer.columns_grew(&columns, &mut buf);
            }
        }
        let count = loaded.rows.len();
        for row in loaded.rows {
            let row = map.align(row, width);
            writer.row(&row, &mut buf);
            written += 1;
        }
        out.write_all(&buf).map_err(write_error)?;
        buf.clear();
        if !progress(written) {
            return Err(Error::Cancelled);
        }
        if loaded.end || count == 0 {
            break;
        }
        page += 1;
    }
    writer.finish(&mut buf);
    out.write_all(&buf).map_err(write_error)?;
    out.flush().map_err(write_error)?;
    Ok(written)
}

#[derive(Debug, Clone)]
pub struct CsvOptions {
    /// Rows asked for per load.
    pub page_rows: u32,
    pub header: bool,
    pub delimiter: u8,
    /// The columns to write, in order. `None` takes the first page's.
    pub columns: Option<Arc<[Column]>>,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            page_rows: 1000,
            header: true,
            delimiter: b',',
            columns: None,
        }
    }
}

/// [`export`] as RFC 4180 CSV.
pub async fn export_csv<W: Write>(
    loader: &dyn PageLoader,
    options: &CsvOptions,
    out: W,
    progress: impl FnMut(u64) -> bool,
) -> Result<u64> {
    let mut writer = CsvWriter::new(CsvFormat {
        delimiter: options.delimiter,
        header: options.header,
        ..CsvFormat::default()
    });
    let export_options = ExportOptions {
        page_rows: options.page_rows,
        columns: options.columns.clone(),
    };
    export(loader, &mut writer, &export_options, out, progress).await
}

fn write_error(e: std::io::Error) -> Error {
    Error::Other(format!("could not write the export: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::{LoadedPage, RowCount};
    use crate::testing::{FakeLoader, fake_columns};
    use deltos_data::ValueKind;
    use futures::executor::block_on;
    use futures::future::BoxFuture;

    #[test]
    fn exports_every_row_in_pages() {
        let loader = FakeLoader::new(2345);
        let mut out = Vec::new();
        let mut calls = Vec::new();
        let written = block_on(export_csv(&loader, &CsvOptions::default(), &mut out, |n| {
            calls.push(n);
            true
        }))
        .unwrap();
        assert_eq!(written, 2345);
        assert_eq!(calls, [1000, 2000, 2345]);
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.split("\r\n").collect();
        assert_eq!(lines.len(), 2347, "header, rows and the empty tail");
        assert_eq!(lines[0], "id,name,score,even");
        assert_eq!(lines[1], "0,row-0,0,true");
        assert_eq!(lines[2345], "2344,row-2344,586,true");
        assert_eq!(lines[2346], "");
        assert_eq!(loader.requested_pages(), [0, 1, 2]);
    }

    #[test]
    fn cancel_stops_after_the_current_page() {
        let loader = FakeLoader::new(5000);
        let mut out = Vec::new();
        let result = block_on(export_csv(&loader, &CsvOptions::default(), &mut out, |n| {
            n < 2000
        }));
        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(loader.requested_pages(), [0, 1]);
        assert_eq!(out.iter().filter(|b| **b == b'\n').count(), 2001);
    }

    struct Static(Vec<LoadedPage>);

    impl PageLoader for Static {
        fn random_access(&self) -> bool {
            true
        }
        fn load(&self, request: PageRequest) -> BoxFuture<'static, Result<LoadedPage>> {
            let page = self
                .0
                .get(request.page as usize)
                .cloned()
                .unwrap_or(LoadedPage {
                    columns: Arc::from(Vec::new()),
                    rows: Vec::new(),
                    end: true,
                });
            Box::pin(futures::future::ready(Ok(page)))
        }
        fn count(&self) -> BoxFuture<'static, Result<RowCount>> {
            Box::pin(futures::future::ready(Ok(RowCount::Unknown)))
        }
    }

    /// Two pages of a document collection: the second brings a new field.
    fn growing() -> Static {
        let first: Arc<[Column]> = vec![
            Column::new("a,b", "text", ValueKind::Text),
            Column::new("v", "text", ValueKind::Text),
        ]
        .into();
        let second: Arc<[Column]> = vec![
            Column::new("v", "text", ValueKind::Text),
            Column::new("new", "text", ValueKind::Text),
            Column::new("a,b", "text", ValueKind::Text),
        ]
        .into();
        Static(vec![
            LoadedPage {
                columns: first,
                rows: vec![
                    vec![Value::text("plain"), Value::text("say \"hi\", now")].into(),
                    vec![Value::text("line\nbreak"), Value::Null].into(),
                    vec![Value::Bytes(vec![0, 255].into()), Value::Float(2.5)].into(),
                    vec![Value::Bool(false), Value::Json("{\"k\":1}".into())].into(),
                ]
                .into_iter()
                .collect(),
                end: false,
            },
            LoadedPage {
                columns: second,
                rows: vec![vec![Value::Int(1), Value::text("kept"), Value::Int(2)].into()],
                end: true,
            },
        ])
    }

    #[test]
    fn quotes_what_rfc_4180_needs_and_aligns_columns() {
        let mut out = Vec::new();
        block_on(export_csv(
            &growing(),
            &CsvOptions::default(),
            &mut out,
            |_| true,
        ))
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "\"a,b\",v\r\n\
             plain,\"say \"\"hi\"\", now\"\r\n\
             \"line\nbreak\",\r\n\
             0x00ff,2.5\r\n\
             false,\"{\"\"k\"\":1}\"\r\n\
             2,1\r\n"
        );

        // A caller-provided header, with a different delimiter and no header line.
        let mut out = Vec::new();
        let options = CsvOptions {
            header: false,
            delimiter: b';',
            columns: Some(fake_columns(2).into()),
            ..CsvOptions::default()
        };
        block_on(export_csv(&FakeLoader::new(2), &options, &mut out, |_| {
            true
        }))
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "0;row-0\r\n1;row-1\r\n");
    }

    #[test]
    fn csv_quote_styles_and_null_text() {
        let columns = vec![
            Column::new("n", "int", ValueKind::Int),
            Column::new("price", "numeric", ValueKind::Decimal),
            Column::new("name", "text", ValueKind::Text),
        ];
        let rows = vec![
            vec![Value::Int(1), Value::text("2.50"), Value::text("a")],
            vec![Value::Null, Value::Null, Value::text("NULL")],
        ];
        let csv = |quote, null: &str| {
            write_rows(
                &mut CsvWriter::new(CsvFormat {
                    quote,
                    null_text: null.into(),
                    line_end: "\n",
                    ..CsvFormat::default()
                }),
                &columns,
                &rows,
            )
        };
        assert_eq!(
            csv(QuoteStyle::Necessary, ""),
            "n,price,name\n1,2.50,a\n,,NULL"
        );
        assert_eq!(
            csv(QuoteStyle::Always, "\\N"),
            "\"n\",\"price\",\"name\"\n\"1\",\"2.50\",\"a\"\n\\N,\\N,\"NULL\"",
            "NULL is never quoted"
        );
        assert_eq!(
            csv(QuoteStyle::NonNumeric, "NULL"),
            "\"n\",\"price\",\"name\"\n1,2.50,\"a\"\nNULL,NULL,\"NULL\""
        );
        // Text that reads as the NULL text is quoted in every style, so the two stay
        // apart: an empty string is `""` when NULL is written as nothing.
        assert_eq!(
            csv(QuoteStyle::Necessary, "NULL"),
            "n,price,name\n1,2.50,a\nNULL,NULL,\"NULL\""
        );
        let empty = write_rows(
            &mut CsvWriter::new(CsvFormat {
                header: false,
                line_end: "\n",
                ..CsvFormat::default()
            }),
            &columns,
            &[vec![Value::Null, Value::text(""), Value::text("")]],
        );
        assert_eq!(empty, ",\"\",\"\"");
        let tsv = write_rows(
            &mut CsvWriter::new(CsvFormat {
                delimiter: b'\t',
                header: false,
                line_end: "\n",
                ..CsvFormat::default()
            }),
            &columns,
            &[vec![Value::Int(1), Value::text("a\tb"), Value::text("c,d")]],
        );
        assert_eq!(tsv, "1\t\"a\tb\"\tc,d");
    }

    #[test]
    fn json_arrays_lines_and_pretty() {
        let columns = vec![
            Column::new("id", "int", ValueKind::Int),
            Column::new("doc", "jsonb", ValueKind::Json),
            Column::new("note", "text", ValueKind::Text),
        ];
        let rows = vec![
            vec![
                Value::Int(1),
                Value::Json("{\"k\": [1, 2]}".into()),
                Value::text("say \"hi\"\n"),
            ],
            vec![Value::Int(2), Value::Null, Value::Float(f64::NAN)],
        ];
        assert_eq!(
            write_rows(
                &mut JsonWriter::new(JsonStyle::Lines, false),
                &columns,
                &rows
            ),
            "{\"id\":1,\"doc\":{\"k\": [1, 2]},\"note\":\"say \\\"hi\\\"\\n\"}\n\
             {\"id\":2,\"doc\":null,\"note\":\"NaN\"}"
        );
        assert_eq!(
            write_rows(
                &mut JsonWriter::new(JsonStyle::Array, false),
                &columns,
                &rows[1..]
            ),
            "[\n{\"id\":2,\"doc\":null,\"note\":\"NaN\"}\n]"
        );
        assert_eq!(
            write_rows(&mut JsonWriter::new(JsonStyle::Array, false), &columns, &[]),
            "[]"
        );
        // Pretty output is laid out as serde_json lays it out, keys in column order.
        assert_eq!(
            write_rows(
                &mut JsonWriter::new(JsonStyle::Array, true),
                &columns,
                &rows
            ),
            "[\n  {\n    \"id\": 1,\n    \"doc\": {\n      \"k\": [\n        1,\n        2\n      ]\n    },\n    \"note\": \"say \\\"hi\\\"\\n\"\n  },\n  {\n    \"id\": 2,\n    \"doc\": null,\n    \"note\": \"NaN\"\n  }\n]"
        );
        assert_eq!(
            write_rows(
                &mut JsonWriter::new(JsonStyle::Lines, true),
                &columns,
                &rows[1..]
            ),
            "{\n  \"id\": 2,\n  \"doc\": null,\n  \"note\": \"NaN\"\n}"
        );
    }

    #[test]
    fn json_columns_keep_their_digits_and_key_order() {
        let columns = vec![Column::new("doc", "jsonb", ValueKind::Json)];
        let rows = vec![
            vec![Value::Json(
                "{\"z\": 12345678901234567890123,\n \"a\": 0.10000000000000000001}".into(),
            )],
            vec![Value::Json("not json".into())],
        ];
        assert_eq!(
            write_rows(
                &mut JsonWriter::new(JsonStyle::Lines, false),
                &columns,
                &rows
            ),
            "{\"doc\":{\"z\": 12345678901234567890123,  \"a\": 0.10000000000000000001}}\n\
             {\"doc\":\"not json\"}"
        );
    }

    #[test]
    fn json_takes_fields_later_pages_bring() {
        let mut out = Vec::new();
        let mut writer = JsonWriter::new(JsonStyle::Lines, false);
        block_on(export(
            &growing(),
            &mut writer,
            &ExportOptions::default(),
            &mut out,
            |_| true,
        ))
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let last = text.lines().last().unwrap();
        assert_eq!(last, "{\"a,b\":2,\"v\":1,\"new\":\"kept\"}");
    }

    #[test]
    fn markdown_escapes_pipes_and_breaks() {
        let columns = vec![
            Column::new("a|b", "text", ValueKind::Text),
            Column::new("c", "text", ValueKind::Text),
        ];
        let rows = vec![vec![Value::text("x\ny"), Value::Null]];
        assert_eq!(
            write_rows(&mut MarkdownWriter::new(), &columns, &rows),
            "| a\\|b | c |\n| --- | --- |\n| x y | NULL |"
        );
    }
}

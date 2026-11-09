//! Streaming CSV export straight from a [`PageLoader`], one page in memory at a time.
//!
//! The output follows RFC 4180: fields separated by the delimiter, records ended by
//! CRLF, and a field quoted with double quotes when it holds the delimiter, a quote, a
//! carriage return or a line feed, with quotes inside doubled. Null is an empty field,
//! booleans are `true` and `false`, bytes are `0x` followed by hex.
//!
//! The header is the first page's columns, or [`CsvOptions::columns`] when given (the
//! cache's merged list is the natural choice for a document collection the person has
//! been scrolling, since columns a later page introduces cannot be added to a header
//! already written and are left out).
//!
//! The writer is used synchronously from the export future, so the future belongs on a
//! thread that may block, such as the blocking hook or a dedicated thread.

use std::io::Write;
use std::sync::Arc;

use deltos_data::{Column, Error, Result, Value};

use crate::columns::ColumnSet;
use crate::loader::{PageLoader, PageRequest};

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

/// Writes every row the loader has as CSV and returns how many. `progress` is called
/// after each page with the running total; returning `false` stops the export with
/// [`Error::Cancelled`].
pub async fn export_csv<W: Write>(
    loader: &dyn PageLoader,
    options: &CsvOptions,
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
            if options.header {
                for (i, column) in header.iter().enumerate() {
                    if i > 0 {
                        buf.push(options.delimiter);
                    }
                    write_text(&mut buf, &column.name, options.delimiter);
                }
                buf.extend_from_slice(b"\r\n");
            }
        }
        let map = set.merge(&loaded.columns);
        let count = loaded.rows.len();
        for row in loaded.rows {
            let row = map.align(row, width);
            for (i, value) in row.iter().enumerate() {
                if i > 0 {
                    buf.push(options.delimiter);
                }
                write_value(&mut buf, value, options.delimiter);
            }
            buf.extend_from_slice(b"\r\n");
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
    out.flush().map_err(write_error)?;
    Ok(written)
}

fn write_error(e: std::io::Error) -> Error {
    Error::Other(format!("could not write the export: {e}"))
}

fn write_value(buf: &mut Vec<u8>, value: &Value, delimiter: u8) {
    match value {
        Value::Null => {}
        Value::Bool(true) => buf.extend_from_slice(b"true"),
        Value::Bool(false) => buf.extend_from_slice(b"false"),
        Value::Int(i) => {
            let _ = write!(buf, "{i}");
        }
        Value::Float(f) => {
            let _ = write!(buf, "{f}");
        }
        Value::Text(s) | Value::Json(s) => write_text(buf, s, delimiter),
        Value::Bytes(bytes) => {
            buf.extend_from_slice(b"0x");
            for b in bytes.iter() {
                let _ = write!(buf, "{b:02x}");
            }
        }
    }
}

fn write_text(buf: &mut Vec<u8>, text: &str, delimiter: u8) {
    let needs_quotes = text
        .bytes()
        .any(|b| b == delimiter || b == b'"' || b == b'\n' || b == b'\r');
    if !needs_quotes {
        buf.extend_from_slice(text.as_bytes());
        return;
    }
    buf.push(b'"');
    for b in text.bytes() {
        if b == b'"' {
            buf.push(b'"');
        }
        buf.push(b);
    }
    buf.push(b'"');
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

    #[test]
    fn quotes_what_rfc_4180_needs_and_aligns_columns() {
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
        let loader = Static(vec![
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
                rows: vec![vec![Value::Int(1), Value::text("dropped"), Value::Int(2)].into()],
                end: true,
            },
        ]);
        let mut out = Vec::new();
        block_on(export_csv(
            &loader,
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
}

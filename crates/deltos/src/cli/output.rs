//! How results are written: the row formats, and an aligned table for terminals.

use std::path::Path;

use deltos_data::{Column, DriverKind, ObjectRef, Value};
use deltos_grid::{CsvFormat, CsvWriter, JsonStyle, JsonWriter, MarkdownWriter, RowWriter};

/// Rows the table format reads before it fixes its column widths. Rows after them are
/// cut to those widths, so memory stays bounded however long the result is.
const TABLE_SAMPLE: usize = 200;
/// The widest a table column grows, in characters.
const TABLE_MAX_WIDTH: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Table,
    Csv,
    Tsv,
    Json,
    JsonLines,
    Markdown,
    Sql,
}

impl Format {
    pub fn parse(text: &str) -> Result<Self, String> {
        Ok(match text.trim().to_ascii_lowercase().as_str() {
            "table" => Format::Table,
            "csv" => Format::Csv,
            "tsv" => Format::Tsv,
            "json" => Format::Json,
            "jsonl" | "ndjson" | "json-lines" => Format::JsonLines,
            "md" | "markdown" => Format::Markdown,
            "sql" => Format::Sql,
            other => {
                return Err(format!(
                    "unknown format “{other}”: use table, csv, tsv, json, jsonl, md or sql"
                ));
            }
        })
    }

    /// The format a file name's extension asks for.
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?;
        match ext.to_ascii_lowercase().as_str() {
            "txt" => Some(Format::Table),
            e => Format::parse(e).ok(),
        }
    }

    /// `--format`, else the output file's extension, else `fallback`.
    pub fn pick(
        given: Option<&str>,
        output: Option<&Path>,
        fallback: Self,
    ) -> Result<Self, String> {
        match given {
            Some(f) => Format::parse(f),
            None => Ok(output.and_then(Format::from_path).unwrap_or(fallback)),
        }
    }

    /// A writer for this format. `object` names the table INSERTs go into.
    pub fn writer(
        self,
        kind: DriverKind,
        object: Option<&ObjectRef>,
    ) -> Result<Box<dyn RowWriter>, String> {
        Ok(match self {
            Format::Table => Box::new(TableWriter::default()),
            Format::Csv => Box::new(CsvWriter::new(CsvFormat {
                line_end: "\n",
                ..CsvFormat::default()
            })),
            Format::Tsv => Box::new(CsvWriter::new(CsvFormat {
                delimiter: b'\t',
                line_end: "\n",
                ..CsvFormat::default()
            })),
            Format::Json => Box::new(JsonWriter::new(JsonStyle::Array, false)),
            Format::JsonLines => Box::new(JsonWriter::new(JsonStyle::Lines, false)),
            Format::Markdown => Box::new(MarkdownWriter::new()),
            Format::Sql => {
                if !crate::ui::edit::speaks_sql(kind) {
                    return Err(format!("{} has no INSERT statements", kind.label()));
                }
                let object = object.ok_or("the sql format needs a table: use export")?;
                Box::new(crate::ui::transfer::sql::InsertWriter::new(
                    kind,
                    object,
                    crate::ui::transfer::sql::default_rows_per_statement(kind),
                ))
            }
        })
    }
}

/// Columns padded to line up, under a header and a rule. Values are put on one line and
/// cut to [`TABLE_MAX_WIDTH`].
#[derive(Debug, Default)]
pub struct TableWriter {
    columns: Vec<String>,
    numeric: Vec<bool>,
    widths: Vec<usize>,
    /// Rows held until the widths are fixed.
    sample: Vec<Vec<String>>,
    fixed: bool,
}

impl TableWriter {
    fn cell(value: &Value) -> String {
        value.preview(TABLE_MAX_WIDTH)
    }

    fn fix(&mut self, out: &mut Vec<u8>) {
        if self.fixed {
            return;
        }
        self.fixed = true;
        for row in &self.sample {
            for (w, c) in self.widths.iter_mut().zip(row) {
                *w = (*w).max(c.chars().count().min(TABLE_MAX_WIDTH));
            }
        }
        let header = self.columns.clone();
        self.line(&header, out, false);
        let rule: Vec<String> = self.widths.iter().map(|w| "-".repeat(*w)).collect();
        self.line(&rule, out, false);
        for row in std::mem::take(&mut self.sample) {
            self.line(&row, out, true);
        }
    }

    fn line(&self, cells: &[String], out: &mut Vec<u8>, align_numbers: bool) {
        let mut text = String::new();
        for (i, cell) in cells.iter().enumerate() {
            let width = self.widths.get(i).copied().unwrap_or(0);
            let cell: String = if cell.chars().count() > width {
                let mut c: String = cell.chars().take(width.saturating_sub(1)).collect();
                c.push('…');
                c
            } else {
                cell.clone()
            };
            let pad = width.saturating_sub(cell.chars().count());
            if i > 0 {
                text.push_str("  ");
            }
            let right = align_numbers && self.numeric.get(i).copied().unwrap_or(false);
            if right {
                text.extend(std::iter::repeat_n(' ', pad));
                text.push_str(&cell);
            } else {
                text.push_str(&cell);
                if i + 1 < cells.len() {
                    text.extend(std::iter::repeat_n(' ', pad));
                }
            }
        }
        out.extend_from_slice(text.trim_end().as_bytes());
        out.push(b'\n');
    }
}

impl RowWriter for TableWriter {
    fn begin(&mut self, columns: &[Column], _out: &mut Vec<u8>) {
        self.columns = columns.iter().map(|c| c.name.clone()).collect();
        self.numeric = columns.iter().map(|c| c.kind.is_numeric()).collect();
        self.widths = self
            .columns
            .iter()
            .map(|c| c.chars().count().min(TABLE_MAX_WIDTH))
            .collect();
    }

    fn row(&mut self, row: &[Value], out: &mut Vec<u8>) {
        let cells: Vec<String> = row.iter().map(Self::cell).collect();
        if self.fixed {
            self.line(&cells, out, true);
            return;
        }
        self.sample.push(cells);
        if self.sample.len() >= TABLE_SAMPLE {
            self.fix(out);
        }
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        self.fix(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_data::ValueKind;

    fn render(columns: &[Column], rows: &[Vec<Value>]) -> String {
        deltos_grid::write_rows(&mut TableWriter::default(), columns, rows)
    }

    #[test]
    fn lines_up_columns_and_numbers() {
        let columns = [
            Column::new("id", "int", ValueKind::Int),
            Column::new("name", "text", ValueKind::Text),
        ];
        let rows = vec![
            vec![Value::Int(7), Value::text("Ada")],
            vec![Value::Int(1234), Value::Null],
        ];
        assert_eq!(
            render(&columns, &rows),
            "id    name\n----  ----\n   7  Ada\n1234  NULL"
        );
    }

    #[test]
    fn keeps_values_on_one_line_and_short() {
        let columns = [Column::new("t", "text", ValueKind::Text)];
        let long = "x".repeat(100);
        let rows = vec![vec![Value::text("a\nb")], vec![Value::text(long)]];
        let out = render(&columns, &rows);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[2], "a b");
        assert_eq!(lines[3].chars().count(), TABLE_MAX_WIDTH);
        assert!(lines[3].ends_with('…'));
    }

    #[test]
    fn picks_the_format() {
        let out = Some(Path::new("rows.jsonl"));
        assert_eq!(Format::pick(None, out, Format::Csv), Ok(Format::JsonLines));
        assert_eq!(
            Format::pick(Some("md"), out, Format::Csv),
            Ok(Format::Markdown)
        );
        assert_eq!(Format::pick(None, None, Format::Tsv), Ok(Format::Tsv));
        assert!(Format::parse("xml").is_err());
    }
}

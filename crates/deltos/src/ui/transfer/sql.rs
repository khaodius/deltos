//! Rows as SQL `INSERT` statements, several rows to a statement, quoted for the database
//! they are meant for. The table menu's "Copy as INSERT" and the SQL export both write
//! through [`InsertWriter`].

use deltos_data::{Column, DriverKind, ObjectRef, Value};
use deltos_grid::RowWriter;

use crate::ui::edit;

/// Rows per statement a SQL export starts with: MySQL's default packet limit is the
/// tightest, so it gets smaller statements.
pub fn default_rows_per_statement(kind: DriverKind) -> usize {
    match kind {
        DriverKind::Mysql => 100,
        _ => 500,
    }
}

/// Writes `INSERT INTO table (columns) VALUES (...), (...);` with up to
/// `rows_per_statement` rows each. A statement of one row stays on one line.
#[derive(Debug, Clone)]
pub struct InsertWriter {
    kind: DriverKind,
    table: String,
    /// What goes before the first statement, such as a `CREATE TABLE`.
    preamble: Option<String>,
    rows_per_statement: usize,
    /// The quoted column list of the statements being written.
    columns: String,
    width: usize,
    /// Rows in the statement still open.
    pending: usize,
}

impl InsertWriter {
    pub fn new(kind: DriverKind, object: &ObjectRef, rows_per_statement: usize) -> Self {
        Self {
            kind,
            table: edit::table_name(kind, object),
            preamble: None,
            rows_per_statement: rows_per_statement.max(1),
            columns: String::new(),
            width: 0,
            pending: 0,
        }
    }

    /// Writes `text` (a `CREATE TABLE` statement, say) and a blank line first.
    pub fn with_preamble(mut self, text: Option<String>) -> Self {
        self.preamble = text.filter(|t| !t.trim().is_empty());
        self
    }

    fn set_columns(&mut self, columns: &[Column]) {
        self.width = columns.len();
        self.columns = columns
            .iter()
            .map(|c| edit::quote_ident(self.kind, &c.name))
            .collect::<Vec<_>>()
            .join(", ");
    }

    fn close(&mut self, out: &mut Vec<u8>) {
        if self.pending > 0 {
            out.extend_from_slice(b";\n");
            self.pending = 0;
        }
    }
}

impl RowWriter for InsertWriter {
    fn begin(&mut self, columns: &[Column], out: &mut Vec<u8>) {
        if let Some(text) = &self.preamble {
            out.extend_from_slice(text.trim_end().as_bytes());
            if !text.trim_end().ends_with(';') {
                out.push(b';');
            }
            out.extend_from_slice(b"\n\n");
        }
        self.set_columns(columns);
    }

    fn columns_grew(&mut self, columns: &[Column], out: &mut Vec<u8>) {
        // A statement's column list is fixed, so the next rows start a new one.
        self.close(out);
        self.set_columns(columns);
    }

    fn row(&mut self, row: &[Value], out: &mut Vec<u8>) {
        if self.pending == 0 {
            out.extend_from_slice(b"INSERT INTO ");
            out.extend_from_slice(self.table.as_bytes());
            out.extend_from_slice(b" (");
            out.extend_from_slice(self.columns.as_bytes());
            out.extend_from_slice(b") VALUES");
            out.extend_from_slice(if self.rows_per_statement == 1 {
                b" ("
            } else {
                b"\n  ("
            });
        } else {
            out.extend_from_slice(b",\n  (");
        }
        for (i, value) in row.iter().take(self.width).enumerate() {
            if i > 0 {
                out.extend_from_slice(b", ");
            }
            out.extend_from_slice(edit::literal(self.kind, value).as_bytes());
        }
        out.push(b')');
        self.pending += 1;
        if self.pending >= self.rows_per_statement {
            self.close(out);
        }
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        self.close(out);
    }
}

/// The statement that empties `object`, for "Empty the table first" on import.
pub fn truncate_statement(kind: DriverKind, object: &ObjectRef) -> String {
    match deltos_drivers::ddl::Flavor::of(kind, "") {
        Some(flavor) => deltos_drivers::ddl::truncate_table(flavor, object),
        None => format!("TRUNCATE TABLE {}", edit::table_name(kind, object)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_data::{ObjectKind, ValueKind};
    use deltos_grid::write_rows;

    fn object(namespace: &[&str]) -> ObjectRef {
        ObjectRef {
            namespace: namespace.iter().map(|s| s.to_string()).collect(),
            name: "orders".into(),
            kind: ObjectKind::Table,
        }
    }

    fn columns() -> Vec<Column> {
        vec![
            Column::new("id", "int", ValueKind::Int),
            Column::new("name", "text", ValueKind::Text),
            Column::new("ok", "bool", ValueKind::Bool),
        ]
    }

    #[test]
    fn batches_rows_into_statements() {
        let rows: Vec<Vec<Value>> = (1..=5)
            .map(|i| {
                vec![
                    Value::Int(i),
                    Value::text(format!("n'{i}")),
                    Value::Bool(i % 2 == 0),
                ]
            })
            .collect();
        let mut writer = InsertWriter::new(DriverKind::Postgres, &object(&["shop", "public"]), 2);
        assert_eq!(
            write_rows(&mut writer, &columns(), &rows),
            "INSERT INTO \"public\".\"orders\" (\"id\", \"name\", \"ok\") VALUES\n  (1, 'n''1', FALSE),\n  (2, 'n''2', TRUE);\n\
             INSERT INTO \"public\".\"orders\" (\"id\", \"name\", \"ok\") VALUES\n  (3, 'n''3', FALSE),\n  (4, 'n''4', TRUE);\n\
             INSERT INTO \"public\".\"orders\" (\"id\", \"name\", \"ok\") VALUES\n  (5, 'n''5', FALSE);"
        );
        let mut writer = InsertWriter::new(DriverKind::Mysql, &object(&["shop"]), 1)
            .with_preamble(Some("CREATE TABLE `orders` (`id` int)".into()));
        assert_eq!(
            write_rows(&mut writer, &columns(), &rows[..2]),
            "CREATE TABLE `orders` (`id` int);\n\n\
             INSERT INTO `shop`.`orders` (`id`, `name`, `ok`) VALUES (1, 'n''1', 0);\n\
             INSERT INTO `shop`.`orders` (`id`, `name`, `ok`) VALUES (2, 'n''2', 1);"
        );
    }

    #[test]
    fn new_columns_start_a_new_statement() {
        let mut writer = InsertWriter::new(DriverKind::Sqlite, &object(&["main"]), 10);
        let mut out = Vec::new();
        let first = columns();
        writer.begin(&first[..1], &mut out);
        writer.row(&[Value::Int(1)], &mut out);
        writer.columns_grew(&first[..2], &mut out);
        writer.row(&[Value::Int(2), Value::Null], &mut out);
        writer.finish(&mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "INSERT INTO \"orders\" (\"id\") VALUES\n  (1);\nINSERT INTO \"orders\" (\"id\", \"name\") VALUES\n  (2, NULL);\n"
        );
    }

    #[test]
    fn empties_tables_per_database() {
        assert_eq!(
            truncate_statement(DriverKind::Sqlite, &object(&["main"])),
            "DELETE FROM \"orders\""
        );
        assert_eq!(
            truncate_statement(DriverKind::Mysql, &object(&["shop"])),
            "TRUNCATE TABLE `shop`.`orders`"
        );
        assert_eq!(
            truncate_statement(DriverKind::Mongodb, &object(&["shop"])),
            "db.getCollection(\"orders\").deleteMany({})"
        );
    }
}

//! Rows as SQL `INSERT` statements, several rows to a statement, quoted for the database
//! they are meant for. The table menu's "Copy as INSERT" and the SQL export both write
//! through [`InsertWriter`]; a MongoDB dump writes shell calls through [`MongoWriter`].

use deltos_data::{Column, DriverKind, ObjectRef, Value, ValueKind};
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
    /// The column names of the statements being written, unquoted.
    names: Vec<String>,
    /// Rows in the statement still open.
    pending: usize,
    /// `INSERT+UPDATE`: the key a row that exists already is matched on. An empty key
    /// still makes MySQL update (it matches on any unique key) and the others skip.
    upsert: Option<Vec<String>>,
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
            names: Vec::new(),
            pending: 0,
            upsert: None,
        }
    }

    /// Makes each statement update the rows that exist already, matched on `key`.
    pub fn with_upsert(mut self, key: Vec<String>) -> Self {
        self.upsert = Some(key);
        self
    }

    /// Writes `text` (a `CREATE TABLE` statement, say) and a blank line first.
    pub fn with_preamble(mut self, text: Option<String>) -> Self {
        self.preamble = text.filter(|t| !t.trim().is_empty());
        self
    }

    fn set_columns(&mut self, columns: &[Column]) {
        self.width = columns.len();
        self.names = columns.iter().map(|c| c.name.clone()).collect();
        self.columns = columns
            .iter()
            .map(|c| edit::quote_ident(self.kind, &c.name))
            .collect::<Vec<_>>()
            .join(", ");
    }

    /// What makes an `INSERT` update the rows that exist: `ON DUPLICATE KEY UPDATE` on
    /// MySQL, `ON CONFLICT ... DO UPDATE` elsewhere.
    fn upsert_clause(&self) -> Option<String> {
        let key = self.upsert.as_ref()?;
        let q = |n: &str| edit::quote_ident(self.kind, n);
        if self.kind == DriverKind::Mysql {
            let set: Vec<String> = self
                .names
                .iter()
                .map(|n| format!("{} = VALUES({})", q(n), q(n)))
                .collect();
            return Some(format!("\nON DUPLICATE KEY UPDATE {}", set.join(", ")));
        }
        if key.is_empty() {
            return Some("\nON CONFLICT DO NOTHING".into());
        }
        let target: Vec<String> = key.iter().map(|k| q(k)).collect();
        let set: Vec<String> = self
            .names
            .iter()
            .filter(|n| !key.contains(n))
            .map(|n| format!("{} = EXCLUDED.{}", q(n), q(n)))
            .collect();
        Some(if set.is_empty() {
            format!("\nON CONFLICT ({}) DO NOTHING", target.join(", "))
        } else {
            format!(
                "\nON CONFLICT ({}) DO UPDATE SET {}",
                target.join(", "),
                set.join(", ")
            )
        })
    }

    fn close(&mut self, out: &mut Vec<u8>) {
        if self.pending > 0 {
            if let Some(clause) = self.upsert_clause() {
                out.extend_from_slice(clause.as_bytes());
            }
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

/// How a MongoDB dump writes documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MongoMode {
    /// `insertMany`, up to a batch of documents a call.
    Insert,
    /// A `replaceOne` with `upsert` per document, matched on `_id`. Documents without an
    /// `_id` are inserted.
    Upsert,
}

/// `db.getCollection("name")`, or in another database `db.getSiblingDB("db")...`.
pub fn mongo_collection(database: Option<&str>, name: &str) -> String {
    match database {
        Some(db) => format!(
            "db.getSiblingDB({}).getCollection({})",
            edit::json_string(db),
            edit::json_string(name)
        ),
        None => format!("db.getCollection({})", edit::json_string(name)),
    }
}

/// Documents as MongoDB shell calls that Deltos' shell and `mongosh` both run. A field
/// that is null is left out of its document: the grid shows a field a document lacks as
/// null, and sparse documents are far more common than explicit nulls.
#[derive(Debug, Clone)]
pub struct MongoWriter {
    /// What the calls are made on, from [`mongo_collection`].
    collection: String,
    mode: MongoMode,
    batch: usize,
    /// Each field's name as a JSON string, and its kind for the literal.
    fields: Vec<(String, ValueKind)>,
    id: Option<usize>,
    /// Documents in the `insertMany` still open.
    pending: usize,
}

impl MongoWriter {
    pub fn new(collection: String, mode: MongoMode, batch: usize) -> Self {
        Self {
            collection,
            mode,
            batch: batch.max(1),
            fields: Vec::new(),
            id: None,
            pending: 0,
        }
    }

    fn set_columns(&mut self, columns: &[Column]) {
        self.fields = columns
            .iter()
            .map(|c| (edit::json_string(&c.name), c.kind))
            .collect();
        self.id = columns.iter().position(|c| c.name == "_id");
    }

    fn document(&self, row: &[Value]) -> String {
        let fields: Vec<String> = self
            .fields
            .iter()
            .zip(row)
            .filter(|(_, v)| !matches!(v, Value::Null))
            .map(|((name, kind), v)| format!("{name}: {}", edit::mongo_literal(v, *kind)))
            .collect();
        if fields.is_empty() {
            "{}".into()
        } else {
            format!("{{ {} }}", fields.join(", "))
        }
    }

    fn close(&mut self, out: &mut Vec<u8>) {
        if self.pending > 0 {
            out.extend_from_slice(b"\n]);\n");
            self.pending = 0;
        }
    }
}

impl RowWriter for MongoWriter {
    fn begin(&mut self, columns: &[Column], _out: &mut Vec<u8>) {
        self.set_columns(columns);
    }

    fn columns_grew(&mut self, columns: &[Column], _out: &mut Vec<u8>) {
        // Each document names its own fields, so an open call carries on.
        self.set_columns(columns);
    }

    fn row(&mut self, row: &[Value], out: &mut Vec<u8>) {
        let doc = self.document(row);
        let id = self
            .id
            .filter(|_| self.mode == MongoMode::Upsert)
            .and_then(|i| Some((row.get(i)?, self.fields.get(i)?.1)))
            .filter(|(v, _)| !matches!(v, Value::Null));
        if let Some((id, kind)) = id {
            self.close(out);
            out.extend_from_slice(
                format!(
                    "{}.replaceOne({{ \"_id\": {} }}, {doc}, {{ \"upsert\": true }});\n",
                    self.collection,
                    edit::mongo_literal(id, kind)
                )
                .as_bytes(),
            );
            return;
        }
        if self.pending == 0 {
            out.extend_from_slice(self.collection.as_bytes());
            out.extend_from_slice(b".insertMany([\n  ");
        } else {
            out.extend_from_slice(b",\n  ");
        }
        out.extend_from_slice(doc.as_bytes());
        self.pending += 1;
        if self.pending >= self.batch {
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
    fn upserts_per_database() {
        let rows: Vec<Vec<Value>> = vec![vec![Value::Int(1), Value::text("a"), Value::Bool(true)]];
        let mut writer =
            InsertWriter::new(DriverKind::Mysql, &object(&[]), 10).with_upsert(vec!["id".into()]);
        assert_eq!(
            write_rows(&mut writer, &columns(), &rows),
            "INSERT INTO `orders` (`id`, `name`, `ok`) VALUES\n  (1, 'a', 1)\n\
             ON DUPLICATE KEY UPDATE `id` = VALUES(`id`), `name` = VALUES(`name`), `ok` = VALUES(`ok`);"
        );
        let mut writer = InsertWriter::new(DriverKind::Postgres, &object(&["shop", "public"]), 10)
            .with_upsert(vec!["id".into()]);
        assert_eq!(
            write_rows(&mut writer, &columns(), &rows),
            "INSERT INTO \"public\".\"orders\" (\"id\", \"name\", \"ok\") VALUES\n  (1, 'a', TRUE)\n\
             ON CONFLICT (\"id\") DO UPDATE SET \"name\" = EXCLUDED.\"name\", \"ok\" = EXCLUDED.\"ok\";"
        );
        let mut writer =
            InsertWriter::new(DriverKind::Sqlite, &object(&["main"]), 10).with_upsert(Vec::new());
        assert!(write_rows(&mut writer, &columns(), &rows).ends_with("ON CONFLICT DO NOTHING;"));
    }

    #[test]
    fn writes_mongo_shell_calls() {
        let columns = vec![
            Column::new("_id", "objectId", ValueKind::ObjectId),
            Column::new("name", "string", ValueKind::Text),
            Column::new("age", "int", ValueKind::Int),
        ];
        let id = "64b7f0c2a1b2c3d4e5f60718";
        let rows = vec![
            vec![Value::text(id), Value::text("Ann"), Value::Int(3)],
            vec![Value::text(id), Value::Null, Value::Int(4)],
        ];
        let mut writer = MongoWriter::new(mongo_collection(None, "people"), MongoMode::Insert, 100);
        assert_eq!(
            write_rows(&mut writer, &columns, &rows),
            format!(
                "db.getCollection(\"people\").insertMany([\n  \
                 {{ \"_id\": ObjectId(\"{id}\"), \"name\": \"Ann\", \"age\": 3 }},\n  \
                 {{ \"_id\": ObjectId(\"{id}\"), \"age\": 4 }}\n]);"
            )
        );
        let mut writer = MongoWriter::new(
            mongo_collection(Some("shop"), "people"),
            MongoMode::Upsert,
            100,
        );
        assert_eq!(
            write_rows(&mut writer, &columns, &rows[..1]),
            format!(
                "db.getSiblingDB(\"shop\").getCollection(\"people\").replaceOne(\
                 {{ \"_id\": ObjectId(\"{id}\") }}, \
                 {{ \"_id\": ObjectId(\"{id}\"), \"name\": \"Ann\", \"age\": 3 }}, \
                 {{ \"upsert\": true }});"
            )
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

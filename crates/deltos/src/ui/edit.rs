//! Turning what the person types into a cell into a value of the column's type, finding a
//! row's key, and writing rows out as SQL, for editing and copying in the table view. The
//! review of staged changes shows the statements a commit runs from here too.

use deltos_data::{Column, DriverKind, ObjectRef, RowEdit, Value, ValueKind};

use crate::ui::grid::CellInput;

/// The value `input` stands for in `column`, or why it does not fit. Dates, times, UUIDs
/// and exact decimals go to the database as text, which checks and converts them.
pub fn parse_input(column: &Column, input: &CellInput) -> Result<Value, String> {
    let text = match input {
        CellInput::Null if column.nullable == Some(false) => {
            return Err(format!("{} can't be NULL", column.name));
        }
        CellInput::Null => return Ok(Value::Null),
        CellInput::Text(t) => t,
    };
    let trimmed = text.trim();
    Ok(match column.kind {
        ValueKind::Int => Value::Int(
            trimmed
                .parse()
                .map_err(|_| format!("{} needs a whole number, not \"{trimmed}\"", column.name))?,
        ),
        ValueKind::Float => Value::Float(
            trimmed
                .parse()
                .map_err(|_| format!("{} needs a number, not \"{trimmed}\"", column.name))?,
        ),
        ValueKind::Decimal => {
            trimmed
                .parse::<f64>()
                .map_err(|_| format!("{} needs a number, not \"{trimmed}\"", column.name))?;
            Value::text(trimmed)
        }
        ValueKind::Bool => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "t" | "1" | "yes" | "y" | "on" => Value::Bool(true),
            "false" | "f" | "0" | "no" | "n" | "off" => Value::Bool(false),
            _ => return Err(format!("{} needs true or false", column.name)),
        },
        ValueKind::Json | ValueKind::Array => {
            serde_json::from_str::<serde_json::Value>(trimmed)
                .map_err(|e| format!("{} needs valid JSON: {e}", column.name))?;
            Value::Json(trimmed.into())
        }
        _ => Value::text(text.as_str()),
    })
}

/// The primary key of a row as `(column, value)` pairs in key order, or `None` when the
/// table has no key the grid knows about.
pub fn key_of(columns: &[Column], values: &[Value]) -> Option<Vec<(String, Value)>> {
    let mut key: Vec<(u16, String, Value)> = columns
        .iter()
        .zip(values)
        .filter_map(|(c, v)| c.primary_key.map(|p| (p, c.name.clone(), v.clone())))
        .collect();
    if key.is_empty() {
        return None;
    }
    key.sort_by_key(|(p, _, _)| *p);
    Some(key.into_iter().map(|(_, n, v)| (n, v)).collect())
}

/// Whether the copy-as-SQL items make sense for this database.
pub fn speaks_sql(kind: DriverKind) -> bool {
    !matches!(
        kind,
        DriverKind::Mongodb | DriverKind::Redis | DriverKind::Cassandra
    )
}

/// `name` as a quoted identifier for `kind`.
pub fn quote_ident(kind: DriverKind, name: &str) -> String {
    match kind {
        DriverKind::Mysql | DriverKind::Clickhouse => format!("`{}`", name.replace('`', "``")),
        DriverKind::Mssql => format!("[{}]", name.replace(']', "]]")),
        _ => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

/// `v` as a SQL literal for `kind`. A PostgreSQL string with a backslash in it is
/// written as an escape string (`E'a\\b'`), which reads the same whether or not the
/// server has `standard_conforming_strings` on; a plain `'a\b'` would lose the
/// backslash on a server that has it off.
pub fn literal(kind: DriverKind, v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => match kind {
            DriverKind::Sqlite | DriverKind::Mysql => (if *b { "1" } else { "0" }).into(),
            _ => (if *b { "TRUE" } else { "FALSE" }).into(),
        },
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => f.to_string(),
        // SQLite reads an out-of-range number as infinity and has no NaN (it stores
        // NULL); the others take the words PostgreSQL uses, in quotes.
        Value::Float(f) => match kind {
            DriverKind::Sqlite if f.is_nan() => "NULL".into(),
            DriverKind::Sqlite => (if *f > 0.0 { "9e999" } else { "-9e999" }).into(),
            _ if f.is_nan() => "'NaN'".into(),
            _ => (if *f > 0.0 {
                "'Infinity'"
            } else {
                "'-Infinity'"
            })
            .into(),
        },
        Value::Bytes(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            match kind {
                DriverKind::Postgres => format!("E'\\\\x{hex}'"),
                _ => format!("X'{hex}'"),
            }
        }
        other => {
            let s = other.to_string().replace('\'', "''");
            match kind {
                DriverKind::Mysql => format!("'{}'", s.replace('\\', "\\\\")),
                DriverKind::Postgres if s.contains('\\') => {
                    format!("E'{}'", s.replace('\\', "\\\\"))
                }
                _ => format!("'{s}'"),
            }
        }
    }
}

/// The table's qualified name, quoted for `kind`.
pub fn table_name(kind: DriverKind, object: &ObjectRef) -> String {
    let mut parts: Vec<&str> = match kind {
        // Postgres namespaces are database then schema; a statement names the schema only.
        DriverKind::Postgres => object
            .namespace
            .iter()
            .skip(1)
            .map(String::as_str)
            .collect(),
        DriverKind::Sqlite => object
            .namespace
            .iter()
            .filter(|n| n.as_str() != "main")
            .map(String::as_str)
            .collect(),
        _ => object.namespace.iter().map(String::as_str).collect(),
    };
    parts.push(&object.name);
    parts
        .iter()
        .map(|p| quote_ident(kind, p))
        .collect::<Vec<_>>()
        .join(".")
}

/// One `INSERT` statement per row.
pub fn insert_statements(
    kind: DriverKind,
    object: &ObjectRef,
    names: &[&str],
    rows: &[Vec<Value>],
) -> String {
    let columns: Vec<Column> = names
        .iter()
        .map(|n| Column::new(*n, "", ValueKind::Other))
        .collect();
    let mut writer = crate::ui::transfer::sql::InsertWriter::new(kind, object, 1);
    deltos_grid::write_rows(&mut writer, &columns, rows)
}

/// A `WHERE` clause that picks out one row by its key.
pub fn where_clause(kind: DriverKind, key: &[(String, Value)]) -> String {
    let parts: Vec<String> = key
        .iter()
        .map(|(n, v)| match v {
            Value::Null => format!("{} IS NULL", quote_ident(kind, n)),
            v => format!("{} = {}", quote_ident(kind, n), literal(kind, v)),
        })
        .collect();
    format!("WHERE {}", parts.join(" AND "))
}

/// One statement per edit, as a commit runs it, for the person to read before it does.
/// Drivers send values as parameters; here they are written as literals of the dialect.
/// MongoDB edits are shown as the shell calls that do the same.
pub fn preview_statements(
    kind: DriverKind,
    object: &ObjectRef,
    columns: &[Column],
    edits: &[RowEdit],
) -> Vec<String> {
    if kind == DriverKind::Mongodb {
        return edits
            .iter()
            .map(|e| mongo_statement(object, columns, e))
            .collect();
    }
    let table = table_name(kind, object);
    edits
        .iter()
        .map(|edit| match edit {
            RowEdit::Update { key, changes } => {
                let set = changes
                    .iter()
                    .map(|(n, v)| format!("{} = {}", quote_ident(kind, n), literal(kind, v)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("UPDATE {table} SET {set} {};", where_clause(kind, key))
            }
            RowEdit::Delete { key } => {
                format!("DELETE FROM {table} {};", where_clause(kind, key))
            }
            RowEdit::Insert { values } if values.is_empty() => match kind {
                DriverKind::Mysql => format!("INSERT INTO {table} () VALUES ();"),
                _ => format!("INSERT INTO {table} DEFAULT VALUES;"),
            },
            RowEdit::Insert { values } => {
                let names: Vec<&str> = values.iter().map(|(n, _)| n.as_str()).collect();
                let row: Vec<Value> = values.iter().map(|(_, v)| v.clone()).collect();
                insert_statements(kind, object, &names, &[row])
            }
        })
        .collect()
}

/// A JSON string literal, for the shell.
fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{}\"", s.replace('"', "\\\"")))
}

/// A value in MongoDB shell syntax, by the kind of its column.
fn mongo_literal(v: &Value, kind: ValueKind) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => f.to_string(),
        Value::Float(f) => format!("Number({})", json_string(&f.to_string())),
        Value::Json(s) => s.to_string(),
        Value::Bytes(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            format!("Binary.createFromHexString({})", json_string(&hex))
        }
        Value::Text(s) => {
            let text = s.trim();
            match kind {
                ValueKind::ObjectId
                    if text.len() == 24 && text.bytes().all(|b| b.is_ascii_hexdigit()) =>
                {
                    format!("ObjectId({})", json_string(text))
                }
                ValueKind::DateTime | ValueKind::Date => format!("ISODate({})", json_string(text)),
                ValueKind::Uuid => format!("UUID({})", json_string(text)),
                ValueKind::Decimal => format!("NumberDecimal({})", json_string(text)),
                ValueKind::Json | ValueKind::Array => s.to_string(),
                _ => json_string(s),
            }
        }
    }
}

fn mongo_document(pairs: &[(String, Value)], columns: &[Column]) -> String {
    if pairs.is_empty() {
        return "{}".into();
    }
    let kind = |name: &str| {
        columns
            .iter()
            .find(|c| c.name == name)
            .map_or(ValueKind::Other, |c| c.kind)
    };
    let fields = pairs
        .iter()
        .map(|(n, v)| format!("{}: {}", json_string(n), mongo_literal(v, kind(n))))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{ {fields} }}")
}

fn mongo_statement(object: &ObjectRef, columns: &[Column], edit: &RowEdit) -> String {
    let collection = match object.namespace.first() {
        Some(db) => format!(
            "db.getSiblingDB({}).getCollection({})",
            json_string(db),
            json_string(&object.name)
        ),
        None => format!("db.getCollection({})", json_string(&object.name)),
    };
    match edit {
        RowEdit::Update { key, changes } => format!(
            "{collection}.updateOne({}, {{ \"$set\": {} }})",
            mongo_document(key, columns),
            mongo_document(changes, columns)
        ),
        RowEdit::Delete { key } => {
            format!("{collection}.deleteOne({})", mongo_document(key, columns))
        }
        RowEdit::Insert { values } => {
            // The driver leaves out empty fields rather than storing nulls.
            let values: Vec<(String, Value)> = values
                .iter()
                .filter(|(_, v)| !v.is_null())
                .cloned()
                .collect();
            format!(
                "{collection}.insertOne({})",
                mongo_document(&values, columns)
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, kind: ValueKind) -> Column {
        Column::new(name, "t", kind)
    }

    #[test]
    fn reads_input_as_the_column_type() {
        let t = |s: &str| CellInput::Text(s.into());
        assert_eq!(
            parse_input(&col("n", ValueKind::Int), &t(" 42 ")),
            Ok(Value::Int(42))
        );
        assert!(parse_input(&col("n", ValueKind::Int), &t("4.2")).is_err());
        assert_eq!(
            parse_input(&col("ok", ValueKind::Bool), &t("Yes")),
            Ok(Value::Bool(true))
        );
        assert_eq!(
            parse_input(&col("price", ValueKind::Decimal), &t("19.990")),
            Ok(Value::text("19.990"))
        );
        assert!(parse_input(&col("doc", ValueKind::Json), &t("{oops")).is_err());
        assert_eq!(
            parse_input(&col("name", ValueKind::Text), &t(" spaced ")),
            Ok(Value::text(" spaced "))
        );
        let mut required = col("id", ValueKind::Int);
        required.nullable = Some(false);
        assert!(parse_input(&required, &CellInput::Null).is_err());
        assert_eq!(
            parse_input(&col("note", ValueKind::Text), &CellInput::Null),
            Ok(Value::Null)
        );
    }

    #[test]
    fn finds_the_key_in_key_order() {
        let mut a = col("a", ValueKind::Int);
        a.primary_key = Some(1);
        let mut b = col("b", ValueKind::Int);
        b.primary_key = Some(0);
        let c = col("c", ValueKind::Text);
        let values = [Value::Int(1), Value::Int(2), Value::text("x")];
        assert_eq!(
            key_of(&[a, b, c.clone()], &values),
            Some(vec![
                ("b".into(), Value::Int(2)),
                ("a".into(), Value::Int(1))
            ])
        );
        assert_eq!(key_of(&[c], &values[2..]), None);
    }

    #[test]
    fn literals_read_back_whatever_the_server_settings() {
        let pg = |v: &Value| literal(DriverKind::Postgres, v);
        assert_eq!(pg(&Value::text("a\\b'c")), "E'a\\\\b''c'");
        assert_eq!(pg(&Value::text("plain")), "'plain'");
        assert_eq!(pg(&Value::Bytes(vec![0, 255].into())), "E'\\\\x00ff'");
        assert_eq!(pg(&Value::Float(f64::NAN)), "'NaN'");
        assert_eq!(pg(&Value::Float(f64::NEG_INFINITY)), "'-Infinity'");
        assert_eq!(
            literal(DriverKind::Sqlite, &Value::Float(f64::INFINITY)),
            "9e999"
        );
        assert_eq!(literal(DriverKind::Sqlite, &Value::Float(f64::NAN)), "NULL");
        assert_eq!(literal(DriverKind::Mysql, &Value::text("a\\b")), "'a\\\\b'");
    }

    #[test]
    fn writes_inserts_per_dialect() {
        let object = ObjectRef {
            namespace: vec!["shop".into(), "public".into()],
            name: "orders".into(),
            kind: deltos_data::ObjectKind::Table,
        };
        let rows = vec![vec![Value::Int(1), Value::text("O'Brien"), Value::Null]];
        assert_eq!(
            insert_statements(
                DriverKind::Postgres,
                &object,
                &["id", "name", "note"],
                &rows
            ),
            r#"INSERT INTO "public"."orders" ("id", "name", "note") VALUES (1, 'O''Brien', NULL);"#
        );
        let object = ObjectRef {
            namespace: vec!["shop".into()],
            ..object
        };
        assert_eq!(
            insert_statements(DriverKind::Mysql, &object, &["id", "name", "note"], &rows),
            "INSERT INTO `shop`.`orders` (`id`, `name`, `note`) VALUES (1, 'O''Brien', NULL);"
        );
        assert_eq!(
            where_clause(DriverKind::Mysql, &[("id".into(), Value::Int(7))]),
            "WHERE `id` = 7"
        );
    }

    fn edits() -> Vec<RowEdit> {
        vec![
            RowEdit::Delete {
                key: vec![("id".into(), Value::Int(2))],
            },
            RowEdit::Update {
                key: vec![("id".into(), Value::Int(1))],
                changes: vec![
                    ("name".into(), Value::text("O'Brien")),
                    ("ok".into(), Value::Bool(true)),
                ],
            },
            RowEdit::Insert {
                values: vec![
                    ("name".into(), Value::text("new")),
                    ("note".into(), Value::Null),
                ],
            },
            RowEdit::Insert { values: vec![] },
        ]
    }

    fn object(namespace: &[&str]) -> ObjectRef {
        ObjectRef {
            namespace: namespace.iter().map(|s| (*s).to_owned()).collect(),
            name: "orders".into(),
            kind: deltos_data::ObjectKind::Table,
        }
    }

    #[test]
    fn previews_postgres() {
        let out = preview_statements(
            DriverKind::Postgres,
            &object(&["shop", "public"]),
            &[],
            &edits(),
        );
        assert_eq!(
            out,
            vec![
                r#"DELETE FROM "public"."orders" WHERE "id" = 2;"#,
                r#"UPDATE "public"."orders" SET "name" = 'O''Brien', "ok" = TRUE WHERE "id" = 1;"#,
                r#"INSERT INTO "public"."orders" ("name", "note") VALUES ('new', NULL);"#,
                r#"INSERT INTO "public"."orders" DEFAULT VALUES;"#,
            ]
        );
    }

    #[test]
    fn previews_mysql() {
        let mut e = edits();
        e.push(RowEdit::Update {
            key: vec![("id".into(), Value::Int(3))],
            changes: vec![("path".into(), Value::text(r"C:\temp"))],
        });
        let out = preview_statements(DriverKind::Mysql, &object(&["shop"]), &[], &e);
        assert_eq!(out[0], "DELETE FROM `shop`.`orders` WHERE `id` = 2;");
        assert_eq!(
            out[1],
            "UPDATE `shop`.`orders` SET `name` = 'O''Brien', `ok` = 1 WHERE `id` = 1;"
        );
        assert_eq!(out[3], "INSERT INTO `shop`.`orders` () VALUES ();");
        assert_eq!(
            out[4],
            r"UPDATE `shop`.`orders` SET `path` = 'C:\\temp' WHERE `id` = 3;"
        );
    }

    #[test]
    fn previews_sqlite() {
        let out = preview_statements(DriverKind::Sqlite, &object(&["main"]), &[], &edits());
        assert_eq!(out[0], r#"DELETE FROM "orders" WHERE "id" = 2;"#);
        assert_eq!(
            out[1],
            r#"UPDATE "orders" SET "name" = 'O''Brien', "ok" = 1 WHERE "id" = 1;"#
        );
        assert_eq!(out[3], r#"INSERT INTO "orders" DEFAULT VALUES;"#);
        // A NULL key is matched with IS NULL, as the driver does.
        let null_key = [RowEdit::Delete {
            key: vec![("id".into(), Value::Null)],
        }];
        assert_eq!(
            preview_statements(DriverKind::Sqlite, &object(&["main"]), &[], &null_key)[0],
            r#"DELETE FROM "orders" WHERE "id" IS NULL;"#
        );
    }

    #[test]
    fn previews_mongo_shell_calls() {
        let mut id = col("_id", ValueKind::ObjectId);
        id.primary_key = Some(0);
        let columns = [
            id,
            col("tags", ValueKind::Array),
            col("at", ValueKind::DateTime),
        ];
        let oid = Value::text("65a1b2c3d4e5f60718293a4b");
        let e = [
            RowEdit::Update {
                key: vec![("_id".into(), oid.clone())],
                changes: vec![
                    ("tags".into(), Value::Json(r#"["a","b"]"#.into())),
                    ("name".into(), Value::text("say \"hi\"")),
                ],
            },
            RowEdit::Delete {
                key: vec![("_id".into(), oid)],
            },
            RowEdit::Insert {
                values: vec![
                    ("at".into(), Value::text("2024-03-09T14:05:00Z")),
                    ("note".into(), Value::Null),
                ],
            },
        ];
        let out = preview_statements(DriverKind::Mongodb, &object(&["shop"]), &columns, &e);
        let c = r#"db.getSiblingDB("shop").getCollection("orders")"#;
        assert_eq!(
            out[0],
            format!(
                r#"{c}.updateOne({{ "_id": ObjectId("65a1b2c3d4e5f60718293a4b") }}, {{ "$set": {{ "tags": ["a","b"], "name": "say \"hi\"" }} }})"#
            )
        );
        assert_eq!(
            out[1],
            format!(r#"{c}.deleteOne({{ "_id": ObjectId("65a1b2c3d4e5f60718293a4b") }})"#)
        );
        assert_eq!(
            out[2],
            format!(r#"{c}.insertOne({{ "at": ISODate("2024-03-09T14:05:00Z") }})"#)
        );
    }
}

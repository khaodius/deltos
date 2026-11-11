//! Turning what the person types into a cell into a value of the column's type, finding a
//! row's key, and writing rows out as SQL, for editing and copying in the table view.

use deltos_data::{Column, DriverKind, ObjectRef, Value, ValueKind};

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

fn quote_ident(kind: DriverKind, name: &str) -> String {
    match kind {
        DriverKind::Mysql | DriverKind::Clickhouse => format!("`{}`", name.replace('`', "``")),
        DriverKind::Mssql => format!("[{}]", name.replace(']', "]]")),
        _ => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

fn literal(kind: DriverKind, v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => match kind {
            DriverKind::Sqlite | DriverKind::Mysql => (if *b { "1" } else { "0" }).into(),
            _ => (if *b { "TRUE" } else { "FALSE" }).into(),
        },
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => f.to_string(),
        Value::Bytes(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            match kind {
                DriverKind::Postgres => format!("'\\x{hex}'"),
                _ => format!("X'{hex}'"),
            }
        }
        other => {
            let s = other.to_string().replace('\'', "''");
            match kind {
                DriverKind::Mysql => format!("'{}'", s.replace('\\', "\\\\")),
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
    let table = table_name(kind, object);
    let columns = names
        .iter()
        .map(|n| quote_ident(kind, n))
        .collect::<Vec<_>>()
        .join(", ");
    rows.iter()
        .map(|row| {
            let values = row
                .iter()
                .map(|v| literal(kind, v))
                .collect::<Vec<_>>()
                .join(", ");
            format!("INSERT INTO {table} ({columns}) VALUES ({values});")
        })
        .collect::<Vec<_>>()
        .join("\n")
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
}

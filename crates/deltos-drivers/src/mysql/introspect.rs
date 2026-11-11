//! The MySQL catalog, from `information_schema` and `SHOW CREATE`.

use deltos_data::{
    Column, Error, ForeignKey, ForeignKeyAction, Index, Namespace, NamespaceKind, ObjectEntry,
    ObjectInfo, ObjectKind, ObjectRef, Result, Value, ValueKind,
};
use mysql_async::Conn;
use mysql_async::prelude::Queryable;

use super::decode::{self, kind_for, number, text};
use super::{DIALECT, my_error};

const SYSTEM_SCHEMAS: [&str; 4] = ["information_schema", "mysql", "performance_schema", "sys"];

async fn rows(conn: &mut Conn, sql: &str, params: &[Value]) -> Result<Vec<mysql_async::Row>> {
    conn.exec(sql, decode::params(params))
        .await
        .map_err(my_error)
}

/// The database an object lives in.
pub(super) fn database_of(object: &ObjectRef) -> Result<&str> {
    object
        .namespace
        .first()
        .map(String::as_str)
        .ok_or_else(|| Error::Query("Choose a database first.".to_string()))
}

pub(super) async fn namespaces(conn: &mut Conn) -> Result<Vec<Namespace>> {
    let rows = rows(
        conn,
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA",
        &[],
    )
    .await?;
    let mut names: Vec<String> = rows.iter().filter_map(|row| text(row.as_ref(0))).collect();
    names.sort_by_key(|name| {
        (
            SYSTEM_SCHEMAS.contains(&name.to_ascii_lowercase().as_str()),
            name.clone(),
        )
    });
    Ok(names
        .into_iter()
        .map(|name| Namespace {
            path: vec![name],
            kind: NamespaceKind::Database,
        })
        .collect())
}

pub(super) async fn objects(conn: &mut Conn, db: &str) -> Result<Vec<ObjectEntry>> {
    let tables = rows(
        conn,
        "SELECT TABLE_NAME, TABLE_TYPE, TABLE_ROWS, DATA_LENGTH + INDEX_LENGTH, TABLE_COMMENT \
         FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME",
        &[Value::text(db)],
    )
    .await?;
    let namespace = vec![db.to_string()];
    let mut out = Vec::with_capacity(tables.len());
    for row in tables {
        let Some(name) = text(row.as_ref(0)) else {
            continue;
        };
        let table_type = text(row.as_ref(1)).unwrap_or_default();
        let kind = match table_type.as_str() {
            "VIEW" | "SYSTEM VIEW" => ObjectKind::View,
            "SEQUENCE" => ObjectKind::Sequence,
            _ => ObjectKind::Table,
        };
        let comment = text(row.as_ref(4)).filter(|c| !c.is_empty() && c != "VIEW");
        out.push(ObjectEntry {
            object: ObjectRef {
                namespace: namespace.clone(),
                name,
                kind,
            },
            estimated_rows: if kind == ObjectKind::Table {
                number(row.as_ref(2))
            } else {
                None
            },
            size_bytes: number(row.as_ref(3)),
            comment,
        });
    }
    let routines = rows(
        conn,
        "SELECT ROUTINE_NAME, ROUTINE_TYPE FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = ? ORDER BY ROUTINE_NAME",
        &[Value::text(db)],
    )
    .await;
    match routines {
        Ok(routines) => {
            for row in routines {
                let Some(name) = text(row.as_ref(0)) else {
                    continue;
                };
                let kind = if text(row.as_ref(1)).as_deref() == Some("PROCEDURE") {
                    ObjectKind::Procedure
                } else {
                    ObjectKind::Function
                };
                out.push(ObjectEntry {
                    object: ObjectRef {
                        namespace: namespace.clone(),
                        name,
                        kind,
                    },
                    estimated_rows: None,
                    size_bytes: None,
                    comment: None,
                });
            }
        }
        Err(e) => tracing::debug!("could not list routines: {e}"),
    }
    Ok(out)
}

/// `TABLE_ROWS`, InnoDB's estimate. `None` for views, which keep none.
pub(super) async fn table_rows(conn: &mut Conn, db: &str, table: &str) -> Result<Option<u64>> {
    let rows = rows(
        conn,
        "SELECT TABLE_ROWS FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
        &[Value::text(db), Value::text(table)],
    )
    .await?;
    Ok(rows.first().and_then(|row| number(row.as_ref(0))))
}

pub(super) struct MyColumn {
    pub name: String,
    pub column_type: String,
    pub kind: ValueKind,
    pub nullable: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
    pub primary_key: Option<u16>,
}

impl MyColumn {
    pub fn column(&self) -> Column {
        Column {
            name: self.name.clone(),
            type_name: self.column_type.clone(),
            kind: self.kind,
            nullable: Some(self.nullable),
            primary_key: self.primary_key,
            default: self.default.clone(),
            comment: self.comment.clone(),
        }
    }
}

pub(super) struct TableInfo {
    pub db: String,
    pub name: String,
    pub is_view: bool,
    pub columns: Vec<MyColumn>,
    pub indexes: Vec<Index>,
    pub tie_break: Vec<String>,
    pub estimated_rows: Option<u64>,
    pub size_bytes: Option<u64>,
    pub comment: Option<String>,
}

impl TableInfo {
    pub async fn load(conn: &mut Conn, object: &ObjectRef) -> Result<TableInfo> {
        let db = database_of(object)?.to_string();
        let params = [Value::text(db.as_str()), Value::text(object.name.as_str())];
        let column_rows = rows(
            conn,
            "SELECT COLUMN_NAME, COLUMN_TYPE, DATA_TYPE, IS_NULLABLE, COLUMN_DEFAULT, COLUMN_COMMENT \
             FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
            &params,
        )
        .await?;
        if column_rows.is_empty() {
            return Err(Error::Query(format!(
                "There is no table or view named {} in {db}.",
                object.name
            )));
        }
        let mut columns: Vec<MyColumn> = column_rows
            .iter()
            .map(|row| {
                let column_type = text(row.as_ref(1)).unwrap_or_default();
                let data_type = text(row.as_ref(2)).unwrap_or_default();
                MyColumn {
                    name: text(row.as_ref(0)).unwrap_or_default(),
                    kind: kind_for(&data_type, &column_type),
                    column_type,
                    nullable: text(row.as_ref(3)).as_deref() == Some("YES"),
                    default: text(row.as_ref(4)),
                    comment: text(row.as_ref(5)).filter(|c| !c.is_empty()),
                    primary_key: None,
                }
            })
            .collect();

        let table_rows = rows(
            conn,
            "SELECT TABLE_TYPE, TABLE_ROWS, DATA_LENGTH + INDEX_LENGTH, TABLE_COMMENT \
             FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
            &params,
        )
        .await?;
        let table = table_rows.first();
        let table_type = table.and_then(|r| text(r.as_ref(0))).unwrap_or_default();
        let is_view = table_type.contains("VIEW");

        let statistics = rows(
            conn,
            "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME, INDEX_TYPE, NULLABLE \
             FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
             ORDER BY INDEX_NAME = 'PRIMARY' DESC, INDEX_NAME, SEQ_IN_INDEX",
            &params,
        )
        .await?;
        let mut indexes: Vec<Index> = Vec::new();
        // Indexes whose every part is a plain NOT NULL column can stand in for a key.
        let mut usable: Vec<bool> = Vec::new();
        for row in &statistics {
            let name = text(row.as_ref(0)).unwrap_or_default();
            let unique = number(row.as_ref(1)) == Some(0);
            let column = text(row.as_ref(2));
            let not_null = text(row.as_ref(4)).is_none_or(|n| n.is_empty());
            let position = match indexes.iter().position(|i| i.name == name) {
                Some(i) => i,
                None => {
                    let primary = name == "PRIMARY";
                    indexes.push(Index {
                        primary,
                        name,
                        columns: Vec::new(),
                        unique,
                        method: text(row.as_ref(3)),
                        // MySQL drops a UNIQUE key like any index; only the primary key
                        // needs its own clause.
                        constraint: primary,
                    });
                    usable.push(unique);
                    indexes.len() - 1
                }
            };
            match column {
                Some(column) => {
                    usable[position] &= not_null;
                    indexes[position].columns.push(column);
                }
                None => {
                    usable[position] = false;
                    indexes[position].columns.push("(expression)".to_string());
                }
            }
        }
        if let Some(primary) = indexes.iter().find(|i| i.primary) {
            for (n, key) in primary.columns.iter().enumerate() {
                if let Some(column) = columns.iter_mut().find(|c| &c.name == key) {
                    column.primary_key = u16::try_from(n).ok();
                }
            }
        }
        let tie_break = indexes
            .iter()
            .zip(&usable)
            .find(|(index, usable)| index.primary || **usable)
            .map(|(index, _)| index.columns.clone())
            .unwrap_or_default();
        Ok(TableInfo {
            db,
            name: object.name.clone(),
            is_view,
            columns,
            indexes,
            tie_break,
            estimated_rows: if is_view {
                None
            } else {
                table.and_then(|r| number(r.as_ref(1)))
            },
            size_bytes: table.and_then(|r| number(r.as_ref(2))),
            comment: table
                .and_then(|r| text(r.as_ref(3)))
                .filter(|c| !c.is_empty() && c != "VIEW"),
        })
    }

    pub fn table(&self) -> String {
        DIALECT.qualified(&[&self.db], &self.name)
    }

    pub fn nullable(&self, column: &str) -> bool {
        self.columns
            .iter()
            .find(|c| c.name == column)
            .is_none_or(|c| c.nullable)
    }
}

pub(super) async fn describe(conn: &mut Conn, object: &ObjectRef) -> Result<ObjectInfo> {
    let db = database_of(object)?;
    let routine = match object.kind {
        ObjectKind::Function => Some("FUNCTION"),
        ObjectKind::Procedure => Some("PROCEDURE"),
        ObjectKind::Sequence => Some("SEQUENCE"),
        _ => None,
    };
    if let Some(keyword) = routine {
        let statement = format!(
            "SHOW CREATE {keyword} {}",
            DIALECT.qualified(&[db], &object.name)
        );
        let row: Option<mysql_async::Row> = conn.query_first(statement).await.map_err(my_error)?;
        // SHOW CREATE FUNCTION puts the statement third, SHOW CREATE SEQUENCE second.
        let index = if keyword == "SEQUENCE" { 1 } else { 2 };
        return Ok(ObjectInfo {
            definition: row
                .and_then(|r| text(r.as_ref(index)))
                .map(|d| format!("{d};")),
            ..Default::default()
        });
    }

    let info = TableInfo::load(conn, object).await?;
    let fk_rows = rows(
        conn,
        "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, \
                k.REFERENCED_COLUMN_NAME, r.DELETE_RULE, r.UPDATE_RULE \
         FROM information_schema.KEY_COLUMN_USAGE k \
         LEFT JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
           ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND r.TABLE_NAME = k.TABLE_NAME \
          AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
         WHERE k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ? AND k.REFERENCED_TABLE_NAME IS NOT NULL \
         ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
        &[Value::text(db), Value::text(object.name.as_str())],
    )
    .await?;
    let mut foreign_keys: Vec<ForeignKey> = Vec::new();
    for row in fk_rows {
        let name = text(row.as_ref(0)).unwrap_or_default();
        let column = text(row.as_ref(1)).unwrap_or_default();
        let target_column = text(row.as_ref(4)).unwrap_or_default();
        match foreign_keys.iter_mut().find(|fk| fk.name == name) {
            Some(fk) => {
                fk.columns.push(column);
                fk.target_columns.push(target_column);
            }
            None => foreign_keys.push(ForeignKey {
                name,
                columns: vec![column],
                target: ObjectRef {
                    namespace: vec![text(row.as_ref(2)).unwrap_or_else(|| db.to_string())],
                    name: text(row.as_ref(3)).unwrap_or_default(),
                    kind: ObjectKind::Table,
                },
                target_columns: vec![target_column],
                on_delete: ForeignKeyAction::parse(&text(row.as_ref(5)).unwrap_or_default()),
                on_update: ForeignKeyAction::parse(&text(row.as_ref(6)).unwrap_or_default()),
            }),
        }
    }

    // The keys of other tables that point here, with this table's columns first.
    let incoming_rows = rows(
        conn,
        "SELECT CONSTRAINT_NAME, REFERENCED_COLUMN_NAME, TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME \
         FROM information_schema.KEY_COLUMN_USAGE \
         WHERE REFERENCED_TABLE_SCHEMA = ? AND REFERENCED_TABLE_NAME = ? \
         ORDER BY TABLE_SCHEMA, TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION",
        &[Value::text(db), Value::text(object.name.as_str())],
    )
    .await?;
    let mut referenced_by: Vec<ForeignKey> = Vec::new();
    for row in incoming_rows {
        let name = text(row.as_ref(0)).unwrap_or_default();
        let column = text(row.as_ref(1)).unwrap_or_default();
        let schema = text(row.as_ref(2)).unwrap_or_else(|| db.to_string());
        let table = text(row.as_ref(3)).unwrap_or_default();
        let source_column = text(row.as_ref(4)).unwrap_or_default();
        match referenced_by.iter_mut().find(|fk| {
            fk.name == name
                && fk.target.name == table
                && fk.target.namespace.first() == Some(&schema)
        }) {
            Some(fk) => {
                fk.columns.push(column);
                fk.target_columns.push(source_column);
            }
            None => referenced_by.push(ForeignKey {
                name,
                columns: vec![column],
                target: ObjectRef {
                    namespace: vec![schema],
                    name: table,
                    kind: ObjectKind::Table,
                },
                target_columns: vec![source_column],
                on_delete: ForeignKeyAction::default(),
                on_update: ForeignKeyAction::default(),
            }),
        }
    }

    let statement = format!(
        "SHOW CREATE {} {}",
        if info.is_view { "VIEW" } else { "TABLE" },
        info.table()
    );
    let definition = match conn.query_first::<mysql_async::Row, _>(statement).await {
        Ok(row) => row.and_then(|r| text(r.as_ref(1))).map(|d| format!("{d};")),
        Err(e) => {
            tracing::debug!("could not read the definition: {e}");
            None
        }
    };
    Ok(ObjectInfo {
        columns: info.columns.iter().map(MyColumn::column).collect(),
        indexes: info.indexes.clone(),
        foreign_keys,
        referenced_by,
        estimated_rows: info.estimated_rows,
        size_bytes: info.size_bytes,
        comment: info.comment.clone(),
        definition,
    })
}

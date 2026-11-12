//! The PostgreSQL catalog: databases, schemas, objects and their structure.

use std::fmt::Write;

use deltos_data::{
    Column, Error, ForeignKey, ForeignKeyAction, Index, Namespace, NamespaceKind, ObjectEntry,
    ObjectInfo, ObjectKind, ObjectRef, Result, Value, ValueKind,
};

use super::decode::Decoder;
use super::{DIALECT, Session, pg_error};

pub(super) async fn namespaces(session: &Session, current: &str) -> Result<Vec<Namespace>> {
    let mut out = Vec::new();
    let databases = session
        .query(
            "SELECT datname::text FROM pg_catalog.pg_database WHERE datallowconn AND NOT datistemplate ORDER BY datname",
            &[],
        )
        .await?;
    for row in databases {
        out.push(Namespace {
            path: vec![row.try_get(0).map_err(pg_error)?],
            kind: NamespaceKind::Database,
        });
    }
    // System schemas go last; the TOAST and temporary ones are never browsed.
    let schemas = session
        .query(
            "SELECT nspname::text FROM pg_catalog.pg_namespace \
             WHERE nspname NOT LIKE 'pg!_toast%' ESCAPE '!' AND nspname NOT LIKE 'pg!_temp!_%' ESCAPE '!' \
             ORDER BY nspname IN ('pg_catalog', 'information_schema'), nspname",
            &[],
        )
        .await?;
    for row in schemas {
        out.push(Namespace {
            path: vec![current.to_string(), row.try_get(0).map_err(pg_error)?],
            kind: NamespaceKind::Schema,
        });
    }
    Ok(out)
}

pub(super) async fn objects(session: &Session, db: &str, schema: &str) -> Result<Vec<ObjectEntry>> {
    let namespace = vec![db.to_string(), schema.to_string()];
    let rows = session
        .query(
            "SELECT c.relname::text, c.relkind::text, c.reltuples::float8, \
                    CASE WHEN c.relkind IN ('r', 'm', 't') THEN pg_catalog.pg_total_relation_size(c.oid) END, \
                    pg_catalog.obj_description(c.oid, 'pg_class') \
             FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f') \
             ORDER BY c.relname",
            &[Value::text(schema)],
        )
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let name: String = row.try_get(0).map_err(pg_error)?;
        let relkind: String = row.try_get(1).map_err(pg_error)?;
        let tuples: Option<f64> = row.try_get(2).map_err(pg_error)?;
        let size: Option<i64> = row.try_get(3).map_err(pg_error)?;
        let kind = match relkind.as_str() {
            "v" => ObjectKind::View,
            "m" => ObjectKind::MaterializedView,
            "S" => ObjectKind::Sequence,
            _ => ObjectKind::Table,
        };
        out.push(ObjectEntry {
            object: ObjectRef {
                namespace: namespace.clone(),
                name,
                kind,
            },
            estimated_rows: tuples.filter(|t| *t >= 0.0).map(|t| t as u64),
            size_bytes: size.and_then(|s| u64::try_from(s).ok()),
            comment: row.try_get(4).map_err(pg_error)?,
        });
    }
    // Functions are cheap to list from pg_proc. prokind needs PostgreSQL 11; on older
    // servers the sidebar simply shows no functions.
    let functions = session
        .query(
            "SELECT DISTINCT p.proname::text, p.prokind::text FROM pg_catalog.pg_proc p \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = $1 AND p.prokind IN ('f', 'p') ORDER BY 1",
            &[Value::text(schema)],
        )
        .await;
    match functions {
        Ok(rows) => {
            for row in rows {
                let name: String = row.try_get(0).map_err(pg_error)?;
                let kind: String = row.try_get(1).map_err(pg_error)?;
                out.push(ObjectEntry {
                    object: ObjectRef {
                        namespace: namespace.clone(),
                        name,
                        kind: if kind == "p" {
                            ObjectKind::Procedure
                        } else {
                            ObjectKind::Function
                        },
                    },
                    estimated_rows: None,
                    size_bytes: None,
                    comment: None,
                });
            }
        }
        Err(e) => tracing::debug!("could not list functions: {e}"),
    }
    Ok(out)
}

/// One column as the catalog describes it.
pub(super) struct PgColumn {
    pub name: String,
    pub type_name: String,
    pub not_null: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
    pub primary_key: Option<u16>,
    /// Selected as `::text` because no decoder here reads its binary form.
    pub text_cast: bool,
    pub kind: ValueKind,
}

impl PgColumn {
    pub fn column(&self) -> Column {
        Column {
            name: self.name.clone(),
            type_name: self.type_name.clone(),
            kind: self.kind,
            nullable: Some(!self.not_null),
            primary_key: self.primary_key,
            default: self.default.clone(),
            comment: self.comment.clone(),
        }
    }
}

/// What browsing and describing need to know about a relation.
pub(super) struct TableInfo {
    pub relkind: String,
    pub columns: Vec<PgColumn>,
    pub tie_break: Vec<String>,
    /// System columns selected after the visible ones to make the order total.
    pub hidden: Vec<String>,
    pub estimated_rows: Option<u64>,
    pub size_bytes: Option<u64>,
    pub comment: Option<String>,
}

impl TableInfo {
    pub async fn load(session: &Session, schema: &str, table: &str) -> Result<TableInfo> {
        let rows = session
            .query(
                "SELECT c.relkind::text, c.reltuples::float8, \
                        CASE WHEN c.relkind IN ('r', 'm') THEN pg_catalog.pg_total_relation_size(c.oid) END, \
                        pg_catalog.obj_description(c.oid, 'pg_class'), \
                        a.attname::text, a.atttypid, pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                        t.typtype::text, t.typname::text, t.typbasetype, t.typcategory::text, \
                        et.oid, et.typtype::text, et.typname::text, \
                        pg_catalog.pg_get_expr(d.adbin, d.adrelid), pg_catalog.col_description(c.oid, a.attnum), \
                        (SELECT pg_catalog.array_position(i.indkey::int2[], a.attnum) - pg_catalog.array_lower(i.indkey::int2[], 1) \
                          FROM pg_catalog.pg_index i WHERE i.indrelid = c.oid AND i.indisprimary) \
                 FROM pg_catalog.pg_class c \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped \
                 LEFT JOIN pg_catalog.pg_type t ON t.oid = a.atttypid \
                 LEFT JOIN pg_catalog.pg_type et ON et.oid = t.typelem AND t.typcategory = 'A' \
                 LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = c.oid AND d.adnum = a.attnum \
                 WHERE n.nspname = $1 AND c.relname = $2 \
                 ORDER BY a.attnum",
                &[Value::text(schema), Value::text(table)],
            )
            .await?;
        let Some(first) = rows.first() else {
            return Err(Error::Query(format!(
                "There is no table or view named {table} in {schema}."
            )));
        };
        let relkind: String = first.try_get(0).map_err(pg_error)?;
        let tuples: Option<f64> = first.try_get(1).map_err(pg_error)?;
        let size: Option<i64> = first.try_get(2).map_err(pg_error)?;
        let comment: Option<String> = first.try_get(3).map_err(pg_error)?;
        let mut columns = Vec::with_capacity(rows.len());
        for row in &rows {
            let Some(name) = row.try_get::<_, Option<String>>(4).map_err(pg_error)? else {
                continue;
            };
            let oid: u32 = row.try_get(5).map_err(pg_error)?;
            let typtype: String = row.try_get(8).map_err(pg_error)?;
            let typname: String = row.try_get(9).map_err(pg_error)?;
            let base: u32 = row.try_get(10).map_err(pg_error)?;
            let category: String = row.try_get(11).map_err(pg_error)?;
            let element: Option<u32> = row.try_get(12).map_err(pg_error)?;
            let element_typtype: Option<String> = row.try_get(13).map_err(pg_error)?;
            let element_name: Option<String> = row.try_get(14).map_err(pg_error)?;
            let primary: Option<i32> = row.try_get(17).map_err(pg_error)?;
            let decoder = column_decoder(
                oid,
                &typtype,
                &typname,
                base,
                element.map(|oid| {
                    (
                        oid,
                        element_typtype.as_deref().unwrap_or(""),
                        element_name.as_deref().unwrap_or(""),
                    )
                }),
            );
            let text_cast = decoder.is_none();
            let kind = match &decoder {
                Some(decoder) => decoder.kind(),
                None if category == "S" => ValueKind::Text,
                None => ValueKind::Other,
            };
            columns.push(PgColumn {
                name,
                type_name: row.try_get(6).map_err(pg_error)?,
                not_null: row.try_get(7).map_err(pg_error)?,
                default: row.try_get(15).map_err(pg_error)?,
                comment: row.try_get(16).map_err(pg_error)?,
                primary_key: primary.and_then(|p| u16::try_from(p).ok()),
                text_cast,
                kind,
            });
        }
        let mut key: Vec<&PgColumn> = columns.iter().filter(|c| c.primary_key.is_some()).collect();
        key.sort_by_key(|c| c.primary_key);
        let mut tie_break: Vec<String> = key.iter().map(|c| c.name.clone()).collect();
        let mut hidden = Vec::new();
        if tie_break.is_empty() {
            // Without a key, the physical row address tells rows apart. A partitioned
            // table repeats addresses across partitions, so the partition comes first.
            match relkind.as_str() {
                "r" | "m" => hidden.push("ctid".to_string()),
                "p" => hidden.extend(["tableoid".to_string(), "ctid".to_string()]),
                _ => {}
            }
            tie_break = hidden.clone();
        }
        Ok(TableInfo {
            relkind,
            columns,
            tie_break,
            hidden,
            estimated_rows: tuples.filter(|t| *t >= 0.0).map(|t| t as u64),
            size_bytes: size.and_then(|s| u64::try_from(s).ok()),
            comment,
        })
    }

    pub fn nullable(&self, column: &str) -> bool {
        if self.hidden.iter().any(|h| h == column) {
            return false;
        }
        self.columns
            .iter()
            .find(|c| c.name == column)
            .is_none_or(|c| !c.not_null)
    }
}

/// The decoder for a column's type, when its binary form is one we read exactly.
fn column_decoder(
    oid: u32,
    typtype: &str,
    typname: &str,
    base: u32,
    element: Option<(u32, &str, &str)>,
) -> Option<Decoder> {
    if let Some(decoder) = Decoder::for_oid(oid) {
        return Some(decoder);
    }
    if let Some(decoder) = Decoder::for_extension(typname) {
        return Some(decoder);
    }
    match typtype {
        "e" => Some(Decoder::Text),
        "d" => Decoder::for_oid(base),
        _ => {
            let (element_oid, element_typtype, element_name) = element?;
            let inner = Decoder::for_oid(element_oid)
                .or_else(|| Decoder::for_extension(element_name))
                .or_else(|| (element_typtype == "e").then_some(Decoder::Text))?;
            Some(Decoder::Array(Box::new(inner)))
        }
    }
}

pub(super) async fn describe(
    session: &Session,
    db: &str,
    schema: &str,
    object: &ObjectRef,
) -> Result<ObjectInfo> {
    match object.kind {
        ObjectKind::Function | ObjectKind::Procedure => {
            return Ok(ObjectInfo {
                definition: function_definition(session, schema, &object.name).await?,
                ..Default::default()
            });
        }
        ObjectKind::Sequence => {
            return Ok(ObjectInfo {
                definition: sequence_definition(session, schema, &object.name).await?,
                ..Default::default()
            });
        }
        _ => {}
    }
    let info = TableInfo::load(session, schema, &object.name).await?;
    let params = [Value::text(schema), Value::text(object.name.as_str())];
    let qualified = DIALECT.qualified(&[schema], &object.name);

    let index_rows = session
        .query(
            "SELECT ic.relname::text, ix.indisunique, ix.indisprimary, am.amname::text, \
                    ARRAY(SELECT pg_catalog.pg_get_indexdef(ix.indexrelid, k, true) \
                          FROM pg_catalog.generate_series(1, ix.indnkeyatts) AS k ORDER BY k), \
                    pg_catalog.pg_get_indexdef(ix.indexrelid), \
                    EXISTS (SELECT 1 FROM pg_catalog.pg_constraint con WHERE con.conindid = ix.indexrelid) \
             FROM pg_catalog.pg_index ix \
             JOIN pg_catalog.pg_class ic ON ic.oid = ix.indexrelid \
             JOIN pg_catalog.pg_am am ON am.oid = ic.relam \
             JOIN pg_catalog.pg_class c ON c.oid = ix.indrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 \
             ORDER BY ix.indisprimary DESC, ic.relname",
            &params,
        )
        .await?;
    let mut indexes = Vec::new();
    let mut index_statements = Vec::new();
    for row in index_rows {
        let backs_constraint: bool = row.try_get(6).map_err(pg_error)?;
        indexes.push(Index {
            name: row.try_get(0).map_err(pg_error)?,
            unique: row.try_get(1).map_err(pg_error)?,
            primary: row.try_get(2).map_err(pg_error)?,
            method: row.try_get(3).map_err(pg_error)?,
            columns: row.try_get(4).map_err(pg_error)?,
            constraint: backs_constraint,
        });
        if !backs_constraint {
            index_statements.push(row.try_get::<_, String>(5).map_err(pg_error)?);
        }
    }

    let fk_rows = session
        .query(
            "SELECT con.conname::text, \
                    ARRAY(SELECT a.attname::text FROM unnest(con.conkey) WITH ORDINALITY AS k(n, i) \
                          JOIN pg_catalog.pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.n ORDER BY k.i), \
                    tn.nspname::text, tc.relname::text, \
                    ARRAY(SELECT a.attname::text FROM unnest(con.confkey) WITH ORDINALITY AS k(n, i) \
                          JOIN pg_catalog.pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.n ORDER BY k.i), \
                    con.confdeltype::text, con.confupdtype::text \
             FROM pg_catalog.pg_constraint con \
             JOIN pg_catalog.pg_class c ON c.oid = con.conrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_class tc ON tc.oid = con.confrelid \
             JOIN pg_catalog.pg_namespace tn ON tn.oid = tc.relnamespace \
             WHERE con.contype = 'f' AND n.nspname = $1 AND c.relname = $2 \
             ORDER BY con.conname",
            &params,
        )
        .await?;
    let mut foreign_keys = Vec::new();
    for row in fk_rows {
        foreign_keys.push(ForeignKey {
            name: row.try_get(0).map_err(pg_error)?,
            columns: row.try_get(1).map_err(pg_error)?,
            target: ObjectRef {
                namespace: vec![db.to_string(), row.try_get(2).map_err(pg_error)?],
                name: row.try_get(3).map_err(pg_error)?,
                kind: ObjectKind::Table,
            },
            target_columns: row.try_get(4).map_err(pg_error)?,
            on_delete: fk_action(&row.try_get::<_, String>(5).map_err(pg_error)?),
            on_update: fk_action(&row.try_get::<_, String>(6).map_err(pg_error)?),
        });
    }

    let definition = match info.relkind.as_str() {
        "v" | "m" => {
            let rows = session
                .query(
                    "SELECT pg_catalog.pg_get_viewdef(c.oid, true) FROM pg_catalog.pg_class c \
                     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1 AND c.relname = $2",
                    &params,
                )
                .await?;
            let body: Option<String> = rows.first().and_then(|r| r.try_get(0).ok());
            body.map(|body| {
                let kind = if info.relkind == "m" {
                    "MATERIALIZED VIEW"
                } else {
                    "VIEW"
                };
                let mut statement = format!("CREATE {kind} {qualified} AS\n{}", body.trim_end());
                if !statement.ends_with(';') {
                    statement.push(';');
                }
                for index in &index_statements {
                    let _ = write!(statement, "\n\n{index};");
                }
                statement
            })
        }
        _ => Some(table_definition(session, &qualified, &info, &params, &index_statements).await?),
    };

    Ok(ObjectInfo {
        columns: info.columns.iter().map(|c| c.column()).collect(),
        indexes,
        foreign_keys,
        estimated_rows: info.estimated_rows,
        size_bytes: info.size_bytes,
        comment: info.comment.clone(),
        definition,
    })
}

/// A foreign key action from its `pg_constraint` code.
fn fk_action(code: &str) -> ForeignKeyAction {
    match code {
        "r" => ForeignKeyAction::Restrict,
        "c" => ForeignKeyAction::Cascade,
        "n" => ForeignKeyAction::SetNull,
        "d" => ForeignKeyAction::SetDefault,
        _ => ForeignKeyAction::NoAction,
    }
}

/// `CREATE TABLE` with columns, constraints, indexes and comments. It covers what the
/// structure tab shows; partitioning, inheritance and storage options are left out.
async fn table_definition(
    session: &Session,
    qualified: &str,
    info: &TableInfo,
    params: &[Value],
    index_statements: &[String],
) -> Result<String> {
    let constraints = session
        .query(
            "SELECT con.conname::text, pg_catalog.pg_get_constraintdef(con.oid, true) \
             FROM pg_catalog.pg_constraint con \
             JOIN pg_catalog.pg_class c ON c.oid = con.conrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND con.contype IN ('p', 'u', 'c', 'f', 'x') \
             ORDER BY CASE con.contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'c' THEN 2 ELSE 3 END, con.conname",
            params,
        )
        .await?;
    let mut lines: Vec<String> = Vec::new();
    for column in &info.columns {
        let mut line = format!(
            "    {} {}",
            DIALECT.quote_ident(&column.name),
            column.type_name
        );
        if let Some(default) = &column.default {
            let _ = write!(line, " DEFAULT {default}");
        }
        if column.not_null {
            line.push_str(" NOT NULL");
        }
        lines.push(line);
    }
    for row in constraints {
        let name: String = row.try_get(0).map_err(pg_error)?;
        let definition: String = row.try_get(1).map_err(pg_error)?;
        lines.push(format!(
            "    CONSTRAINT {} {definition}",
            DIALECT.quote_ident(&name)
        ));
    }
    let keyword = if info.relkind == "f" {
        "CREATE FOREIGN TABLE"
    } else {
        "CREATE TABLE"
    };
    let mut out = format!("{keyword} {qualified} (\n{}\n);", lines.join(",\n"));
    for index in index_statements {
        let _ = write!(out, "\n\n{index};");
    }
    let mut literal = String::new();
    if let Some(comment) = &info.comment {
        DIALECT.push_string_literal(&mut literal, comment);
        let _ = write!(out, "\n\nCOMMENT ON TABLE {qualified} IS {literal};");
    }
    for column in &info.columns {
        if let Some(comment) = &column.comment {
            literal.clear();
            DIALECT.push_string_literal(&mut literal, comment);
            let _ = write!(
                out,
                "\n\nCOMMENT ON COLUMN {qualified}.{} IS {literal};",
                DIALECT.quote_ident(&column.name)
            );
        }
    }
    Ok(out)
}

async fn function_definition(
    session: &Session,
    schema: &str,
    name: &str,
) -> Result<Option<String>> {
    let rows = session
        .query(
            "SELECT pg_catalog.pg_get_functiondef(p.oid) FROM pg_catalog.pg_proc p \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = $1 AND p.proname = $2 AND p.prokind IN ('f', 'p') ORDER BY p.oid",
            &[Value::text(schema), Value::text(name)],
        )
        .await?;
    let bodies: Vec<String> = rows
        .iter()
        .filter_map(|row| row.try_get::<_, String>(0).ok())
        .map(|body| format!("{};", body.trim_end()))
        .collect();
    Ok((!bodies.is_empty()).then(|| bodies.join("\n\n")))
}

async fn sequence_definition(
    session: &Session,
    schema: &str,
    name: &str,
) -> Result<Option<String>> {
    let rows = session
        .query(
            "SELECT pg_catalog.format_type(s.seqtypid, NULL), s.seqstart, s.seqincrement, s.seqmin, s.seqmax, s.seqcache, s.seqcycle \
             FROM pg_catalog.pg_sequence s JOIN pg_catalog.pg_class c ON c.oid = s.seqrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2",
            &[Value::text(schema), Value::text(name)],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let ty: String = row.try_get(0).map_err(pg_error)?;
    let numbers: Vec<i64> = (1..=5)
        .map(|i| row.try_get::<_, i64>(i))
        .collect::<std::result::Result<_, _>>()
        .map_err(pg_error)?;
    let cycle: bool = row.try_get(6).map_err(pg_error)?;
    Ok(Some(format!(
        "CREATE SEQUENCE {} AS {ty} START {} INCREMENT {} MINVALUE {} MAXVALUE {} CACHE {}{};",
        DIALECT.qualified(&[schema], name),
        numbers[0],
        numbers[1],
        numbers[2],
        numbers[3],
        numbers[4],
        if cycle { " CYCLE" } else { "" }
    )))
}

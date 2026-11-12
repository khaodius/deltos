//! Statements that change the structure of tables, written for each database.
//!
//! The structure editor starts from a [`TableDef`] read from an [`ObjectInfo`], stages the
//! person's edits as a [`Draft`] whose entries remember which original column, index or
//! foreign key they came from, and asks [`plan`] for the statements that turn one into the
//! other. [`create_table`], [`drop_table`] and [`truncate_table`] cover whole tables.
//! Nothing here talks to a database; the statements run through
//! [`Connection::apply_schema`](deltos_data::Connection::apply_schema).
//!
//! Statements come in an order where each finds what it needs: foreign keys are dropped
//! first, then indexes, then columns; columns are renamed, then changed, then added; new
//! indexes and foreign keys come last.
//!
//! - PostgreSQL gets one `ALTER TABLE` per change, `COMMENT ON COLUMN` for comments and
//!   `CREATE INDEX`/`DROP INDEX`. A type change converts the old values with `USING`.
//! - MySQL and MariaDB restate the whole column with `MODIFY COLUMN`, or `CHANGE COLUMN`
//!   when it is renamed. Clauses the editor does not show (`AUTO_INCREMENT`, `ON UPDATE`,
//!   the character set and collation) are kept as `SHOW CREATE TABLE` printed them.
//! - SQLite changes in place what its `ALTER TABLE` can: add, rename and drop columns
//!   (dropping needs 3.35), and create and drop indexes. Anything else rebuilds the table
//!   the way SQLite documents: create the new shape under a temporary name, copy the rows,
//!   drop the old table, rename the new one, then recreate its indexes and triggers. A
//!   table with clauses the rebuild could not carry over (CHECK, COLLATE, generated
//!   columns, WITHOUT ROWID, STRICT and the like) is refused with a note instead.
//! - MongoDB has no columns to change, only indexes, through the `createIndexes` and
//!   `dropIndexes` commands.

use std::ops::Range;

use deltos_data::{DriverKind, ForeignKeyAction, ObjectInfo, ObjectKind, ObjectRef};

use crate::sql::{Dialect, split_statements};

/// The name a SQLite rebuild gives the new table until it replaces the old one.
pub const REBUILD_PREFIX: &str = "_deltos_new_";

/// The database a statement is written for, with what its version allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Postgres,
    Mysql,
    /// `drop_column`: `ALTER TABLE ... DROP COLUMN` exists (SQLite 3.35 and later).
    Sqlite {
        drop_column: bool,
    },
    Mongo,
}

impl Flavor {
    /// The flavor for a connection, from its kind and the version it reports.
    pub fn of(kind: DriverKind, server_version: &str) -> Option<Flavor> {
        match kind {
            DriverKind::Postgres => Some(Flavor::Postgres),
            DriverKind::Mysql => Some(Flavor::Mysql),
            DriverKind::Sqlite => Some(Flavor::Sqlite {
                drop_column: version_of(server_version).is_none_or(|v| v >= (3, 35)),
            }),
            DriverKind::Mongodb => Some(Flavor::Mongo),
            _ => None,
        }
    }

    /// Columns can be added and changed. MongoDB has none to change.
    pub fn columns(self) -> bool {
        self != Flavor::Mongo
    }

    pub fn comments(self) -> bool {
        matches!(self, Flavor::Postgres | Flavor::Mysql)
    }

    pub fn foreign_keys(self) -> bool {
        self != Flavor::Mongo
    }

    /// Foreign keys have names the database keeps. SQLite does not report them.
    pub fn named_foreign_keys(self) -> bool {
        matches!(self, Flavor::Postgres | Flavor::Mysql)
    }

    /// A failure part way through leaves nothing changed.
    pub fn transactional(self) -> bool {
        matches!(self, Flavor::Postgres | Flavor::Sqlite { .. })
    }

    /// The type a new column starts with.
    pub fn default_type(self) -> &'static str {
        match self {
            Flavor::Postgres => "text",
            Flavor::Mysql => "varchar(255)",
            Flavor::Sqlite { .. } => "TEXT",
            Flavor::Mongo => "",
        }
    }

    /// Common types, offered while typing one.
    pub fn types(self) -> &'static [&'static str] {
        match self {
            Flavor::Postgres => &[
                "bigint",
                "bigserial",
                "boolean",
                "bytea",
                "date",
                "double precision",
                "integer",
                "interval",
                "jsonb",
                "numeric(10,2)",
                "real",
                "serial",
                "smallint",
                "text",
                "time",
                "timestamp",
                "timestamptz",
                "uuid",
                "varchar(255)",
            ],
            Flavor::Mysql => &[
                "bigint",
                "bigint unsigned",
                "blob",
                "char(36)",
                "date",
                "datetime",
                "decimal(10,2)",
                "double",
                "int",
                "json",
                "longtext",
                "smallint",
                "text",
                "time",
                "timestamp",
                "tinyint(1)",
                "varchar(255)",
            ],
            Flavor::Sqlite { .. } => &["INTEGER", "REAL", "TEXT", "BLOB", "NUMERIC"],
            Flavor::Mongo => &[],
        }
    }

    fn dialect(self) -> Dialect {
        match self {
            Flavor::Postgres => Dialect::Postgres,
            Flavor::Mysql => Dialect::Mysql,
            Flavor::Sqlite { .. } | Flavor::Mongo => Dialect::Sqlite,
        }
    }

    /// Two names that the database would take as the same column or index.
    fn same_name(self, a: &str, b: &str) -> bool {
        match self {
            Flavor::Postgres | Flavor::Mongo => a == b,
            Flavor::Mysql | Flavor::Sqlite { .. } => a.eq_ignore_ascii_case(b),
        }
    }
}

/// The first `major.minor` in a version banner such as `SQLite 3.45.1`.
fn version_of(text: &str) -> Option<(u32, u32)> {
    text.split(|c: char| !c.is_ascii_digit() && c != '.')
        .find_map(|part| {
            let mut numbers = part.split('.');
            let major = numbers.next()?.parse().ok()?;
            let minor = numbers.next()?.parse().ok()?;
            Some((major, minor))
        })
}

/// The leading parts a statement qualifies a table of `namespace` with: the schema for
/// PostgreSQL, the database for MySQL, an attached schema for SQLite (`main` needs
/// none), and nothing for MongoDB, whose database comes with the statement.
pub fn qualifier(flavor: Flavor, namespace: &[String]) -> Vec<String> {
    match flavor {
        Flavor::Postgres => namespace.get(1).or(namespace.first()).cloned(),
        Flavor::Mysql => namespace.first().cloned(),
        Flavor::Sqlite { .. } => namespace.first().filter(|s| s.as_str() != "main").cloned(),
        Flavor::Mongo => None,
    }
    .into_iter()
    .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColumnDef {
    pub name: String,
    /// The type as SQL writes it, `varchar(255)` or `timestamp with time zone`.
    pub type_name: String,
    pub nullable: bool,
    /// An SQL expression: `0`, `'open'`, `now()`.
    pub default: Option<String>,
    pub comment: Option<String>,
    /// Position in the primary key, from 0.
    pub primary_key: Option<u16>,
    /// MySQL: `CHARACTER SET ... COLLATE ...` as the server printed it. Kept while the
    /// type stays the same.
    pub collation: Option<String>,
    /// MySQL: clauses after the default, such as `AUTO_INCREMENT` or
    /// `ON UPDATE CURRENT_TIMESTAMP`, kept as they are.
    pub extra: Option<String>,
    /// The column is computed from others, which only SQL can change.
    pub generated: bool,
}

impl ColumnDef {
    pub fn new(name: impl Into<String>, type_name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            type_name: type_name.into(),
            nullable: true,
            ..Default::default()
        }
    }

    /// Whether `self` differs from `old` in what the editor changes, apart from the name.
    fn changed_from(&self, old: &ColumnDef) -> bool {
        self.type_name.trim() != old.type_name.trim()
            || self.nullable != old.nullable
            || default_of(self) != default_of(old)
            || comment_of(self) != comment_of(old)
    }
}

fn default_of(c: &ColumnDef) -> Option<&str> {
    c.default
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
}

fn comment_of(c: &ColumnDef) -> Option<&str> {
    c.comment.as_deref().filter(|d| !d.is_empty())
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexDef {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    /// The index enforces a constraint and is dropped by dropping that.
    pub constraint: bool,
    /// SQLite: the `CREATE INDEX` statement it was made with, to recreate it after a
    /// rebuild with everything it said (a `WHERE`, expressions, collations).
    pub sql: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyDef {
    pub name: String,
    pub columns: Vec<String>,
    pub target: ObjectRef,
    /// Empty in SQLite when the key refers to the target's primary key.
    pub target_columns: Vec<String>,
    pub on_delete: ForeignKeyAction,
    pub on_update: ForeignKeyAction,
}

/// SQLite facts a rebuild has to respect.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rebuild {
    /// What a rebuild would lose, when the table has something it cannot carry over.
    pub blocker: Option<String>,
    /// The primary key is `AUTOINCREMENT`.
    pub autoincrement: bool,
    /// `CREATE TRIGGER` statements to run again after the rebuild.
    pub triggers: Vec<String>,
}

/// A table's structure as the editor works with it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableDef {
    /// See [`qualifier`].
    pub qualifier: Vec<String>,
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub indexes: Vec<IndexDef>,
    pub foreign_keys: Vec<ForeignKeyDef>,
    pub rebuild: Rebuild,
}

impl TableDef {
    /// An empty table, for [`create_table`].
    pub fn new(flavor: Flavor, namespace: &[String], name: impl Into<String>) -> Self {
        Self {
            qualifier: qualifier(flavor, namespace),
            name: name.into(),
            ..Default::default()
        }
    }

    pub fn from_info(flavor: Flavor, object: &ObjectRef, info: &ObjectInfo) -> Self {
        let mut columns: Vec<ColumnDef> = info
            .columns
            .iter()
            .map(|c| ColumnDef {
                name: c.name.clone(),
                type_name: c.type_name.clone(),
                nullable: c.nullable.unwrap_or(true),
                default: c.default.clone(),
                comment: c.comment.clone().filter(|c| !c.is_empty()),
                primary_key: c.primary_key,
                ..Default::default()
            })
            .collect();
        let mut indexes: Vec<IndexDef> = info
            .indexes
            .iter()
            .map(|i| IndexDef {
                name: i.name.clone(),
                columns: i.columns.clone(),
                unique: i.unique,
                primary: i.primary,
                constraint: i.constraint || i.primary,
                sql: None,
            })
            .collect();
        let mut rebuild = Rebuild::default();
        match (flavor, info.definition.as_deref()) {
            (Flavor::Mysql, Some(definition)) => {
                for parsed in mysql_columns(definition) {
                    if let Some(c) = columns
                        .iter_mut()
                        .find(|c| c.name.eq_ignore_ascii_case(&parsed.name))
                    {
                        // SHOW CREATE TABLE writes defaults as SQL; the catalog does not.
                        c.default = parsed.default;
                        c.collation = parsed.collation;
                        c.extra = parsed.extra;
                        c.generated = parsed.generated;
                    }
                }
            }
            (Flavor::Sqlite { .. }, definition) => {
                rebuild = sqlite_rebuild(definition.unwrap_or(""), &mut indexes);
            }
            _ => {}
        }
        Self {
            qualifier: qualifier(flavor, &object.namespace),
            name: object.name.clone(),
            columns,
            indexes,
            foreign_keys: info
                .foreign_keys
                .iter()
                .map(|fk| ForeignKeyDef {
                    name: fk.name.clone(),
                    columns: fk.columns.clone(),
                    target: fk.target.clone(),
                    target_columns: fk.target_columns.clone(),
                    on_delete: fk.on_delete,
                    on_update: fk.on_update,
                })
                .collect(),
            rebuild,
        }
    }
}

/// An entry of a [`Draft`]: what it is now, and which original entry it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged<T> {
    /// Index into the original table's list; `None` for an entry added in the draft.
    pub from: Option<usize>,
    pub def: T,
}

/// The table as the person wants it. Originals left out are dropped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Draft {
    pub columns: Vec<Staged<ColumnDef>>,
    pub indexes: Vec<Staged<IndexDef>>,
    pub foreign_keys: Vec<Staged<ForeignKeyDef>>,
}

impl Draft {
    /// The table unchanged.
    pub fn of(table: &TableDef) -> Self {
        fn staged<T: Clone>(items: &[T]) -> Vec<Staged<T>> {
            items
                .iter()
                .enumerate()
                .map(|(i, def)| Staged {
                    from: Some(i),
                    def: def.clone(),
                })
                .collect()
        }
        Self {
            columns: staged(&table.columns),
            indexes: staged(&table.indexes),
            foreign_keys: staged(&table.foreign_keys),
        }
    }
}

/// The statements for a change, in the order they run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    pub statements: Vec<String>,
    /// Things the person should know before running them.
    pub notes: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }

    /// The statements as one script, each ended with a semicolon.
    pub fn script(&self) -> String {
        let mut out = String::new();
        for statement in &self.statements {
            out.push_str(statement);
            out.push_str(";\n");
        }
        out
    }
}

/// The statements that turn `table` into `draft`, or why they cannot be written.
pub fn plan(flavor: Flavor, table: &TableDef, draft: &Draft) -> Result<Plan, String> {
    let diff = Diff::new(table, draft)?;
    validate(flavor, draft)?;
    if flavor.columns() {
        check_primary_key(flavor, &diff)?;
    }
    if diff.dropped_indexes.iter().any(|i| i.primary) {
        return Err("The primary key cannot be dropped here. Use SQL for that.".into());
    }
    let mut plan = match flavor {
        Flavor::Postgres => postgres_plan(table, &diff),
        Flavor::Mysql => mysql_plan(table, &diff)?,
        Flavor::Sqlite { drop_column } => sqlite_plan(flavor, drop_column, table, &diff)?,
        Flavor::Mongo => mongo_plan(table, &diff)?,
    };
    if !plan.is_empty() && flavor == Flavor::Mysql && plan.statements.len() > 1 {
        plan.notes.push(
            "MySQL commits each statement as it runs. If one fails, the ones before it stay applied."
                .into(),
        );
    }
    Ok(plan)
}

/// `CREATE TABLE` for a new table, with its indexes and, in PostgreSQL, its comments.
pub fn create_table(flavor: Flavor, table: &TableDef) -> Result<Plan, String> {
    if !flavor.columns() {
        return Err("MongoDB creates a collection when the first document goes in.".into());
    }
    if table.name.trim().is_empty() {
        return Err("The table needs a name.".into());
    }
    let draft = Draft {
        columns: added(&table.columns),
        indexes: added(&table.indexes),
        foreign_keys: added(&table.foreign_keys),
    };
    validate(flavor, &draft)?;
    let d = flavor.dialect();
    let name = d.qualified(&table.qualifier, &table.name);
    let columns: Vec<&ColumnDef> = table.columns.iter().collect();
    let keys: Vec<&ForeignKeyDef> = table.foreign_keys.iter().collect();
    let mut statements = vec![format!(
        "CREATE TABLE {name} (\n{}\n)",
        table_body(flavor, &columns, &[], &keys, false)
    )];
    if flavor == Flavor::Postgres {
        for c in &table.columns {
            if let Some(comment) = comment_of(c) {
                statements.push(postgres_comment(&name, &c.name, Some(comment)));
            }
        }
    }
    for index in table.indexes.iter().filter(|i| !i.primary) {
        statements.push(create_index(flavor, table, index));
    }
    Ok(Plan {
        statements,
        notes: Vec::new(),
    })
}

fn added<T: Clone>(items: &[T]) -> Vec<Staged<T>> {
    items
        .iter()
        .map(|def| Staged {
            from: None,
            def: def.clone(),
        })
        .collect()
}

pub fn drop_table(flavor: Flavor, object: &ObjectRef) -> String {
    match flavor {
        Flavor::Mongo => format!("db.getCollection({}).drop()", json_string(&object.name)),
        _ => {
            let kind = match object.kind {
                ObjectKind::View => "VIEW",
                ObjectKind::MaterializedView => "MATERIALIZED VIEW",
                _ => "TABLE",
            };
            format!("DROP {kind} {}", table_name(flavor, object))
        }
    }
}

/// Removes every row. SQLite has no `TRUNCATE`; a `DELETE` without `WHERE` takes its
/// fast path.
pub fn truncate_table(flavor: Flavor, object: &ObjectRef) -> String {
    match flavor {
        Flavor::Mongo => format!(
            "db.getCollection({}).deleteMany({{}})",
            json_string(&object.name)
        ),
        Flavor::Sqlite { .. } => format!("DELETE FROM {}", table_name(flavor, object)),
        Flavor::Postgres | Flavor::Mysql => {
            format!("TRUNCATE TABLE {}", table_name(flavor, object))
        }
    }
}

fn table_name(flavor: Flavor, object: &ObjectRef) -> String {
    flavor
        .dialect()
        .qualified(&qualifier(flavor, &object.namespace), &object.name)
}

/// A name for a new index, in the style of the database's own.
pub fn index_name(flavor: Flavor, table: &str, columns: &[String], unique: bool) -> String {
    if flavor == Flavor::Mongo {
        return columns
            .iter()
            .map(|c| format!("{c}_1"))
            .collect::<Vec<_>>()
            .join("_");
    }
    let suffix = if unique { "key" } else { "idx" };
    format!("{table}_{}_{suffix}", columns.join("_"))
}

pub fn foreign_key_name(table: &str, columns: &[String]) -> String {
    format!("{table}_{}_fkey", columns.join("_"))
}

// ---------------------------------------------------------------------------------------
// What changed.

struct Diff<'a> {
    dropped_columns: Vec<&'a ColumnDef>,
    /// Old and new, in draft order.
    kept_columns: Vec<(&'a ColumnDef, &'a ColumnDef)>,
    added_columns: Vec<&'a ColumnDef>,
    dropped_indexes: Vec<&'a IndexDef>,
    kept_indexes: Vec<&'a IndexDef>,
    added_indexes: Vec<&'a IndexDef>,
    dropped_keys: Vec<&'a ForeignKeyDef>,
    kept_keys: Vec<&'a ForeignKeyDef>,
    added_keys: Vec<&'a ForeignKeyDef>,
    /// The new columns in draft order.
    columns: Vec<&'a ColumnDef>,
}

/// Splits staged entries into dropped, kept and added. A kept index or key whose
/// definition changed is dropped and added again.
#[allow(clippy::type_complexity)]
fn split<'a, T: PartialEq>(
    original: &'a [T],
    staged: &'a [Staged<T>],
    what: &str,
) -> Result<(Vec<&'a T>, Vec<&'a T>, Vec<&'a T>), String> {
    let mut seen = vec![false; original.len()];
    let (mut kept, mut added) = (Vec::new(), Vec::new());
    let mut dropped = Vec::new();
    for entry in staged {
        match entry.from.and_then(|i| original.get(i).map(|o| (i, o))) {
            Some((i, old)) => {
                if std::mem::replace(&mut seen[i], true) {
                    return Err(format!("The draft lists one {what} twice."));
                }
                if *old == entry.def {
                    kept.push(old);
                } else {
                    dropped.push(old);
                    added.push(&entry.def);
                }
            }
            None => added.push(&entry.def),
        }
    }
    let mut gone: Vec<&T> = original
        .iter()
        .zip(&seen)
        .filter(|(_, seen)| !**seen)
        .map(|(o, _)| o)
        .collect();
    gone.append(&mut dropped);
    Ok((gone, kept, added))
}

impl<'a> Diff<'a> {
    fn new(table: &'a TableDef, draft: &'a Draft) -> Result<Self, String> {
        let mut seen = vec![false; table.columns.len()];
        let mut kept_columns = Vec::new();
        let mut added_columns = Vec::new();
        for entry in &draft.columns {
            match entry
                .from
                .and_then(|i| table.columns.get(i).map(|o| (i, o)))
            {
                Some((i, old)) => {
                    if std::mem::replace(&mut seen[i], true) {
                        return Err("The draft lists one column twice.".into());
                    }
                    kept_columns.push((old, &entry.def));
                }
                None => added_columns.push(&entry.def),
            }
        }
        let dropped_columns = table
            .columns
            .iter()
            .zip(&seen)
            .filter(|(_, seen)| !**seen)
            .map(|(c, _)| c)
            .collect();
        let (dropped_indexes, kept_indexes, added_indexes) =
            split(&table.indexes, &draft.indexes, "index")?;
        let (dropped_keys, kept_keys, added_keys) =
            split(&table.foreign_keys, &draft.foreign_keys, "foreign key")?;
        Ok(Self {
            dropped_columns,
            kept_columns,
            added_columns,
            dropped_indexes,
            kept_indexes,
            added_indexes,
            dropped_keys,
            kept_keys,
            added_keys,
            columns: draft.columns.iter().map(|c| &c.def).collect(),
        })
    }

    fn renamed(&self) -> impl Iterator<Item = (&'a ColumnDef, &'a ColumnDef)> + '_ {
        self.kept_columns
            .iter()
            .copied()
            .filter(|(old, new)| old.name != new.name)
    }
}

/// Names, types and references that every database needs.
fn validate(flavor: Flavor, draft: &Draft) -> Result<(), String> {
    if flavor.columns() {
        if draft.columns.is_empty() {
            return Err("A table needs at least one column.".into());
        }
        for (i, c) in draft.columns.iter().map(|c| &c.def).enumerate() {
            if c.name.trim().is_empty() {
                return Err("Every column needs a name.".into());
            }
            if c.type_name.trim().is_empty() && !matches!(flavor, Flavor::Sqlite { .. }) {
                return Err(format!("Column {} needs a type.", c.name));
            }
            if draft.columns[..i]
                .iter()
                .any(|o| flavor.same_name(&o.def.name, &c.name))
            {
                return Err(format!("There are two columns named {}.", c.name));
            }
        }
    }
    let has_column = |name: &str| {
        !flavor.columns()
            || draft
                .columns
                .iter()
                .any(|c| flavor.same_name(&c.def.name, name))
    };
    for (i, index) in draft.indexes.iter().enumerate() {
        let index = &index.def;
        if index.name.trim().is_empty() {
            return Err("Every index needs a name.".into());
        }
        if index.columns.is_empty() {
            return Err(format!("Index {} needs at least one column.", index.name));
        }
        if draft.indexes[..i]
            .iter()
            .any(|o| flavor.same_name(&o.def.name, &index.name))
        {
            return Err(format!("There are two indexes named {}.", index.name));
        }
    }
    for staged in draft.indexes.iter().filter(|i| i.from.is_none()) {
        if let Some(missing) = staged.def.columns.iter().find(|c| !has_column(c)) {
            return Err(format!(
                "Index {} uses {missing}, which is not a column of the table.",
                staged.def.name
            ));
        }
    }
    for staged in draft.foreign_keys.iter().filter(|k| k.from.is_none()) {
        let key = &staged.def;
        let label = if key.name.is_empty() {
            "A foreign key".to_string()
        } else {
            format!("Foreign key {}", key.name)
        };
        if flavor.named_foreign_keys() && key.name.trim().is_empty() {
            return Err("Every foreign key needs a name.".into());
        }
        if key.columns.is_empty() {
            return Err(format!("{label} needs at least one column."));
        }
        if key.target.name.trim().is_empty() {
            return Err(format!("{label} needs a table to refer to."));
        }
        if key.target_columns.len() != key.columns.len() {
            return Err(format!(
                "{label} needs as many referenced columns as it has columns."
            ));
        }
        if let Some(missing) = key.columns.iter().find(|c| !has_column(c)) {
            return Err(format!(
                "{label} uses {missing}, which is not a column of the table."
            ));
        }
    }
    Ok(())
}

/// The editor changes columns, not keys: the primary key must come out as it went in.
fn check_primary_key(flavor: Flavor, diff: &Diff) -> Result<(), String> {
    let mut before: Vec<(u16, &str)> = diff
        .dropped_columns
        .iter()
        .copied()
        .chain(diff.kept_columns.iter().map(|(old, _)| *old))
        .filter_map(|c| c.primary_key.map(|p| (p, c.name.as_str())))
        .collect();
    before.sort();
    let mut after: Vec<(u16, &str)> = diff
        .kept_columns
        .iter()
        .filter_map(|(old, new)| new.primary_key.map(|p| (p, old.name.as_str())))
        .collect();
    after.sort();
    let added_key = diff.added_columns.iter().any(|c| c.primary_key.is_some());
    let same = before.len() == after.len()
        && before
            .iter()
            .zip(&after)
            .all(|(a, b)| flavor.same_name(a.1, b.1));
    if !same || added_key {
        return Err("Changing the primary key is not supported here. Use SQL for that.".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Clauses shared by the SQL databases.

fn column_list(d: Dialect, columns: &[String]) -> String {
    columns
        .iter()
        .map(|c| d.quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ")
}

fn literal(d: Dialect, text: &str) -> String {
    let mut out = String::new();
    d.push_string_literal(&mut out, text);
    out
}

/// A column as `CREATE TABLE` and `ADD COLUMN` write it, without its name.
fn column_clauses(flavor: Flavor, c: &ColumnDef, keep_collation: bool) -> String {
    let d = flavor.dialect();
    let mut out = c.type_name.trim().to_string();
    let default = default_of(c);
    match flavor {
        Flavor::Postgres => {
            if let Some(default) = default {
                out.push_str(" DEFAULT ");
                out.push_str(default);
            }
            if !c.nullable {
                out.push_str(" NOT NULL");
            }
        }
        Flavor::Mysql => {
            if keep_collation && let Some(collation) = &c.collation {
                out.push(' ');
                out.push_str(collation);
            }
            out.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
            if let Some(default) = default {
                out.push_str(" DEFAULT ");
                out.push_str(default);
            }
            if let Some(extra) = c.extra.as_deref().filter(|e| !e.is_empty()) {
                out.push(' ');
                out.push_str(extra);
            }
            if let Some(comment) = comment_of(c) {
                out.push_str(" COMMENT ");
                out.push_str(&literal(d, comment));
            }
        }
        Flavor::Sqlite { .. } | Flavor::Mongo => {
            if !c.nullable {
                out.push_str(" NOT NULL");
            }
            if let Some(default) = default {
                out.push_str(" DEFAULT ");
                out.push_str(default);
            }
        }
    }
    out.trim_start().to_string()
}

fn references(flavor: Flavor, key: &ForeignKeyDef) -> String {
    let d = flavor.dialect();
    let target = match flavor {
        // SQLite keys refer to tables in the same schema, by bare name.
        Flavor::Sqlite { .. } => d.quote_ident(&key.target.name),
        _ => d.qualified(&qualifier(flavor, &key.target.namespace), &key.target.name),
    };
    let mut out = format!(
        "FOREIGN KEY ({}) REFERENCES {target}",
        column_list(d, &key.columns)
    );
    if !key.target_columns.iter().all(|c| c.is_empty()) {
        out.push_str(&format!(" ({})", column_list(d, &key.target_columns)));
    }
    if key.on_delete != ForeignKeyAction::NoAction {
        out.push_str(" ON DELETE ");
        out.push_str(key.on_delete.sql());
    }
    if key.on_update != ForeignKeyAction::NoAction {
        out.push_str(" ON UPDATE ");
        out.push_str(key.on_update.sql());
    }
    out
}

fn constraint(flavor: Flavor, key: &ForeignKeyDef) -> String {
    if flavor.named_foreign_keys() && !key.name.is_empty() {
        format!(
            "CONSTRAINT {} {}",
            flavor.dialect().quote_ident(&key.name),
            references(flavor, key)
        )
    } else {
        references(flavor, key)
    }
}

/// The inside of `CREATE TABLE (...)`: columns, the primary key, `UNIQUE` constraints
/// and foreign keys, one per line.
fn table_body(
    flavor: Flavor,
    columns: &[&ColumnDef],
    unique: &[&IndexDef],
    keys: &[&ForeignKeyDef],
    autoincrement: bool,
) -> String {
    let d = flavor.dialect();
    let mut key: Vec<&ColumnDef> = columns
        .iter()
        .copied()
        .filter(|c| c.primary_key.is_some())
        .collect();
    key.sort_by_key(|c| c.primary_key);
    let inline_key = autoincrement && key.len() == 1;
    let mut lines: Vec<String> = columns
        .iter()
        .map(|c| {
            let mut line = format!(
                "  {} {}",
                d.quote_ident(&c.name),
                column_clauses(flavor, c, true)
            );
            if inline_key && c.primary_key.is_some() {
                line.push_str(" PRIMARY KEY AUTOINCREMENT");
            }
            line.trim_end().to_string()
        })
        .collect();
    if !key.is_empty() && !inline_key {
        let names: Vec<String> = key.iter().map(|c| c.name.clone()).collect();
        lines.push(format!("  PRIMARY KEY ({})", column_list(d, &names)));
    }
    for index in unique {
        lines.push(format!("  UNIQUE ({})", column_list(d, &index.columns)));
    }
    for key in keys {
        lines.push(format!("  {}", constraint(flavor, key)));
    }
    lines.join(",\n")
}

fn create_index(flavor: Flavor, table: &TableDef, index: &IndexDef) -> String {
    let d = flavor.dialect();
    let unique = if index.unique { "UNIQUE " } else { "" };
    let columns = column_list(d, &index.columns);
    match flavor {
        // SQLite qualifies the index, never the table.
        Flavor::Sqlite { .. } => format!(
            "CREATE {unique}INDEX {} ON {} ({columns})",
            d.qualified(&table.qualifier, &index.name),
            d.quote_ident(&table.name)
        ),
        _ => format!(
            "CREATE {unique}INDEX {} ON {} ({columns})",
            d.quote_ident(&index.name),
            d.qualified(&table.qualifier, &table.name)
        ),
    }
}

// ---------------------------------------------------------------------------------------
// PostgreSQL.

fn postgres_comment(table: &str, column: &str, comment: Option<&str>) -> String {
    let d = Dialect::Postgres;
    format!(
        "COMMENT ON COLUMN {table}.{} IS {}",
        d.quote_ident(column),
        comment.map_or_else(|| "NULL".to_string(), |c| literal(d, c))
    )
}

fn postgres_plan(table: &TableDef, diff: &Diff) -> Plan {
    let flavor = Flavor::Postgres;
    let d = Dialect::Postgres;
    let t = d.qualified(&table.qualifier, &table.name);
    let mut out = Vec::new();
    for key in &diff.dropped_keys {
        out.push(format!(
            "ALTER TABLE {t} DROP CONSTRAINT {}",
            d.quote_ident(&key.name)
        ));
    }
    for index in &diff.dropped_indexes {
        if index.constraint {
            out.push(format!(
                "ALTER TABLE {t} DROP CONSTRAINT {}",
                d.quote_ident(&index.name)
            ));
        } else {
            out.push(format!(
                "DROP INDEX {}",
                d.qualified(&table.qualifier, &index.name)
            ));
        }
    }
    for c in &diff.dropped_columns {
        out.push(format!(
            "ALTER TABLE {t} DROP COLUMN {}",
            d.quote_ident(&c.name)
        ));
    }
    for (old, new) in diff.renamed() {
        out.push(format!(
            "ALTER TABLE {t} RENAME COLUMN {} TO {}",
            d.quote_ident(&old.name),
            d.quote_ident(&new.name)
        ));
    }
    for (old, new) in &diff.kept_columns {
        let name = d.quote_ident(&new.name);
        let ty = new.type_name.trim();
        let mut old_default = default_of(old);
        if ty != old.type_name.trim() {
            // The old default may not cast to the new type, so it goes first and the
            // default the person wants is set after.
            if old_default.is_some() {
                out.push(format!("ALTER TABLE {t} ALTER COLUMN {name} DROP DEFAULT"));
                old_default = None;
            }
            out.push(format!(
                "ALTER TABLE {t} ALTER COLUMN {name} TYPE {ty} USING {name}::{ty}"
            ));
        }
        if new.nullable != old.nullable {
            let action = if new.nullable {
                "DROP NOT NULL"
            } else {
                "SET NOT NULL"
            };
            out.push(format!("ALTER TABLE {t} ALTER COLUMN {name} {action}"));
        }
        if default_of(new) != old_default {
            out.push(match default_of(new) {
                Some(default) => {
                    format!("ALTER TABLE {t} ALTER COLUMN {name} SET DEFAULT {default}")
                }
                None => format!("ALTER TABLE {t} ALTER COLUMN {name} DROP DEFAULT"),
            });
        }
        if comment_of(new) != comment_of(old) {
            out.push(postgres_comment(&t, &new.name, comment_of(new)));
        }
    }
    for c in &diff.added_columns {
        out.push(format!(
            "ALTER TABLE {t} ADD COLUMN {} {}",
            d.quote_ident(&c.name),
            column_clauses(flavor, c, true)
        ));
        if let Some(comment) = comment_of(c) {
            out.push(postgres_comment(&t, &c.name, Some(comment)));
        }
    }
    for index in &diff.added_indexes {
        out.push(create_index(flavor, table, index));
    }
    for key in &diff.added_keys {
        out.push(format!("ALTER TABLE {t} ADD {}", constraint(flavor, key)));
    }
    Plan {
        statements: out,
        notes: Vec::new(),
    }
}

// ---------------------------------------------------------------------------------------
// MySQL and MariaDB.

fn mysql_plan(table: &TableDef, diff: &Diff) -> Result<Plan, String> {
    let flavor = Flavor::Mysql;
    let d = Dialect::Mysql;
    let t = d.qualified(&table.qualifier, &table.name);
    let mut out = Vec::new();
    for key in &diff.dropped_keys {
        out.push(format!(
            "ALTER TABLE {t} DROP FOREIGN KEY {}",
            d.quote_ident(&key.name)
        ));
    }
    for index in &diff.dropped_indexes {
        out.push(format!(
            "ALTER TABLE {t} DROP INDEX {}",
            d.quote_ident(&index.name)
        ));
    }
    for c in &diff.dropped_columns {
        out.push(format!(
            "ALTER TABLE {t} DROP COLUMN {}",
            d.quote_ident(&c.name)
        ));
    }
    for (old, new) in &diff.kept_columns {
        let renamed = old.name != new.name;
        if !renamed && !new.changed_from(old) {
            continue;
        }
        if old.generated {
            return Err(format!(
                "Column {} is generated. Change it with SQL instead.",
                old.name
            ));
        }
        let same_type = new
            .type_name
            .trim()
            .eq_ignore_ascii_case(old.type_name.trim());
        // Without its CHARACTER SET and COLLATE, a column takes the table's, so they stay
        // as long as the column holds text: a longer varchar keeps comparing as it did.
        let keep_collation =
            same_type || (holds_text(&old.type_name) && holds_text(&new.type_name));
        let clauses = column_clauses(flavor, new, keep_collation);
        out.push(if renamed {
            format!(
                "ALTER TABLE {t} CHANGE COLUMN {} {} {clauses}",
                d.quote_ident(&old.name),
                d.quote_ident(&new.name)
            )
        } else {
            format!(
                "ALTER TABLE {t} MODIFY COLUMN {} {clauses}",
                d.quote_ident(&new.name)
            )
        });
    }
    for c in &diff.added_columns {
        out.push(format!(
            "ALTER TABLE {t} ADD COLUMN {} {}",
            d.quote_ident(&c.name),
            column_clauses(flavor, c, true)
        ));
    }
    for index in &diff.added_indexes {
        out.push(create_index(flavor, table, index));
    }
    for key in &diff.added_keys {
        out.push(format!("ALTER TABLE {t} ADD {}", constraint(flavor, key)));
    }
    Ok(Plan {
        statements: out,
        notes: Vec::new(),
    })
}

/// Whether a MySQL type holds characters, so a character set and collation apply to it.
fn holds_text(type_name: &str) -> bool {
    let word: String = type_name
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_lowercase();
    matches!(
        word.as_str(),
        "char"
            | "character"
            | "varchar"
            | "nchar"
            | "nvarchar"
            | "national"
            | "tinytext"
            | "text"
            | "mediumtext"
            | "longtext"
            | "enum"
            | "set"
    )
}

// ---------------------------------------------------------------------------------------
// SQLite.

fn sqlite_plan(
    flavor: Flavor,
    drop_column: bool,
    table: &TableDef,
    diff: &Diff,
) -> Result<Plan, String> {
    let d = Dialect::Sqlite;
    // A column can only go when nothing that stays depends on it.
    for c in &diff.dropped_columns {
        let uses = |columns: &[String]| columns.iter().any(|n| flavor.same_name(n, &c.name));
        if let Some(index) = diff.kept_indexes.iter().find(|i| uses(&i.columns)) {
            return Err(format!(
                "Column {} is part of index {}. Drop the index as well, or keep the column.",
                c.name, index.name
            ));
        }
        if diff.kept_keys.iter().any(|k| uses(&k.columns)) {
            return Err(format!(
                "Column {} is part of a foreign key. Drop the key as well, or keep the column.",
                c.name
            ));
        }
    }
    let rebuild = diff.kept_columns.iter().any(|(old, new)| {
        old.type_name.trim() != new.type_name.trim()
            || old.nullable != new.nullable
            || default_of(old) != default_of(new)
    }) || !diff.dropped_keys.is_empty()
        || !diff.added_keys.is_empty()
        || diff.dropped_indexes.iter().any(|i| i.constraint)
        || diff
            .added_columns
            .iter()
            .any(|c| !c.nullable && default_of(c).is_none())
        || (!diff.dropped_columns.is_empty() && !drop_column);

    if !rebuild {
        let t = d.qualified(&table.qualifier, &table.name);
        let mut out = Vec::new();
        for index in &diff.dropped_indexes {
            out.push(format!(
                "DROP INDEX {}",
                d.qualified(&table.qualifier, &index.name)
            ));
        }
        for c in &diff.dropped_columns {
            out.push(format!(
                "ALTER TABLE {t} DROP COLUMN {}",
                d.quote_ident(&c.name)
            ));
        }
        for (old, new) in diff.renamed() {
            out.push(format!(
                "ALTER TABLE {t} RENAME COLUMN {} TO {}",
                d.quote_ident(&old.name),
                d.quote_ident(&new.name)
            ));
        }
        for c in &diff.added_columns {
            out.push(format!(
                "ALTER TABLE {t} ADD COLUMN {} {}",
                d.quote_ident(&c.name),
                column_clauses(flavor, c, true)
            ));
        }
        for index in &diff.added_indexes {
            out.push(create_index(flavor, table, index));
        }
        return Ok(Plan {
            statements: out,
            notes: Vec::new(),
        });
    }

    if diff.renamed().next().is_some() {
        return Err(
            "SQLite has to rebuild the table for these changes. Apply the column renames on their own first, then the rest."
                .into(),
        );
    }
    if let Some(blocker) = &table.rebuild.blocker {
        return Err(format!(
            "SQLite can only make these changes by rebuilding the table, and the rebuild would lose its {blocker}. Change it with SQL instead."
        ));
    }
    let temp = format!("{REBUILD_PREFIX}{}", table.name);
    let old = d.qualified(&table.qualifier, &table.name);
    let new = d.qualified(&table.qualifier, &temp);
    let unique: Vec<&IndexDef> = diff
        .kept_indexes
        .iter()
        .copied()
        .filter(|i| i.constraint && !i.primary)
        .collect();
    let keys: Vec<&ForeignKeyDef> = diff
        .kept_keys
        .iter()
        .chain(&diff.added_keys)
        .copied()
        .collect();
    let mut out = vec![format!(
        "CREATE TABLE {new} (\n{}\n)",
        table_body(
            flavor,
            &diff.columns,
            &unique,
            &keys,
            table.rebuild.autoincrement
        )
    )];
    let copied: Vec<String> = diff
        .kept_columns
        .iter()
        .map(|(_, c)| c.name.clone())
        .collect();
    if !copied.is_empty() {
        let list = column_list(d, &copied);
        out.push(format!(
            "INSERT INTO {new} ({list}) SELECT {list} FROM {old}"
        ));
    }
    let autoincrement = table.rebuild.autoincrement
        && diff
            .columns
            .iter()
            .filter(|c| c.primary_key.is_some())
            .count()
            == 1;
    if autoincrement {
        // The new table's counter only knows the copied rows. The old one may have gone
        // higher before its last rows were deleted, and AUTOINCREMENT promises never to
        // hand out those numbers again, so the new table takes over the old counter. The
        // rename then carries it over to the table's own name.
        let seq = d.qualified(&table.qualifier, "sqlite_sequence");
        let (old_name, new_name) = (literal(d, &table.name), literal(d, &temp));
        out.push(format!(
            "INSERT INTO {seq} (name, seq) SELECT {new_name}, 0 WHERE NOT EXISTS (SELECT 1 FROM {seq} WHERE name = {new_name})"
        ));
        out.push(format!(
            "UPDATE {seq} SET seq = max(seq, coalesce((SELECT max(seq) FROM {seq} WHERE name = {old_name}), 0)) WHERE name = {new_name}"
        ));
    }
    out.push(format!("DROP TABLE {old}"));
    // Views that name the table would fail the rename's check while it is gone; the
    // legacy rename leaves them, and the keys of other tables, as they are.
    out.push("PRAGMA legacy_alter_table = ON".into());
    out.push(format!(
        "ALTER TABLE {new} RENAME TO {}",
        d.quote_ident(&table.name)
    ));
    out.push("PRAGMA legacy_alter_table = OFF".into());
    for index in diff.kept_indexes.iter().filter(|i| !i.constraint) {
        out.push(
            index
                .sql
                .clone()
                .unwrap_or_else(|| create_index(flavor, table, index)),
        );
    }
    for index in &diff.added_indexes {
        out.push(create_index(flavor, table, index));
    }
    out.extend(table.rebuild.triggers.iter().cloned());
    Ok(Plan {
        statements: out,
        notes: vec![
            "SQLite cannot make some of these changes in place, so the table is rebuilt: its rows are copied into a new table that then takes its name, and its indexes and triggers are created again. Foreign keys are checked before the change is committed."
                .into(),
        ],
    })
}

/// What a SQLite rebuild needs to know from the table's definition, which is its
/// `CREATE TABLE` followed by its `CREATE INDEX` and `CREATE TRIGGER` statements. Fills
/// in each index's own statement.
fn sqlite_rebuild(definition: &str, indexes: &mut [IndexDef]) -> Rebuild {
    let mut rebuild = Rebuild::default();
    let mut first = true;
    for range in split_statements(definition, Dialect::Sqlite) {
        let statement = &definition[range];
        let tokens = tokens(statement, false, false);
        let words: Vec<String> = tokens
            .iter()
            .filter_map(|(t, _)| match t {
                Token::Word(w) => Some(w.to_ascii_uppercase()),
                _ => None,
            })
            .take(3)
            .collect();
        let starts = |w: &[&str]| {
            words
                .iter()
                .map(String::as_str)
                .take(w.len())
                .eq(w.iter().copied())
        };
        if first {
            first = false;
            rebuild.blocker = sqlite_blocker(&tokens);
            rebuild.autoincrement = tokens.iter().any(
                |(t, _)| matches!(t, Token::Word(w) if w.eq_ignore_ascii_case("AUTOINCREMENT")),
            );
            if !starts(&["CREATE", "TABLE"]) && rebuild.blocker.is_none() {
                rebuild.blocker = Some("definition, which is not a plain CREATE TABLE".into());
            }
        } else if starts(&["CREATE", "TRIGGER"]) || starts(&["CREATE", "TEMP"]) {
            rebuild.triggers.push(statement.to_string());
        } else if (starts(&["CREATE", "INDEX"]) || starts(&["CREATE", "UNIQUE", "INDEX"]))
            && let Some(name) = created_name(&tokens)
            && let Some(index) = indexes
                .iter_mut()
                .find(|i| i.name.eq_ignore_ascii_case(&name))
        {
            index.sql = Some(statement.to_string());
        }
    }
    rebuild
}

/// The clause of a `CREATE TABLE` that a rebuild from columns, keys and indexes would
/// lose, named for the person.
fn sqlite_blocker(tokens: &[(Token, Range<usize>)]) -> Option<String> {
    for (i, (token, _)) in tokens.iter().enumerate() {
        let Token::Word(word) = token else { continue };
        let next_is_paren = matches!(tokens.get(i + 1), Some((Token::Punct('('), _)));
        let found = match word.to_ascii_uppercase().as_str() {
            "VIRTUAL" => "virtual table module",
            "CHECK" => "CHECK constraints",
            "COLLATE" => "column collations",
            "GENERATED" => "generated columns",
            "AS" if next_is_paren => "generated columns",
            "WITHOUT" => "WITHOUT ROWID storage",
            "STRICT" => "STRICT typing",
            "CONFLICT" => "ON CONFLICT clauses",
            "DEFERRABLE" => "deferred foreign keys",
            _ => continue,
        };
        return Some(found.into());
    }
    None
}

/// The object a `CREATE [UNIQUE] INDEX [IF NOT EXISTS] [schema.]name` makes.
fn created_name(tokens: &[(Token, Range<usize>)]) -> Option<String> {
    let mut i = tokens
        .iter()
        .position(|(t, _)| matches!(t, Token::Word(w) if w.eq_ignore_ascii_case("INDEX")))?
        + 1;
    let word = |i: usize, w: &str| matches!(tokens.get(i), Some((Token::Word(x), _)) if x.eq_ignore_ascii_case(w));
    if word(i, "IF") && word(i + 1, "NOT") && word(i + 2, "EXISTS") {
        i += 3;
    }
    let name_at = |i: usize| match tokens.get(i) {
        Some((Token::Word(w), _)) => Some(w.to_string()),
        Some((Token::Quoted(q), _)) => Some(q.clone()),
        _ => None,
    };
    let mut name = name_at(i)?;
    if matches!(tokens.get(i + 1), Some((Token::Punct('.'), _))) {
        name = name_at(i + 2)?;
    }
    Some(name)
}

// ---------------------------------------------------------------------------------------
// MongoDB.

fn mongo_plan(table: &TableDef, diff: &Diff) -> Result<Plan, String> {
    let collection = json_string(&table.name);
    let mut out = Vec::new();
    for index in &diff.dropped_indexes {
        out.push(format!(
            "db.runCommand({{dropIndexes: {collection}, index: {}}})",
            json_string(&index.name)
        ));
    }
    for index in &diff.added_indexes {
        let key = index
            .columns
            .iter()
            .map(|c| format!("{}: 1", json_string(c)))
            .collect::<Vec<_>>()
            .join(", ");
        let unique = if index.unique { ", unique: true" } else { "" };
        out.push(format!(
            "db.runCommand({{createIndexes: {collection}, indexes: [{{key: {{{key}}}, name: {}{unique}}}]}})",
            json_string(&index.name)
        ));
    }
    Ok(Plan {
        statements: out,
        notes: Vec::new(),
    })
}

/// `text` as a JSON string.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------------------
// Reading definitions.

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A keyword, bare name or number.
    Word(String),
    /// A quoted name, without its quotes.
    Quoted(String),
    /// A string literal.
    Text,
    /// Parentheses and what is inside, when grouping.
    Group,
    Punct(char),
}

/// Splits SQL into tokens with their byte ranges, skipping comments. MySQL reads `"..."`
/// as a string and backslash escapes inside strings. With `group`, a parenthesized part
/// is one token.
fn tokens(sql: &str, mysql: bool, group: bool) -> Vec<(Token, Range<usize>)> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    // Where a quoted run that starts at `start` ends, and where its inside ends: before
    // the closing quote, or at the end of the text when there is none.
    let quoted = |start: usize, quote: u8, escapes: bool| -> (usize, usize) {
        let mut j = start + 1;
        while j < bytes.len() {
            if escapes && bytes[j] == b'\\' {
                j += 2;
                continue;
            }
            if bytes[j] == quote {
                if bytes.get(j + 1) == Some(&quote) {
                    j += 2;
                    continue;
                }
                return (j + 1, j);
            }
            j += 1;
        }
        (bytes.len(), bytes.len())
    };
    let quoted_end = |start: usize, quote: u8, escapes: bool| quoted(start, quote, escapes).0;
    while i < bytes.len() {
        let b = bytes[i];
        let start = i;
        match b {
            b if b.is_ascii_whitespace() => i += 1,
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = sql[i + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |e| i + 2 + e + 2);
            }
            b'\'' => {
                i = quoted_end(i, b'\'', mysql);
                out.push((Token::Text, start..i));
            }
            b'"' if mysql => {
                i = quoted_end(i, b'"', true);
                out.push((Token::Text, start..i));
            }
            b'"' | b'`' => {
                let (end, inner_end) = quoted(i, b, false);
                i = end;
                let inner = sql.get(start + 1..inner_end).unwrap_or_default();
                let quote = b as char;
                let doubled: String = [quote, quote].iter().collect();
                out.push((
                    Token::Quoted(inner.replace(&doubled, &quote.to_string())),
                    start..i,
                ));
            }
            b'[' if !mysql => {
                let inner_end = sql[i..].find(']').map_or(bytes.len(), |e| i + e);
                i = (inner_end + 1).min(bytes.len());
                let inner = sql.get(start + 1..inner_end).unwrap_or_default();
                out.push((Token::Quoted(inner.to_string()), start..i));
            }
            b'(' if group => {
                let mut depth = 0usize;
                while i < bytes.len() {
                    match bytes[i] {
                        b'(' => {
                            depth += 1;
                            i += 1;
                        }
                        b')' => {
                            depth -= 1;
                            i += 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        b'\'' => i = quoted_end(i, b'\'', mysql),
                        b'"' => i = quoted_end(i, b'"', mysql),
                        b'`' => i = quoted_end(i, b'`', false),
                        _ => i += 1,
                    }
                }
                out.push((Token::Group, start..i));
            }
            b if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80 => {
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric()
                        || bytes[i] == b'_'
                        || bytes[i] == b'$'
                        || bytes[i] >= 0x80)
                {
                    i += 1;
                }
                out.push((Token::Word(sql[start..i].to_string()), start..i));
            }
            _ => {
                // Step over one whole character.
                let len = sql[i..].chars().next().map_or(1, char::len_utf8);
                i += len;
                out.push((Token::Punct(b as char), start..i));
            }
        }
    }
    out
}

/// What `SHOW CREATE TABLE` says about a column beyond the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct MysqlColumn {
    name: String,
    default: Option<String>,
    collation: Option<String>,
    extra: Option<String>,
    generated: bool,
}

/// Reads the column lines of a MySQL or MariaDB `SHOW CREATE TABLE`, which puts each
/// column on a line of its own starting with its quoted name.
fn mysql_columns(definition: &str) -> Vec<MysqlColumn> {
    let mut out = Vec::new();
    for line in definition.lines() {
        let line = line.trim().trim_end_matches(',');
        if !line.starts_with('`') {
            continue;
        }
        let tokens = tokens(line, true, true);
        let Some((Token::Quoted(name), _)) = tokens.first() else {
            continue;
        };
        let mut column = MysqlColumn {
            name: name.clone(),
            ..Default::default()
        };
        let (mut collation, mut extra): (Vec<&str>, Vec<(usize, &str)>) = (Vec::new(), Vec::new());
        // Versioned comments carry clauses of their own, such as `/*!80023 INVISIBLE */`
        // or `/*!80003 SRID 4326 */`; the tokens skip comments, so they are found here.
        let mut from = 0;
        while let Some(at) = line[from..].find("/*!").map(|p| from + p) {
            let end = line[at..].find("*/").map_or(line.len(), |e| at + e + 2);
            if !tokens.iter().any(|(_, r)| r.start <= at && at < r.end) {
                extra.push((at, &line[at..end]));
            }
            from = end;
        }
        let is = |i: usize, w: &str| matches!(tokens.get(i), Some((Token::Word(x), _)) if x.eq_ignore_ascii_case(w));
        let mut i = 1;
        while i < tokens.len() {
            if is(i, "CHARACTER") && is(i + 1, "SET") && i + 2 < tokens.len() {
                collation.push(&line[tokens[i].1.start..tokens[i + 2].1.end]);
                i += 3;
            } else if (is(i, "COLLATE") || is(i, "CHARSET")) && i + 1 < tokens.len() {
                collation.push(&line[tokens[i].1.start..tokens[i + 1].1.end]);
                i += 2;
            } else if is(i, "DEFAULT") {
                let end = value_end(&tokens, i + 1);
                if end > i + 1 {
                    let text = &line[tokens[i + 1].1.start..tokens[end - 1].1.end];
                    column.default = (!text.eq_ignore_ascii_case("NULL")).then(|| text.to_string());
                }
                i = end;
            } else if is(i, "ON") && is(i + 1, "UPDATE") {
                let end = value_end(&tokens, i + 2);
                if end > i + 2 {
                    let start = tokens[i].1.start;
                    extra.push((start, &line[start..tokens[end - 1].1.end]));
                }
                i = end;
            } else if is(i, "AUTO_INCREMENT") || is(i, "INVISIBLE") {
                extra.push((tokens[i].1.start, &line[tokens[i].1.clone()]));
                i += 1;
            } else if is(i, "GENERATED")
                || (is(i, "AS") && matches!(tokens.get(i + 1), Some((Token::Group, _))))
            {
                column.generated = true;
                i += 1;
            } else if is(i, "COMMENT") {
                i += 2;
            } else {
                i += 1;
            }
        }
        column.collation = (!collation.is_empty()).then(|| collation.join(" "));
        extra.sort_by_key(|(at, _)| *at);
        column.extra = (!extra.is_empty()).then(|| {
            extra
                .iter()
                .map(|(_, text)| *text)
                .collect::<Vec<_>>()
                .join(" ")
        });
        out.push(column);
    }
    out
}

/// Where one value that starts at `i` ends: a string (with a charset introducer or a
/// `b`/`x` prefix), a parenthesized expression, a signed number, or a word with an
/// optional argument list such as `current_timestamp(3)`.
fn value_end(tokens: &[(Token, Range<usize>)], i: usize) -> usize {
    let adjacent = |a: usize, b: usize| match (tokens.get(a), tokens.get(b)) {
        (Some((_, x)), Some((_, y))) => x.end == y.start,
        _ => false,
    };
    match tokens.get(i) {
        Some((Token::Text | Token::Group, _)) => i + 1,
        Some((Token::Punct('-' | '+'), _)) => number_end(tokens, i + 1),
        Some((Token::Word(w), _)) => {
            let prefix = w.starts_with('_') || matches!(w.as_str(), "b" | "B" | "x" | "X" | "N");
            if prefix && matches!(tokens.get(i + 1), Some((Token::Text, _))) && adjacent(i, i + 1) {
                return i + 2;
            }
            let mut end = number_end(tokens, i);
            if matches!(tokens.get(end), Some((Token::Group, _))) && adjacent(end - 1, end) {
                end += 1;
            }
            end
        }
        _ => i,
    }
}

/// The end of `1`, `1.5` or a single word starting at `i`.
fn number_end(tokens: &[(Token, Range<usize>)], i: usize) -> usize {
    if !matches!(tokens.get(i), Some((Token::Word(_), _))) {
        return i;
    }
    if matches!(tokens.get(i + 1), Some((Token::Punct('.'), _)))
        && matches!(tokens.get(i + 2), Some((Token::Word(_), _)))
    {
        return i + 3;
    }
    i + 1
}

#[cfg(test)]
mod tests;

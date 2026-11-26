//! The export the dump dialog sets up, laid out the way Adminer's export is: where it goes
//! (open, save, gzip), the format (SQL, CSV with a comma or a semicolon, TSV, JSON), what
//! happens to each database, table and its data, and per table whether its structure and
//! its rows go out.
//!
//! SQL means the database's own: statements for PostgreSQL, MySQL and SQLite, and shell
//! calls for MongoDB, which Deltos' MongoDB shell and `mongosh` both run. A dump goes to
//! one file. The other formats write a file per table, in a folder when there are several.
//! Rows stream a page at a time through [`deltos_grid::export`], so a table of any size
//! goes out in bounded memory, into a temporary file renamed over the target at the end.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use deltos_data::{
    Column, Connection, DriverKind, Error, ExecuteOptions, ObjectInfo, ObjectKind, ObjectRef, Row,
    Value,
};
use deltos_drivers::sql::{Dialect, split_statements};
use deltos_grid::{
    BrowseConfig, BrowseLoader, BrowseQuery, CsvFormat, CsvWriter, ExportOptions, JsonStyle,
    JsonWriter, QuoteStyle, RowWriter,
};
use flate2::Compression;
use flate2::write::GzEncoder;

use super::database::{Table, in_dependency_order};
use super::export::temp_path;
use super::sql::{InsertWriter, MongoMode, MongoWriter, mongo_collection, truncate_statement};
use super::{Outcome, Progress, count, file_label};
use crate::ui::edit;

/// Where the export goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// A temporary file, opened when it is written: a dump in a query tab, other files
    /// in the system's app for them.
    Open,
    Save,
    /// Saved, compressed with gzip.
    Gzip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// SQL, or for MongoDB the shell's calls.
    Script,
    Csv,
    CsvSemicolon,
    Tsv,
    Json,
}

impl Format {
    pub fn extension(self, kind: DriverKind) -> &'static str {
        match self {
            Format::Script if kind == DriverKind::Mongodb => "js",
            Format::Script => "sql",
            Format::Csv | Format::CsvSemicolon => "csv",
            Format::Tsv => "tsv",
            Format::Json => "json",
        }
    }

    fn csv(self) -> Option<u8> {
        match self {
            Format::Csv => Some(b','),
            Format::CsvSemicolon => Some(b';'),
            Format::Tsv => Some(b'\t'),
            Format::Script | Format::Json => None,
        }
    }
}

/// What a dump does to each database (a schema on PostgreSQL) before its tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseMode {
    None,
    Use,
    DropCreate,
    Create,
}

/// What a dump writes for each table's structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TablesMode {
    None,
    DropCreate,
    Create,
}

/// How a dump writes each table's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataMode {
    None,
    TruncateInsert,
    Insert,
    InsertUpdate,
}

/// Everything the dialog's options say. The last one used is offered next time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub output: Output,
    pub format: Format,
    pub database: DatabaseMode,
    pub routines: bool,
    pub events: bool,
    pub tables: TablesMode,
    /// Keep MySQL's `AUTO_INCREMENT=` counter in each `CREATE TABLE`.
    pub auto_increment: bool,
    pub triggers: bool,
    pub data: DataMode,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            output: Output::Save,
            format: Format::Script,
            database: DatabaseMode::None,
            routines: true,
            events: true,
            tables: TablesMode::DropCreate,
            auto_increment: false,
            triggers: true,
            data: DataMode::Insert,
        }
    }
}

static LAST: std::sync::Mutex<Option<Options>> = std::sync::Mutex::new(None);

impl Options {
    /// The options used last in this run of the app, or the defaults.
    pub fn last() -> Self {
        LAST.lock().ok().and_then(|l| *l).unwrap_or_default()
    }

    pub fn remember(self) {
        if let Ok(mut l) = LAST.lock() {
            *l = Some(self);
        }
    }
}

/// What a database offers in the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Engine(pub DriverKind);

impl Engine {
    fn mongo(self) -> bool {
        self.0 == DriverKind::Mongodb
    }

    /// Whether it has a dump format at all.
    pub fn script(self) -> bool {
        edit::speaks_sql(self.0) || self.mongo()
    }

    pub fn script_label(self) -> &'static str {
        if self.mongo() { "Shell" } else { "SQL" }
    }

    /// The database row's label: PostgreSQL's is its schemas. SQLite has one database.
    pub fn database_label(self) -> Option<&'static str> {
        match self.0 {
            DriverKind::Sqlite => None,
            DriverKind::Postgres => Some("Schema"),
            _ => Some("Database"),
        }
    }

    pub fn database_modes(self) -> &'static [(&'static str, DatabaseMode)] {
        const ALL: [(&str, DatabaseMode); 4] = [
            ("None", DatabaseMode::None),
            ("USE", DatabaseMode::Use),
            ("DROP+CREATE", DatabaseMode::DropCreate),
            ("CREATE", DatabaseMode::Create),
        ];
        // A MongoDB database exists once something is written to it.
        if self.mongo() { &ALL[..3] } else { &ALL }
    }

    pub fn routines(self) -> bool {
        matches!(self.0, DriverKind::Mysql | DriverKind::Postgres)
    }

    pub fn events(self) -> bool {
        self.0 == DriverKind::Mysql
    }

    pub fn auto_increment(self) -> bool {
        self.0 == DriverKind::Mysql
    }

    pub fn triggers(self) -> bool {
        matches!(
            self.0,
            DriverKind::Mysql | DriverKind::Postgres | DriverKind::Sqlite
        )
    }

    fn name(self) -> String {
        match self.0 {
            DriverKind::Postgres => "PostgreSQL".into(),
            DriverKind::Mysql => "MySQL".into(),
            DriverKind::Sqlite => "SQLite".into(),
            DriverKind::Mongodb => "MongoDB".into(),
            other => format!("{other:?}"),
        }
    }

    fn comment(self) -> &'static str {
        if self.mongo() { "//" } else { "--" }
    }
}

/// One table, view or collection of the list, with what goes out of it.
#[derive(Debug, Clone)]
pub struct Pick {
    pub object: ObjectRef,
    pub estimated_rows: Option<u64>,
    pub structure: bool,
    pub data: bool,
    /// The rows to write, when not all of them: a table tab's filtered view.
    pub query: Option<BrowseQuery>,
    /// The columns to start with, such as the fields a collection's tab has seen.
    pub columns: Option<Arc<[Column]>>,
}

impl Pick {
    pub fn new(object: ObjectRef, estimated_rows: Option<u64>) -> Self {
        // Views go out as their definition; their rows are the tables'.
        let data = matches!(object.kind, ObjectKind::Table | ObjectKind::Collection);
        Self {
            object,
            estimated_rows,
            structure: true,
            data,
            query: None,
            columns: None,
        }
    }

    pub fn is_view(&self) -> bool {
        matches!(
            self.object.kind,
            ObjectKind::View | ObjectKind::MaterializedView
        )
    }
}

/// The database mode a dump runs with: a dump that spans several databases says which
/// one each table goes in, so "None" becomes "USE" there.
pub fn effective_database(engine: Engine, mode: DatabaseMode, namespaces: usize) -> DatabaseMode {
    if engine.database_label().is_none() {
        DatabaseMode::None
    } else if mode == DatabaseMode::None && namespaces > 1 {
        DatabaseMode::Use
    } else {
        mode
    }
}

/// The lines a dump starts with.
pub fn header(engine: Engine) -> String {
    let mut out = format!(
        "{} Deltos {} {} dump\n\n",
        engine.comment(),
        env!("CARGO_PKG_VERSION"),
        engine.name()
    );
    match engine.0 {
        DriverKind::Mysql => out.push_str(
            "SET NAMES utf8mb4;\nSET foreign_key_checks = 0;\nSET sql_mode = 'NO_AUTO_VALUE_ON_ZERO';\n\n",
        ),
        DriverKind::Postgres => out.push_str("SET client_encoding = 'UTF8';\n\n"),
        DriverKind::Sqlite => out.push_str("PRAGMA foreign_keys = OFF;\n\n"),
        _ => {}
    }
    out
}

/// The database a MongoDB statement names, when the dump names databases.
fn mongo_database(mode: DatabaseMode, namespace: &[String]) -> Option<&str> {
    (mode != DatabaseMode::None)
        .then(|| namespace.first().map(String::as_str))
        .flatten()
}

/// What goes before the first table of `namespace` (`first`) and when the dump comes back
/// to it. `create` is the database's own `CREATE DATABASE`, where it gives one.
pub fn enter_namespace(
    engine: Engine,
    namespace: &[String],
    mode: DatabaseMode,
    first: bool,
    create: Option<&str>,
) -> String {
    let q = |n: &str| edit::quote_ident(engine.0, n);
    match engine.0 {
        DriverKind::Mysql => {
            let Some(db) = namespace.first() else {
                return String::new();
            };
            let use_it = format!("USE {};\n", q(db));
            match (mode, first) {
                (DatabaseMode::None, _) => String::new(),
                (_, false) | (DatabaseMode::Use, _) => use_it,
                (DatabaseMode::Create | DatabaseMode::DropCreate, true) => {
                    let mut out = String::new();
                    if mode == DatabaseMode::DropCreate {
                        out.push_str(&format!("DROP DATABASE IF EXISTS {};\n", q(db)));
                    }
                    let create = create
                        .map(|c| c.trim().trim_end_matches(';').to_owned())
                        .filter(|c| !c.is_empty())
                        .unwrap_or_else(|| format!("CREATE DATABASE {}", q(db)));
                    out.push_str(&create);
                    out.push_str(";\n");
                    out.push_str(&use_it);
                    out
                }
            }
        }
        DriverKind::Postgres => {
            // Database then schema; a path of the database alone has no schema to name.
            let Some(schema) = namespace.get(1) else {
                return String::new();
            };
            let set = format!("SET search_path TO {};\n", q(schema));
            match (mode, first) {
                (DatabaseMode::None, _) => String::new(),
                (_, false) | (DatabaseMode::Use, _) => set,
                (DatabaseMode::Create, true) => {
                    format!("CREATE SCHEMA IF NOT EXISTS {};\n{set}", q(schema))
                }
                (DatabaseMode::DropCreate, true) => format!(
                    "DROP SCHEMA IF EXISTS {} CASCADE;\nCREATE SCHEMA {};\n{set}",
                    q(schema),
                    q(schema)
                ),
            }
        }
        DriverKind::Mongodb => match (mode, first, namespace.first()) {
            (DatabaseMode::DropCreate, true, Some(db)) => {
                format!("{}.dropDatabase();\n", mongo_db(db))
            }
            _ => String::new(),
        },
        _ => String::new(),
    }
}

fn mongo_db(db: &str) -> String {
    format!("db.getSiblingDB({})", edit::json_string(db))
}

/// `object` as a dump's statements name it. A MySQL dump leaves the database to `USE`,
/// as `SHOW CREATE TABLE` does, so it loads into whichever database it is run in.
pub fn dml_object(kind: DriverKind, object: &ObjectRef) -> ObjectRef {
    match kind {
        DriverKind::Mysql => ObjectRef {
            namespace: Vec::new(),
            ..object.clone()
        },
        _ => object.clone(),
    }
}

/// `DROP ... IF EXISTS`, or a collection's `drop()`.
pub fn drop_statement(engine: Engine, object: &ObjectRef, mode: DatabaseMode) -> String {
    if engine.mongo() {
        let db = mongo_database(mode, &object.namespace);
        return format!("{}.drop();\n", mongo_collection(db, &object.name));
    }
    let what = match object.kind {
        ObjectKind::View => "VIEW",
        ObjectKind::MaterializedView => "MATERIALIZED VIEW",
        _ => "TABLE",
    };
    let name = edit::table_name(engine.0, &dml_object(engine.0, object));
    let cascade = if engine.0 == DriverKind::Postgres {
        " CASCADE"
    } else {
        ""
    };
    format!("DROP {what} IF EXISTS {name}{cascade};\n")
}

/// The statements that make `object`, from its definition, with the options applied.
pub fn structure(
    engine: Engine,
    object: &ObjectRef,
    definition: Option<&str>,
    options: &Options,
    database: DatabaseMode,
) -> String {
    let Some(definition) = definition.map(str::trim).filter(|d| !d.is_empty()) else {
        return format!(
            "{} {}: the database gave no definition\n",
            engine.comment(),
            object.name
        );
    };
    let mut text = match engine.0 {
        DriverKind::Mysql if !options.auto_increment => strip_auto_increment(definition),
        // Its triggers go at the end of the dump, with the others.
        DriverKind::Sqlite => split_sqlite(definition).0,
        DriverKind::Mongodb => match mongo_database(database, &object.namespace) {
            Some(db) => qualify_mongo(definition, db),
            None => definition.to_owned(),
        },
        _ => definition.to_owned(),
    };
    let text_end = text.trim_end().len();
    text.truncate(text_end);
    if !engine.mongo() && !text.ends_with(';') {
        text.push(';');
    }
    text.push('\n');
    text
}

/// A MySQL `CREATE TABLE` without its `AUTO_INCREMENT=` table option, the counter, which
/// is the next id and not part of the structure.
pub fn strip_auto_increment(definition: &str) -> String {
    const OPTION: &str = " AUTO_INCREMENT=";
    let mut out = String::with_capacity(definition.len());
    let mut rest = definition;
    while let Some(i) = rest.find(OPTION) {
        let after = &rest[i + OPTION.len()..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            out.push_str(&rest[..i + OPTION.len()]);
        } else {
            out.push_str(&rest[..i]);
        }
        rest = &after[digits..];
    }
    out.push_str(rest);
    out
}

/// A SQLite table's definition, which carries its indexes and triggers: the rest of it,
/// and the triggers apart.
pub fn split_sqlite(definition: &str) -> (String, Vec<String>) {
    let mut kept = Vec::new();
    let mut triggers = Vec::new();
    for range in split_statements(definition, Dialect::Sqlite) {
        let statement = definition[range].trim();
        let words: Vec<String> = statement
            .split_whitespace()
            .take(3)
            .map(str::to_ascii_uppercase)
            .collect();
        let word = |i: usize| words.get(i).map(String::as_str);
        let trigger = word(1) == Some("TRIGGER")
            || (matches!(word(1), Some("TEMP" | "TEMPORARY")) && word(2) == Some("TRIGGER"));
        if trigger {
            triggers.push(format!("{statement};"));
        } else {
            kept.push(statement);
        }
    }
    let mut rest = kept.join(";\n\n");
    if !rest.is_empty() {
        rest.push(';');
    }
    (rest, triggers)
}

/// MongoDB's `db.createCollection(...)` lines, made to name database `db`.
fn qualify_mongo(definition: &str, db: &str) -> String {
    let prefix = format!("{}.", mongo_db(db));
    definition
        .lines()
        .map(|line| match line.strip_prefix("db.") {
            Some(rest) => format!("{prefix}{rest}"),
            None => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Statements wrapped for the MySQL client, whose bodies hold semicolons: each is ended
/// with `;;` between `DELIMITER ;;` and `DELIMITER ;`.
pub fn delimited(statements: &[String]) -> String {
    if statements.is_empty() {
        return String::new();
    }
    let mut out = String::from("DELIMITER ;;\n");
    for s in statements {
        out.push_str(s.trim().trim_end_matches(';').trim_end());
        out.push_str(";;\n");
    }
    out.push_str("DELIMITER ;\n");
    out
}

/// The rows of a small catalog query, such as a table's triggers.
async fn query_rows(
    conn: &dyn Connection,
    sql: &str,
    namespace: Option<Vec<String>>,
) -> Result<Vec<Row>, Error> {
    use futures::StreamExt;
    let options = ExecuteOptions {
        batch_size: 1000,
        namespace,
    };
    let mut stream = conn.execute(sql, &options).await?;
    let mut rows = Vec::new();
    while let Some(batch) = stream.batches.next().await {
        rows.extend(batch?.rows);
        if rows.len() >= 10_000 {
            break;
        }
    }
    Ok(rows)
}

fn cell(row: &Row, i: usize) -> Option<String> {
    match row.get(i)? {
        Value::Null => None,
        Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        v => Some(v.to_string()),
    }
}

fn text_literal(kind: DriverKind, s: &str) -> String {
    edit::literal(kind, &Value::text(s))
}

/// MySQL's own `CREATE DATABASE`, with its character set.
async fn create_database(conn: &dyn Connection, db: &str) -> Option<String> {
    let sql = format!(
        "SHOW CREATE DATABASE {}",
        edit::quote_ident(DriverKind::Mysql, db)
    );
    let rows = query_rows(conn, &sql, None).await.ok()?;
    rows.first().and_then(|r| cell(r, 1))
}

/// The statements that make `object`'s triggers.
async fn triggers(conn: &dyn Connection, engine: Engine, object: &ObjectRef) -> Vec<String> {
    let kind = engine.0;
    match kind {
        DriverKind::Mysql => {
            let Some(db) = object.namespace.first() else {
                return Vec::new();
            };
            let sql = format!(
                "SELECT TRIGGER_NAME, ACTION_TIMING, EVENT_MANIPULATION, ACTION_STATEMENT \
                 FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA = {} \
                 AND EVENT_OBJECT_TABLE = {} ORDER BY ACTION_ORDER",
                text_literal(kind, db),
                text_literal(kind, &object.name)
            );
            let rows = query_rows(conn, &sql, None).await.unwrap_or_default();
            rows.iter()
                .filter_map(|r| {
                    Some(format!(
                        "CREATE TRIGGER {} {} {} ON {} FOR EACH ROW {}",
                        edit::quote_ident(kind, &cell(r, 0)?),
                        cell(r, 1)?,
                        cell(r, 2)?,
                        edit::quote_ident(kind, &object.name),
                        cell(r, 3)?
                    ))
                })
                .collect()
        }
        DriverKind::Postgres => {
            let Some(schema) = object.namespace.get(1) else {
                return Vec::new();
            };
            let sql = format!(
                "SELECT pg_catalog.pg_get_triggerdef(t.oid, true) FROM pg_catalog.pg_trigger t \
                 JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE NOT t.tgisinternal AND n.nspname = {} AND c.relname = {} ORDER BY t.tgname",
                text_literal(kind, schema),
                text_literal(kind, &object.name)
            );
            let rows = query_rows(conn, &sql, None).await.unwrap_or_default();
            rows.iter()
                .filter_map(|r| cell(r, 0))
                .map(|d| format!("{};", d.trim_end_matches(';')))
                .collect()
        }
        _ => Vec::new(),
    }
}

/// The functions and procedures of `namespace`, as statements that make them.
async fn routines(conn: &dyn Connection, engine: Engine, namespace: &[String]) -> Vec<String> {
    let Ok(objects) = conn.objects(namespace).await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in objects {
        let object = entry.object;
        let keyword = match object.kind {
            ObjectKind::Function => "FUNCTION",
            ObjectKind::Procedure => "PROCEDURE",
            _ => continue,
        };
        let Some(definition) = conn.describe(&object).await.ok().and_then(|i| i.definition) else {
            continue;
        };
        if engine.0 == DriverKind::Mysql {
            out.push(format!(
                "DROP {keyword} IF EXISTS {}",
                edit::quote_ident(engine.0, &object.name)
            ));
        }
        out.push(definition);
    }
    out
}

/// MySQL's scheduled events in `db`.
async fn events(conn: &dyn Connection, db: &str) -> Vec<String> {
    let kind = DriverKind::Mysql;
    let sql = format!(
        "SELECT EVENT_NAME FROM information_schema.EVENTS WHERE EVENT_SCHEMA = {} ORDER BY EVENT_NAME",
        text_literal(kind, db)
    );
    let names = query_rows(conn, &sql, None).await.unwrap_or_default();
    let mut out = Vec::new();
    for name in names.iter().filter_map(|r| cell(r, 0)) {
        let show = format!(
            "SHOW CREATE EVENT {}.{}",
            edit::quote_ident(kind, db),
            edit::quote_ident(kind, &name)
        );
        if let Some(create) = query_rows(conn, &show, None)
            .await
            .ok()
            .and_then(|rows| rows.first().and_then(|r| cell(r, 3)))
        {
            out.push(format!(
                "DROP EVENT IF EXISTS {}",
                edit::quote_ident(kind, &name)
            ));
            out.push(create);
        }
    }
    out
}

/// PostgreSQL sequences of `namespace`: their `CREATE SEQUENCE` before the tables whose
/// defaults use them, and their current value after the rows.
async fn sequences(conn: &dyn Connection, namespace: &[String]) -> Vec<(ObjectRef, String)> {
    let Ok(objects) = conn.objects(namespace).await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in objects {
        if entry.object.kind != ObjectKind::Sequence {
            continue;
        }
        if let Some(definition) = conn
            .describe(&entry.object)
            .await
            .ok()
            .and_then(|i| i.definition)
        {
            let definition = match definition.strip_prefix("CREATE SEQUENCE ") {
                Some(rest) => format!("CREATE SEQUENCE IF NOT EXISTS {rest}"),
                None => definition,
            };
            out.push((entry.object, definition));
        }
    }
    out
}

async fn sequence_value(conn: &dyn Connection, sequence: &ObjectRef) -> Option<String> {
    let name = edit::table_name(DriverKind::Postgres, sequence);
    let rows = query_rows(
        conn,
        &format!("SELECT last_value, is_called FROM {name}"),
        None,
    )
    .await
    .ok()?;
    let row = rows.first()?;
    let last = cell(row, 0)?;
    let called = matches!(row.get(1), Some(Value::Bool(true)))
        || cell(row, 1).is_some_and(|c| c == "t" || c == "true");
    Some(format!(
        "SELECT pg_catalog.setval({}, {last}, {called});",
        text_literal(DriverKind::Postgres, &name)
    ))
}

/// The file the export writes, plain or gzip.
enum Sink {
    Plain(BufWriter<File>),
    Gzip(GzEncoder<BufWriter<File>>),
}

impl Sink {
    fn create(path: &Path, gzip: bool) -> Result<Self, Error> {
        let file = File::create(path)
            .map_err(|e| Error::Other(format!("could not create {}: {e}", path.display())))?;
        let out = BufWriter::with_capacity(256 * 1024, file);
        Ok(if gzip {
            Sink::Gzip(GzEncoder::new(out, Compression::default()))
        } else {
            Sink::Plain(out)
        })
    }

    /// Ends the file and renames it from `temp` to `path`.
    fn finish(self, temp: &Path, path: &Path) -> Result<(), Error> {
        let io = |e: std::io::Error| Error::Other(format!("could not write the file: {e}"));
        let out = match self {
            Sink::Plain(out) => out,
            Sink::Gzip(gz) => gz.finish().map_err(io)?,
        };
        let file = out
            .into_inner()
            .map_err(|e| Error::Other(format!("could not write the file: {e}")))?;
        file.sync_all().map_err(io)?;
        drop(file);
        std::fs::rename(temp, path)
            .map_err(|e| Error::Other(format!("could not save {}: {e}", path.display())))
    }
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Sink::Plain(w) => w.write(buf),
            Sink::Gzip(w) => w.write(buf),
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            Sink::Plain(w) => w.write_all(buf),
            Sink::Gzip(w) => w.write_all(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // A gzip flush ends a block; the stream is flushed once, when it finishes.
        match self {
            Sink::Plain(w) => w.flush(),
            Sink::Gzip(_) => Ok(()),
        }
    }
}

/// An export, ready to run.
#[derive(Debug, Clone)]
pub struct Job {
    pub kind: DriverKind,
    pub options: Options,
    pub picks: Vec<Pick>,
    pub page_rows: u32,
    /// Rows per `INSERT`, or documents per `insertMany`.
    pub batch: usize,
    /// The file, or the folder for a file per table.
    pub path: PathBuf,
}

impl Job {
    fn gzip(&self) -> bool {
        self.options.output == Output::Gzip
    }

    /// The picks whose rows go out.
    fn with_data(&self) -> impl Iterator<Item = &Pick> {
        let data = self.options.format != Format::Script || self.options.data != DataMode::None;
        self.picks.iter().filter(move |p| p.data && data)
    }

    /// A format but the dump with more than one table fills a folder.
    pub fn to_folder(&self) -> bool {
        self.options.format != Format::Script && self.with_data().count() > 1
    }
}

fn write_text(out: &mut Sink, text: &str) -> Result<(), Error> {
    out.write_all(text.as_bytes())
        .map_err(|e| Error::Other(format!("could not write the export: {e}")))
}

#[allow(clippy::too_many_arguments)]
fn write_rows(
    handle: &tokio::runtime::Handle,
    conn: &Arc<dyn Connection>,
    pick: &Pick,
    mut writer: Box<dyn RowWriter>,
    page_rows: u32,
    out: &mut Sink,
    before: u64,
    progress: &Progress,
) -> Result<u64, Error> {
    if progress.cancelled() {
        return Err(Error::Cancelled);
    }
    let loader = BrowseLoader::new(
        conn.clone(),
        pick.object.clone(),
        pick.query.clone().unwrap_or_default(),
        BrowseConfig::default(),
    );
    let options = ExportOptions {
        page_rows: page_rows.max(1),
        columns: pick.columns.clone(),
    };
    handle.block_on(deltos_grid::export(
        &loader,
        &mut *writer,
        &options,
        &mut *out,
        |n| {
            progress.rows.store(before + n, Ordering::Relaxed);
            !progress.cancelled()
        },
    ))
}

/// Runs `job` on the calling thread, which may block.
pub fn run(
    handle: &tokio::runtime::Handle,
    conn: &Arc<dyn Connection>,
    job: &Job,
    progress: &Progress,
) -> Outcome {
    let total: u64 = job.with_data().filter_map(|p| p.estimated_rows).sum();
    progress.total_rows.store(total, Ordering::Relaxed);
    let tables = job.picks.iter().filter(|p| p.structure || p.data).count() as u64;
    if tables == 0 {
        return Outcome::failed("Nothing is selected to export");
    }
    let what = count(tables, "table", "tables");
    let failure = |e: Error| match e {
        Error::Cancelled => Outcome::failed(format!(
            "Cancelled after {}; nothing was saved",
            count(progress.rows.load(Ordering::Relaxed), "row", "rows")
        )),
        e => Outcome::failed(format!("Export failed: {e}")),
    };
    if job.options.format == Format::Script {
        let temp = temp_path(&job.path);
        let result = Sink::create(&temp, job.gzip()).and_then(|mut out| {
            let rows = dump(handle, conn, job, &mut out, progress)?;
            out.finish(&temp, &job.path)?;
            Ok(rows)
        });
        return match result {
            Ok(rows) => Outcome {
                file: Some(job.path.clone()),
                ..Outcome::done(format!(
                    "Exported {what} and {} to {}",
                    count(rows, "row", "rows"),
                    file_label(&job.path)
                ))
            },
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                failure(e)
            }
        };
    }
    let picks: Vec<&Pick> = job.with_data().collect();
    if picks.is_empty() {
        return Outcome::failed("No table has Data selected");
    }
    let folder = job.to_folder();
    if folder && let Err(e) = std::fs::create_dir_all(&job.path) {
        return Outcome::failed(format!("Could not create {}: {e}", job.path.display()));
    }
    let qualify = picks
        .iter()
        .any(|p| p.object.namespace != picks[0].object.namespace);
    let mut extension = job.options.format.extension(job.kind).to_owned();
    if job.gzip() {
        extension.push_str(".gz");
    }
    let mut rows = 0u64;
    for pick in &picks {
        let path = if folder {
            job.path.join(super::database::file_name(
                &pick.object,
                qualify,
                &extension,
            ))
        } else {
            job.path.clone()
        };
        let temp = temp_path(&path);
        let writer: Box<dyn RowWriter> = match job.options.format.csv() {
            Some(delimiter) => Box::new(CsvWriter::new(CsvFormat {
                delimiter,
                header: true,
                quote: QuoteStyle::Necessary,
                null_text: String::new(),
                line_end: "\r\n",
            })),
            None => Box::new(JsonWriter::new(JsonStyle::Array, false)),
        };
        let result = Sink::create(&temp, job.gzip()).and_then(|mut out| {
            let n = write_rows(
                handle,
                conn,
                pick,
                writer,
                job.page_rows,
                &mut out,
                rows,
                progress,
            )?;
            out.finish(&temp, &path)?;
            Ok(n)
        });
        match result {
            Ok(n) => rows += n,
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                let mut outcome = failure(e);
                if folder {
                    outcome.message = format!(
                        "{} ({}); the files finished before it are kept",
                        outcome.message, pick.object.name
                    );
                }
                return outcome;
            }
        }
    }
    Outcome {
        file: Some(job.path.clone()),
        ..Outcome::done(format!(
            "Exported {} and {} to {}",
            count(picks.len() as u64, "table", "tables"),
            count(rows, "row", "rows"),
            file_label(&job.path)
        ))
    }
}

/// Where the dump is: the namespaces it has entered.
struct Cursor {
    engine: Engine,
    mode: DatabaseMode,
    current: Option<Vec<String>>,
    entered: Vec<Vec<String>>,
}

impl Cursor {
    /// Moves the dump into `namespace`, writing what that takes.
    fn enter(
        &mut self,
        handle: &tokio::runtime::Handle,
        conn: &dyn Connection,
        namespace: &[String],
        out: &mut Sink,
    ) -> Result<(), Error> {
        if self.current.as_deref() == Some(namespace) {
            return Ok(());
        }
        let first = !self.entered.iter().any(|n| n == namespace);
        let create = if first
            && self.engine.0 == DriverKind::Mysql
            && matches!(self.mode, DatabaseMode::Create | DatabaseMode::DropCreate)
        {
            namespace
                .first()
                .and_then(|db| handle.block_on(create_database(conn, db)))
        } else {
            None
        };
        let text = enter_namespace(self.engine, namespace, self.mode, first, create.as_deref());
        if !text.is_empty() {
            write_text(out, &format!("{text}\n"))?;
        }
        if first {
            self.entered.push(namespace.to_vec());
        }
        self.current = Some(namespace.to_vec());
        Ok(())
    }
}

/// Writes the dump into `out`. Returns the rows written.
fn dump(
    handle: &tokio::runtime::Handle,
    conn: &Arc<dyn Connection>,
    job: &Job,
    out: &mut Sink,
    progress: &Progress,
) -> Result<u64, Error> {
    let engine = Engine(job.kind);
    let o = &job.options;
    let picks: Vec<&Pick> = job
        .picks
        .iter()
        .filter(|p| {
            (p.structure && o.tables != TablesMode::None) || (p.data && o.data != DataMode::None)
        })
        .collect();
    let mut namespaces: Vec<Vec<String>> = Vec::new();
    for p in &job.picks {
        if !namespaces.contains(&p.object.namespace) {
            namespaces.push(p.object.namespace.clone());
        }
    }
    let mode = effective_database(engine, o.database, namespaces.len());

    // Each table's definition, keys and foreign keys, one at a time.
    let mut infos: HashMap<ObjectRef, ObjectInfo> = HashMap::new();
    let mut tables: Vec<Table> = Vec::new();
    let mut views: Vec<&Pick> = Vec::new();
    for p in &picks {
        if progress.cancelled() {
            return Err(Error::Cancelled);
        }
        let info = handle
            .block_on(conn.describe(&p.object))
            .unwrap_or_default();
        if p.is_view() {
            views.push(p);
        } else {
            tables.push(Table {
                object: p.object.clone(),
                definition: None,
                estimated_rows: p.estimated_rows,
                refers_to: info
                    .foreign_keys
                    .iter()
                    .map(|k| k.target.clone())
                    .filter(|t| *t != p.object)
                    .collect(),
            });
        }
        infos.insert(p.object.clone(), info);
    }
    let by_object: HashMap<&ObjectRef, &Pick> = picks.iter().map(|p| (&p.object, *p)).collect();
    let ordered: Vec<&Pick> = in_dependency_order(tables)
        .iter()
        .filter_map(|t| by_object.get(&t.object).copied())
        .collect();

    write_text(out, &header(engine))?;
    let mut cursor = Cursor {
        engine,
        mode,
        current: None,
        entered: Vec::new(),
    };
    // The database sections come first, as Adminer writes them, even for a database
    // whose tables are all left out, so its routines still have a place to go.
    for ns in &namespaces {
        cursor.enter(handle, conn.as_ref(), ns, out)?;
    }
    let structure = |p: &Pick| p.structure && o.tables != TablesMode::None;
    let drop = o.tables == TablesMode::DropCreate;

    // PostgreSQL drops every table first, since a table takes its sequences with it, then
    // makes the sequences the new tables' defaults use.
    let mut sequence_list: Vec<(ObjectRef, String)> = Vec::new();
    if engine.0 == DriverKind::Postgres && o.tables != TablesMode::None {
        if drop {
            let mut drops = String::new();
            for p in ordered.iter().rev().filter(|p| structure(p)) {
                drops.push_str(&drop_statement(engine, &p.object, mode));
            }
            if !drops.is_empty() {
                write_text(out, &format!("{drops}\n"))?;
            }
        }
        for ns in &namespaces {
            sequence_list.extend(handle.block_on(sequences(conn.as_ref(), ns)));
        }
        if !sequence_list.is_empty() {
            let text: String = sequence_list
                .iter()
                .map(|(_, d)| format!("{}\n", d.trim()))
                .collect();
            write_text(out, &format!("{text}\n"))?;
        }
    }

    let mut rows = 0u64;
    let mut deferred_triggers: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    for p in &ordered {
        cursor.enter(handle, conn.as_ref(), &p.object.namespace, out)?;
        let info = infos.get(&p.object);
        let mut text = String::new();
        if structure(p) {
            if drop && engine.0 != DriverKind::Postgres {
                text.push_str(&drop_statement(engine, &p.object, mode));
            }
            text.push_str(&self::structure(
                engine,
                &p.object,
                info.and_then(|i| i.definition.as_deref()),
                o,
                mode,
            ));
            text.push('\n');
            if o.triggers && engine.triggers() {
                let list = match engine.0 {
                    DriverKind::Sqlite => info
                        .and_then(|i| i.definition.as_deref())
                        .map(|d| split_sqlite(d).1)
                        .unwrap_or_default(),
                    _ => handle.block_on(triggers(conn.as_ref(), engine, &p.object)),
                };
                if !list.is_empty() {
                    deferred_triggers.push((p.object.namespace.clone(), list));
                }
            }
        }
        if p.data && o.data != DataMode::None {
            let dml = dml_object(engine.0, &p.object);
            let collection =
                mongo_collection(mongo_database(mode, &p.object.namespace), &p.object.name);
            // A table just made is empty already.
            if o.data == DataMode::TruncateInsert && !(structure(p) && drop) {
                if engine.mongo() {
                    text.push_str(&format!("{collection}.deleteMany({{}});\n"));
                } else {
                    text.push_str(&format!("{};\n", truncate_statement(engine.0, &dml)));
                }
            }
            write_text(out, &text)?;
            text.clear();
            let writer: Box<dyn RowWriter> = if engine.mongo() {
                let mode = if o.data == DataMode::InsertUpdate {
                    MongoMode::Upsert
                } else {
                    MongoMode::Insert
                };
                Box::new(MongoWriter::new(collection, mode, job.batch))
            } else {
                let w = InsertWriter::new(engine.0, &dml, job.batch);
                Box::new(if o.data == DataMode::InsertUpdate {
                    let key = info
                        .map(|i| i.primary_key().iter().map(|c| c.name.clone()).collect())
                        .unwrap_or_default();
                    w.with_upsert(key)
                } else {
                    w
                })
            };
            rows += write_rows(handle, conn, p, writer, job.page_rows, out, rows, progress)?;
            text.push('\n');
        }
        write_text(out, &text)?;
    }

    // Routines and events after the tables, since PostgreSQL checks a SQL function's
    // body against the tables when it is made.
    for ns in &namespaces {
        let mut statements = Vec::new();
        if o.routines && engine.routines() {
            statements.extend(handle.block_on(routines(conn.as_ref(), engine, ns)));
        }
        if o.events
            && engine.events()
            && let Some(db) = ns.first()
        {
            statements.extend(handle.block_on(events(conn.as_ref(), db)));
        }
        if statements.is_empty() {
            continue;
        }
        cursor.enter(handle, conn.as_ref(), ns, out)?;
        let text = if engine.0 == DriverKind::Mysql {
            delimited(&statements)
        } else {
            statements
                .iter()
                .map(|s| format!("{};\n", s.trim().trim_end_matches(';')))
                .collect()
        };
        write_text(out, &format!("{text}\n"))?;
    }

    for p in &views {
        cursor.enter(handle, conn.as_ref(), &p.object.namespace, out)?;
        let mut text = String::new();
        if structure(p) {
            if drop && engine.0 != DriverKind::Postgres {
                text.push_str(&drop_statement(engine, &p.object, mode));
            }
            text.push_str(&self::structure(
                engine,
                &p.object,
                infos.get(&p.object).and_then(|i| i.definition.as_deref()),
                o,
                mode,
            ));
            text.push('\n');
        }
        write_text(out, &text)?;
    }

    // Triggers last, so loading the rows does not fire them, and after the functions a
    // PostgreSQL trigger calls.
    for (ns, list) in deferred_triggers {
        cursor.enter(handle, conn.as_ref(), &ns, out)?;
        let text = if engine.0 == DriverKind::Mysql {
            delimited(&list)
        } else {
            list.iter().map(|s| format!("{s}\n")).collect()
        };
        write_text(out, &format!("{text}\n"))?;
    }

    // The sequences carry on from the rows loaded.
    if o.data != DataMode::None {
        let mut text = String::new();
        for (sequence, _) in &sequence_list {
            if let Some(line) = handle.block_on(sequence_value(conn.as_ref(), sequence)) {
                text.push_str(&line);
                text.push('\n');
            }
        }
        if !text.is_empty() {
            write_text(out, &format!("{text}\n"))?;
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(kind: DriverKind, name: &str) -> ObjectRef {
        let namespace = match kind {
            DriverKind::Postgres => vec!["shop".into(), "public".into()],
            DriverKind::Sqlite => vec!["main".into()],
            _ => vec!["shop".into()],
        };
        ObjectRef {
            namespace,
            name: name.into(),
            kind: ObjectKind::Table,
        }
    }

    #[test]
    fn several_databases_name_theirs() {
        let mysql = Engine(DriverKind::Mysql);
        assert_eq!(
            effective_database(mysql, DatabaseMode::None, 1),
            DatabaseMode::None
        );
        assert_eq!(
            effective_database(mysql, DatabaseMode::None, 2),
            DatabaseMode::Use
        );
        assert_eq!(
            effective_database(mysql, DatabaseMode::Create, 2),
            DatabaseMode::Create
        );
        assert_eq!(
            effective_database(Engine(DriverKind::Sqlite), DatabaseMode::Use, 1),
            DatabaseMode::None
        );
    }

    #[test]
    fn databases_are_entered_per_engine() {
        let ns = |kind| object(kind, "t").namespace;
        let mysql = Engine(DriverKind::Mysql);
        assert_eq!(
            enter_namespace(
                mysql,
                &ns(DriverKind::Mysql),
                DatabaseMode::DropCreate,
                true,
                Some("CREATE DATABASE `shop` /*!40100 DEFAULT CHARACTER SET utf8mb4 */")
            ),
            "DROP DATABASE IF EXISTS `shop`;\nCREATE DATABASE `shop` /*!40100 DEFAULT CHARACTER SET utf8mb4 */;\nUSE `shop`;\n"
        );
        assert_eq!(
            enter_namespace(
                mysql,
                &ns(DriverKind::Mysql),
                DatabaseMode::Create,
                false,
                None
            ),
            "USE `shop`;\n"
        );
        assert_eq!(
            enter_namespace(
                mysql,
                &ns(DriverKind::Mysql),
                DatabaseMode::None,
                true,
                None
            ),
            ""
        );
        let pg = Engine(DriverKind::Postgres);
        assert_eq!(
            enter_namespace(
                pg,
                &ns(DriverKind::Postgres),
                DatabaseMode::Create,
                true,
                None
            ),
            "CREATE SCHEMA IF NOT EXISTS \"public\";\nSET search_path TO \"public\";\n"
        );
        let mongo = Engine(DriverKind::Mongodb);
        assert_eq!(
            enter_namespace(
                mongo,
                &ns(DriverKind::Mongodb),
                DatabaseMode::DropCreate,
                true,
                None
            ),
            "db.getSiblingDB(\"shop\").dropDatabase();\n"
        );
    }

    #[test]
    fn drops_name_the_object_as_the_dump_does() {
        let o = DatabaseMode::None;
        assert_eq!(
            drop_statement(
                Engine(DriverKind::Mysql),
                &object(DriverKind::Mysql, "t"),
                o
            ),
            "DROP TABLE IF EXISTS `t`;\n"
        );
        assert_eq!(
            drop_statement(
                Engine(DriverKind::Postgres),
                &object(DriverKind::Postgres, "t"),
                o
            ),
            "DROP TABLE IF EXISTS \"public\".\"t\" CASCADE;\n"
        );
        assert_eq!(
            drop_statement(
                Engine(DriverKind::Mongodb),
                &object(DriverKind::Mongodb, "t"),
                DatabaseMode::Use
            ),
            "db.getSiblingDB(\"shop\").getCollection(\"t\").drop();\n"
        );
    }

    #[test]
    fn auto_increment_counters_come_out() {
        let def = "CREATE TABLE `t` (\n  `id` int NOT NULL AUTO_INCREMENT,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB AUTO_INCREMENT=1312 DEFAULT CHARSET=utf8mb4;";
        assert_eq!(
            strip_auto_increment(def),
            "CREATE TABLE `t` (\n  `id` int NOT NULL AUTO_INCREMENT,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;"
        );
        let mut options = Options::default();
        let mysql = Engine(DriverKind::Mysql);
        let t = object(DriverKind::Mysql, "t");
        assert!(!structure(mysql, &t, Some(def), &options, DatabaseMode::None).contains("=1312"));
        options.auto_increment = true;
        assert!(structure(mysql, &t, Some(def), &options, DatabaseMode::None).contains("=1312"));
        assert_eq!(
            structure(mysql, &t, None, &options, DatabaseMode::None),
            "-- t: the database gave no definition\n"
        );
    }

    #[test]
    fn sqlite_triggers_come_apart() {
        let def = "CREATE TABLE t (id INTEGER);\n\nCREATE INDEX t_id ON t (id);\n\n\
                   CREATE TRIGGER t_log AFTER INSERT ON t BEGIN SELECT 1; END;";
        let (rest, triggers) = split_sqlite(def);
        assert_eq!(
            rest,
            "CREATE TABLE t (id INTEGER);\n\nCREATE INDEX t_id ON t (id);"
        );
        assert_eq!(
            triggers,
            ["CREATE TRIGGER t_log AFTER INSERT ON t BEGIN SELECT 1; END;"]
        );
        let sqlite = Engine(DriverKind::Sqlite);
        let t = object(DriverKind::Sqlite, "t");
        let text = structure(
            sqlite,
            &t,
            Some(def),
            &Options::default(),
            DatabaseMode::None,
        );
        assert!(!text.contains("t_log"), "{text}");
    }

    #[test]
    fn mongo_definitions_name_the_database_when_asked() {
        let def = "db.createCollection(\"t\");\ndb.getCollection(\"t\").createIndex({\"a\": 1}, {\"name\": \"a_1\"});\n";
        let mongo = Engine(DriverKind::Mongodb);
        let t = object(DriverKind::Mongodb, "t");
        let options = Options::default();
        assert_eq!(
            structure(mongo, &t, Some(def), &options, DatabaseMode::None),
            def
        );
        assert_eq!(
            structure(mongo, &t, Some(def), &options, DatabaseMode::Use),
            "db.getSiblingDB(\"shop\").createCollection(\"t\");\n\
             db.getSiblingDB(\"shop\").getCollection(\"t\").createIndex({\"a\": 1}, {\"name\": \"a_1\"});\n"
        );
    }

    #[test]
    fn routine_bodies_go_between_delimiters() {
        assert_eq!(
            delimited(&[
                "DROP FUNCTION IF EXISTS `f`".into(),
                "CREATE FUNCTION `f`() RETURNS int BEGIN RETURN 1; END;".into()
            ]),
            "DELIMITER ;;\nDROP FUNCTION IF EXISTS `f`;;\nCREATE FUNCTION `f`() RETURNS int BEGIN RETURN 1; END;;\nDELIMITER ;\n"
        );
        assert_eq!(delimited(&[]), "");
    }

    #[test]
    fn engines_offer_what_they_have() {
        let mysql = Engine(DriverKind::Mysql);
        assert!(mysql.routines() && mysql.events() && mysql.auto_increment() && mysql.triggers());
        let pg = Engine(DriverKind::Postgres);
        assert_eq!(pg.database_label(), Some("Schema"));
        assert!(pg.routines() && !pg.events() && !pg.auto_increment());
        let sqlite = Engine(DriverKind::Sqlite);
        assert_eq!(sqlite.database_label(), None);
        assert!(sqlite.triggers() && !sqlite.routines());
        let mongo = Engine(DriverKind::Mongodb);
        assert_eq!(mongo.script_label(), "Shell");
        assert_eq!(mongo.database_modes().len(), 3);
        assert_eq!(Format::Script.extension(DriverKind::Mongodb), "js");
        assert_eq!(Format::CsvSemicolon.extension(DriverKind::Mysql), "csv");
    }

    fn sqlite(rt: &tokio::runtime::Runtime, path: &Path) -> Arc<dyn Connection> {
        let _ = std::fs::remove_file(path);
        rt.block_on(async {
            let driver = deltos_drivers::driver(DriverKind::Sqlite).unwrap();
            let config = deltos_data::ConnectionConfig {
                database: path.to_string_lossy().into_owned(),
                options: vec![("create".into(), "true".into())],
                ..Default::default()
            };
            driver.connect(&config).await
        })
        .unwrap()
    }

    fn run_sql(rt: &tokio::runtime::Runtime, conn: &Arc<dyn Connection>, sql: &str) {
        let options = ExecuteOptions::default();
        for range in split_statements(sql, Dialect::Sqlite) {
            let statement = &sql[range];
            rt.block_on(conn.execute(statement, &options))
                .unwrap_or_else(|e| panic!("{e}: {statement}"));
        }
    }

    fn scalar(rt: &tokio::runtime::Runtime, conn: &Arc<dyn Connection>, sql: &str) -> String {
        let rows = rt.block_on(query_rows(conn.as_ref(), sql, None)).unwrap();
        cell(&rows[0], 0).unwrap_or_default()
    }

    /// A SQLite database dumped with every data mode, gzipped and plain, and run back;
    /// per-table structure and data choices; CSV of several tables to a folder.
    #[test]
    fn dumps_run_back_with_each_option() {
        let dir = std::env::temp_dir().join(format!("deltos-dump-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let conn = sqlite(&rt, &dir.join("shop.db"));
        run_sql(
            &rt,
            &conn,
            "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE accounts (id INTEGER PRIMARY KEY, owner INTEGER REFERENCES people(id));
             CREATE TABLE log (msg TEXT);
             CREATE TRIGGER people_log AFTER INSERT ON people BEGIN INSERT INTO log VALUES ('added'); END;
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 700)
               INSERT INTO people SELECT i, 'person ' || i FROM n;
             INSERT INTO accounts SELECT id, id FROM people WHERE id <= 200;
             CREATE VIEW names AS SELECT name FROM people;",
        );
        let listed = rt.block_on(conn.objects(&["main".to_string()])).unwrap();
        let picks: Vec<Pick> = listed
            .into_iter()
            .filter(|e| e.object.kind.has_rows())
            .map(|e| Pick::new(e.object, e.estimated_rows))
            .collect();
        assert_eq!(picks.len(), 4);
        let mut job = Job {
            kind: DriverKind::Sqlite,
            options: Options::default(),
            picks,
            page_rows: 250,
            batch: 100,
            path: dir.join("shop.sql"),
        };

        // The default: DROP+CREATE and INSERT, with the view and trigger.
        let outcome = run(rt.handle(), &conn, &job, &Progress::default());
        assert!(!outcome.failed, "{}", outcome.message);
        assert_eq!(
            outcome.message,
            "Exported 4 tables and 1,600 rows to shop.sql"
        );
        let sql = std::fs::read_to_string(&job.path).unwrap();
        assert!(sql.starts_with("-- Deltos "), "{sql}");
        let people = sql.find("CREATE TABLE people").unwrap();
        let accounts = sql.find("CREATE TABLE accounts").unwrap();
        assert!(people < accounts, "referenced tables come first");
        assert!(
            sql.find("CREATE VIEW").unwrap() > accounts,
            "views come last"
        );
        let copy = sqlite(&rt, &dir.join("copy.db"));
        run_sql(&rt, &copy, &sql);
        assert_eq!(scalar(&rt, &copy, "SELECT count(*) FROM people"), "700");
        // The trigger came with the table, and the log's rows with the log: none were
        // added by the trigger as the people went in, since the log had them already.
        assert_eq!(scalar(&rt, &copy, "SELECT count(*) FROM log"), "700");
        // Run again over itself: DROP+CREATE makes it so.
        run_sql(&rt, &copy, &sql);
        assert_eq!(scalar(&rt, &copy, "SELECT count(*) FROM accounts"), "200");

        // INSERT+UPDATE over existing rows, gzipped, without structure.
        run_sql(
            &rt,
            &conn,
            "UPDATE people SET name = 'renamed' WHERE id = 5",
        );
        job.options.tables = TablesMode::None;
        job.options.data = DataMode::InsertUpdate;
        job.options.output = Output::Gzip;
        job.path = dir.join("upsert.sql.gz");
        let outcome = run(rt.handle(), &conn, &job, &Progress::default());
        assert!(!outcome.failed, "{}", outcome.message);
        let mut text = String::new();
        std::io::Read::read_to_string(
            &mut flate2::read::GzDecoder::new(File::open(&job.path).unwrap()),
            &mut text,
        )
        .unwrap();
        assert!(!text.contains("CREATE TABLE"));
        assert!(
            text.contains("ON CONFLICT (\"id\") DO UPDATE SET"),
            "{text}"
        );
        run_sql(&rt, &copy, &text);
        assert_eq!(
            scalar(&rt, &copy, "SELECT name FROM people WHERE id = 5"),
            "renamed"
        );
        assert_eq!(scalar(&rt, &copy, "SELECT count(*) FROM people"), "700");

        // Structure of one table only, and data of another, chosen per table.
        for p in &mut job.picks {
            p.structure = p.object.name == "log";
            p.data = p.object.name == "accounts";
        }
        job.options.tables = TablesMode::Create;
        job.options.data = DataMode::TruncateInsert;
        job.options.triggers = false;
        job.options.output = Output::Save;
        job.path = dir.join("some.sql");
        let outcome = run(rt.handle(), &conn, &job, &Progress::default());
        assert!(!outcome.failed, "{}", outcome.message);
        let sql = std::fs::read_to_string(&job.path).unwrap();
        assert!(sql.contains("CREATE TABLE log"), "{sql}");
        assert!(!sql.contains("CREATE TABLE people"), "{sql}");
        assert!(!sql.contains("DROP TABLE"), "{sql}");
        assert!(!sql.contains("TRIGGER"), "{sql}");
        assert!(sql.contains("DELETE FROM \"accounts\";"), "{sql}");
        assert_eq!(sql.matches("INSERT INTO \"accounts\"").count(), 2);
        assert!(!sql.contains("INSERT INTO \"people\""), "{sql}");

        // CSV with a semicolon: a file per table with Data, in a folder.
        job.options.format = Format::CsvSemicolon;
        for p in &mut job.picks {
            p.data = p.object.name != "names";
        }
        job.path = dir.join("csv");
        assert!(job.to_folder());
        let outcome = run(rt.handle(), &conn, &job, &Progress::default());
        assert!(!outcome.failed, "{}", outcome.message);
        let people = std::fs::read_to_string(job.path.join("people.csv")).unwrap();
        assert_eq!(people.lines().count(), 701);
        assert!(people.starts_with("id;name\r\n1;person 1\r\n"), "{people}");
        assert!(!job.path.join("names.csv").exists());

        // TSV of one table: one file.
        job.options.format = Format::Tsv;
        for p in &mut job.picks {
            p.data = p.object.name == "accounts";
        }
        job.path = dir.join("accounts.tsv");
        assert!(!job.to_folder());
        let outcome = run(rt.handle(), &conn, &job, &Progress::default());
        assert!(!outcome.failed, "{}", outcome.message);
        let tsv = std::fs::read_to_string(&job.path).unwrap();
        assert!(tsv.starts_with("id\towner\r\n"), "{tsv}");

        // Nothing chosen.
        for p in &mut job.picks {
            p.structure = false;
            p.data = false;
        }
        assert!(run(rt.handle(), &conn, &job, &Progress::default()).failed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A connection from `DELTOS_TEST_<NAME>_URL`, or `None` to skip.
    fn live(
        rt: &tokio::runtime::Runtime,
        kind: DriverKind,
        var: &str,
    ) -> Option<Arc<dyn Connection>> {
        let url = std::env::var(var).ok().filter(|u| !u.trim().is_empty())?;
        let config = deltos_data::ConnectionConfig {
            url: Some(url),
            ..Default::default()
        };
        let driver = deltos_drivers::driver(kind)?;
        Some(rt.block_on(driver.connect(&config)).expect("connect"))
    }

    fn run_script(
        rt: &tokio::runtime::Runtime,
        conn: &Arc<dyn Connection>,
        dialect: Option<Dialect>,
        text: &str,
    ) {
        let options = ExecuteOptions::default();
        let statements: Vec<String> = match dialect {
            Some(d) => split_statements(text, d)
                .into_iter()
                .map(|r| text[r].to_owned())
                .collect(),
            // The MongoDB dump ends each call at the end of a line.
            None => {
                let mut out = Vec::new();
                let mut current = String::new();
                for line in text.lines().filter(|l| !l.starts_with("//")) {
                    current.push_str(line);
                    current.push('\n');
                    if line.trim_end().ends_with(';') {
                        out.push(std::mem::take(&mut current));
                    }
                }
                out
            }
        };
        for statement in statements {
            let result = rt.block_on(async {
                use futures::StreamExt;
                let mut stream = conn.execute(&statement, &options).await?;
                while let Some(b) = stream.batches.next().await {
                    b?;
                }
                Ok::<_, Error>(())
            });
            result.unwrap_or_else(|e| panic!("{e}: {statement}"));
        }
    }

    fn dump_text(
        rt: &tokio::runtime::Runtime,
        conn: &Arc<dyn Connection>,
        kind: DriverKind,
        under: &[String],
        options: Options,
        path: PathBuf,
    ) -> String {
        let listed = rt
            .block_on(super::super::database::entries(
                conn.as_ref(),
                &[under.to_vec()],
                true,
            ))
            .unwrap();
        let job = Job {
            kind,
            options,
            picks: listed
                .into_iter()
                .map(|e| Pick::new(e.object, e.estimated_rows))
                .collect(),
            page_rows: 100,
            batch: 50,
            path: path.clone(),
        };
        let outcome = run(rt.handle(), conn, &job, &Progress::default());
        assert!(!outcome.failed, "{}", outcome.message);
        std::fs::read_to_string(&path).unwrap()
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    /// A MySQL database with a foreign key, a trigger, a routine and an event, dumped with
    /// DROP+CREATE for the database and run back.
    #[test]
    fn mysql_dumps_run_back() {
        let rt = runtime();
        let Some(conn) = live(&rt, DriverKind::Mysql, "DELTOS_TEST_MYSQL_URL") else {
            return;
        };
        let db = format!("deltos_dump_{}", std::process::id());
        run_script(
            &rt,
            &conn,
            Some(Dialect::Mysql),
            &format!(
                "DROP DATABASE IF EXISTS `{db}`;
                 CREATE DATABASE `{db}`;
                 USE `{db}`;
                 CREATE TABLE people (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(40));
                 CREATE TABLE accounts (id INT AUTO_INCREMENT PRIMARY KEY, owner INT, FOREIGN KEY (owner) REFERENCES people(id));
                 CREATE TABLE log (msg TEXT);
                 CREATE TRIGGER people_log AFTER INSERT ON people FOR EACH ROW INSERT INTO log VALUES (NEW.name);
DELIMITER ;;
CREATE FUNCTION twice(x INT) RETURNS INT DETERMINISTIC BEGIN RETURN x * 2; END;;
DELIMITER ;
                 CREATE EVENT tidy ON SCHEDULE EVERY 1 DAY STARTS CURRENT_TIMESTAMP + INTERVAL 1 DAY DO DELETE FROM log;
                 INSERT INTO people (name) VALUES ('a'), ('b'), ('it''s');
                 INSERT INTO accounts (owner) VALUES (1), (3);
                 CREATE VIEW names AS SELECT name FROM people;"
            ),
        );
        let path = std::env::temp_dir().join(format!("{db}.sql"));
        let options = Options {
            database: DatabaseMode::DropCreate,
            ..Options::default()
        };
        let sql = dump_text(
            &rt,
            &conn,
            DriverKind::Mysql,
            std::slice::from_ref(&db),
            options,
            path.clone(),
        );
        assert!(
            sql.contains(&format!("DROP DATABASE IF EXISTS `{db}`")),
            "{sql}"
        );
        assert!(sql.contains("CREATE TRIGGER `people_log`"), "{sql}");
        assert!(sql.contains("FUNCTION `twice`"), "{sql}");
        assert!(sql.contains("CREATE DEFINER"), "{sql}");
        assert!(sql.contains("EVENT `tidy`"), "{sql}");
        assert!(!sql.contains("AUTO_INCREMENT=4"), "{sql}");
        run_script(&rt, &conn, Some(Dialect::Mysql), &sql);
        let rows = rt
            .block_on(query_rows(
                conn.as_ref(),
                &format!("SELECT (SELECT count(*) FROM `{db}`.people), (SELECT count(*) FROM `{db}`.log), `{db}`.twice(21)"),
                None,
            ))
            .unwrap();
        assert_eq!(cell(&rows[0], 0).as_deref(), Some("3"));
        // The log's rows came from the dump, not from the trigger firing again.
        assert_eq!(cell(&rows[0], 1).as_deref(), Some("3"));
        assert_eq!(cell(&rows[0], 2).as_deref(), Some("42"));
        run_script(
            &rt,
            &conn,
            Some(Dialect::Mysql),
            &format!("DROP DATABASE `{db}`"),
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A PostgreSQL schema with a serial key, a trigger and its function, dumped and run
    /// back over itself.
    #[test]
    fn postgres_dumps_run_back() {
        let rt = runtime();
        let Some(conn) = live(&rt, DriverKind::Postgres, "DELTOS_TEST_POSTGRES_URL") else {
            return;
        };
        let schema = format!("deltos_dump_{}", std::process::id());
        let db = rt
            .block_on(query_rows(conn.as_ref(), "SELECT current_database()", None))
            .ok()
            .and_then(|r| cell(&r[0], 0))
            .unwrap();
        run_script(
            &rt,
            &conn,
            Some(Dialect::Postgres),
            &format!(
                "DROP SCHEMA IF EXISTS {schema} CASCADE;
                 CREATE SCHEMA {schema};
                 CREATE TABLE {schema}.people (id serial PRIMARY KEY, name text);
                 CREATE TABLE {schema}.accounts (id serial PRIMARY KEY, owner int REFERENCES {schema}.people(id));
                 CREATE TABLE {schema}.log (msg text);
                 CREATE FUNCTION {schema}.note() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO {schema}.log VALUES (NEW.name); RETURN NEW; END $$;
                 CREATE TRIGGER people_log AFTER INSERT ON {schema}.people FOR EACH ROW EXECUTE FUNCTION {schema}.note();
                 INSERT INTO {schema}.people (name) VALUES ('a'), ('b'), ('it''s');
                 INSERT INTO {schema}.accounts (owner) VALUES (1), (3);"
            ),
        );
        let path = std::env::temp_dir().join(format!("{schema}.sql"));
        let options = Options {
            database: DatabaseMode::Create,
            ..Options::default()
        };
        let sql = dump_text(
            &rt,
            &conn,
            DriverKind::Postgres,
            &[db, schema.clone()],
            options,
            path.clone(),
        );
        assert!(sql.contains("CREATE TRIGGER people_log"), "{sql}");
        assert!(sql.contains("setval"), "{sql}");
        run_script(&rt, &conn, Some(Dialect::Postgres), &sql);
        run_script(
            &rt,
            &conn,
            Some(Dialect::Postgres),
            &format!("INSERT INTO {schema}.people (name) VALUES ('d')"),
        );
        let rows = rt
            .block_on(query_rows(
                conn.as_ref(),
                &format!("SELECT (SELECT max(id) FROM {schema}.people), (SELECT count(*) FROM {schema}.log)"),
                None,
            ))
            .unwrap();
        // The sequence carried on after the rows, and the trigger came back.
        assert_eq!(cell(&rows[0], 0).as_deref(), Some("4"));
        assert_eq!(cell(&rows[0], 1).as_deref(), Some("4"));
        run_script(
            &rt,
            &conn,
            Some(Dialect::Postgres),
            &format!("DROP SCHEMA {schema} CASCADE"),
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A MongoDB database dumped as shell calls, with DROP+CREATE and INSERT+UPDATE, run
    /// back through Deltos' shell.
    #[test]
    fn mongo_dumps_run_back() {
        let rt = runtime();
        let Some(conn) = live(&rt, DriverKind::Mongodb, "DELTOS_TEST_MONGODB_URL") else {
            return;
        };
        let db = format!("deltos_dump_{}", std::process::id());
        run_script(
            &rt,
            &conn,
            None,
            &format!(
                "db.getSiblingDB(\"{db}\").dropDatabase();
                 db.getSiblingDB(\"{db}\").people.insertMany([{{name: \"a\", age: 3, tags: [\"x\"]}}, {{name: \"b\", when: ISODate(\"2024-01-02T03:04:05Z\")}}]);
                 db.getSiblingDB(\"{db}\").people.createIndex({{name: 1}}, {{unique: true}});"
            ),
        );
        let path = std::env::temp_dir().join(format!("{db}.js"));
        let options = Options {
            database: DatabaseMode::DropCreate,
            data: DataMode::InsertUpdate,
            ..Options::default()
        };
        let script = dump_text(
            &rt,
            &conn,
            DriverKind::Mongodb,
            std::slice::from_ref(&db),
            options,
            path.clone(),
        );
        assert!(script.contains("dropDatabase"), "{script}");
        assert!(script.contains("createIndex"), "{script}");
        assert!(script.contains("replaceOne"), "{script}");
        run_script(&rt, &conn, None, &script);
        let count = rt
            .block_on(query_rows(
                conn.as_ref(),
                &format!("db.getSiblingDB(\"{db}\").people.countDocuments({{}})"),
                None,
            ))
            .unwrap();
        assert_eq!(cell(&count[0], 0).as_deref(), Some("2"));
        run_script(
            &rt,
            &conn,
            None,
            &format!("db.getSiblingDB(\"{db}\").dropDatabase();"),
        );
        let _ = std::fs::remove_file(&path);
    }
}

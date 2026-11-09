//! SQLite, through rusqlite and the bundled SQLite library.
//!
//! A SQLite connection must stay on one thread, so each [`SqliteConnection`] owns a thread
//! that holds the database and answers messages. Statements from `execute` become cursors
//! on that thread. A cursor steps only when its stream asks for the next batch, so a
//! result of any size costs one batch of memory, and the thread keeps answering other
//! requests (browsing, the sidebar, more statements) while a result waits to be read.
//!
//! Open cursors nest: a statement started while another is open runs in a frame on top
//! of it, and the frames below keep answering for their own cursors. A cursor closed
//! while one above it is still open lets go of its rows at once and of its compiled
//! statement when the frames above close. At most [`MAX_OPEN_CURSORS`] are open at once.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use deltos_data::{
    BrowsePage, BrowseRequest, CancelHandle, Capabilities, Column, Connection, ConnectionConfig,
    Driver, DriverKind, Error, ExecuteOptions, ForeignKey, Index, Namespace, NamespaceKind,
    ObjectEntry, ObjectInfo, ObjectKind, ObjectRef, Result, Row, RowBatch, RowEdit, RowStream,
    Value, ValueKind,
};
use futures::future::BoxFuture;
use rusqlite::types::ValueRef;
use rusqlite::{OpenFlags, Statement};
use tokio::sync::oneshot;

use crate::sql::{self, BrowseSql, Dialect, order_keys};
use crate::util;

/// How many results may be open on one connection at once.
pub const MAX_OPEN_CURSORS: u32 = 64;

const DIALECT: Dialect = Dialect::Sqlite;

/// Opens SQLite database files.
///
/// `ConnectionConfig::database` (or a `sqlite://` URL) names the file; `:memory:` opens a
/// private in-memory database. Options: `create=true` creates the file when it is
/// missing (otherwise a mistyped path is an error rather than a new empty database),
/// `read_only=true`, and `busy_timeout` in milliseconds (5000 by default).
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteDriver;

impl Driver for SqliteDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Sqlite
    }

    fn connect<'a>(
        &'a self,
        config: &'a ConnectionConfig,
    ) -> BoxFuture<'a, Result<Arc<dyn Connection>>> {
        Box::pin(async move {
            let connection = SqliteConnection::open(config).await?;
            Ok(Arc::new(connection) as Arc<dyn Connection>)
        })
    }
}

enum Msg {
    Run(Box<dyn FnOnce(&rusqlite::Connection) + Send>),
    Open(OpenRequest),
    Next {
        cursor: u64,
        reply: oneshot::Sender<Result<Option<RowBatch>>>,
    },
    Close {
        cursor: u64,
    },
    Shutdown,
}

struct OpenRequest {
    sql: String,
    batch: usize,
    reply: oneshot::Sender<Result<Opened>>,
}

struct Opened {
    cursor: Option<u64>,
    columns: Arc<[Column]>,
    affected: Option<u64>,
}

impl Opened {
    fn affected(affected: Option<u64>) -> Self {
        Self {
            cursor: None,
            columns: Arc::from(Vec::new()),
            affected,
        }
    }
}

/// An open SQLite database.
pub struct SqliteConnection {
    jobs: mpsc::Sender<Msg>,
    interrupt: Arc<rusqlite::InterruptHandle>,
}

impl SqliteConnection {
    /// Opens the database `config` names and starts its thread.
    pub async fn open(config: &ConnectionConfig) -> Result<Self> {
        let path = match &config.url {
            Some(url) if !url.trim().is_empty() => crate::url::sqlite_path(url).to_string(),
            _ => config.database.trim().to_string(),
        };
        if path.is_empty() {
            return Err(Error::Connect("Choose a database file.".to_string()));
        }
        let option = |name: &str| {
            config
                .options
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.trim().to_ascii_lowercase())
        };
        let is_true =
            |value: Option<String>| matches!(value.as_deref(), Some("true" | "1" | "yes"));
        let read_only = is_true(option("read_only"));
        let in_memory = path == ":memory:" || path.contains("mode=memory");
        let create = is_true(option("create")) || in_memory;
        let busy = option("busy_timeout")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(5000);

        let mut flags = OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        if read_only {
            flags |= OpenFlags::SQLITE_OPEN_READ_ONLY;
        } else {
            flags |= OpenFlags::SQLITE_OPEN_READ_WRITE;
            if create {
                flags |= OpenFlags::SQLITE_OPEN_CREATE;
            }
        }

        let (jobs, receiver) = mpsc::channel();
        let (ready, opened) = oneshot::channel();
        std::thread::Builder::new()
            .name("deltos-sqlite".to_string())
            .spawn(move || run_thread(path, flags, busy, ready, receiver))
            .map_err(|e| Error::Connect(format!("could not start the SQLite thread: {e}")))?;
        let interrupt = opened.await.map_err(|_| Error::Closed)??;
        Ok(Self {
            jobs,
            interrupt: Arc::new(interrupt),
        })
    }

    /// Runs `job` on the database thread.
    fn call<T, F>(&self, job: F) -> BoxFuture<'static, Result<T>>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Connection) -> Result<T> + Send + 'static,
    {
        let (reply, answer) = oneshot::channel();
        let sent = self.jobs.send(Msg::Run(Box::new(move |conn| {
            let _ = reply.send(job(conn));
        })));
        Box::pin(async move {
            sent.map_err(|_| Error::Closed)?;
            answer.await.map_err(|_| Error::Closed)?
        })
    }
}

impl Connection for SqliteConnection {
    fn kind(&self) -> DriverKind {
        DriverKind::Sqlite
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            schemas: false,
            transactions: true,
            edit_rows: true,
            cancel: true,
            language: "sql",
        }
    }

    fn server_version(&self) -> String {
        format!("SQLite {}", rusqlite::version())
    }

    fn namespaces(&self) -> BoxFuture<'_, Result<Vec<Namespace>>> {
        self.call(|conn| {
            let mut stmt = conn
                .prepare("SELECT name FROM pragma_database_list ORDER BY seq")
                .map_err(error)?;
            let names = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(error)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(error)?;
            Ok(names
                .into_iter()
                .map(|name| Namespace {
                    path: vec![name],
                    kind: NamespaceKind::Schema,
                })
                .collect())
        })
    }

    fn objects<'a>(&'a self, namespace: &'a [String]) -> BoxFuture<'a, Result<Vec<ObjectEntry>>> {
        let schema = schema_name(namespace).to_string();
        self.call(move |conn| objects(conn, &schema))
    }

    fn describe<'a>(&'a self, object: &'a ObjectRef) -> BoxFuture<'a, Result<ObjectInfo>> {
        let object = object.clone();
        self.call(move |conn| describe(conn, &object))
    }

    fn browse<'a>(
        &'a self,
        object: &'a ObjectRef,
        request: &'a BrowseRequest,
    ) -> BoxFuture<'a, Result<BrowsePage>> {
        let object = object.clone();
        let request = request.clone();
        self.call(move |conn| browse(conn, &object, &request))
    }

    fn count<'a>(
        &'a self,
        object: &'a ObjectRef,
        request: &'a BrowseRequest,
        exact: bool,
    ) -> BoxFuture<'a, Result<u64>> {
        let object = object.clone();
        let request = request.clone();
        self.call(move |conn| count(conn, &object, &request, exact))
    }

    fn execute<'a>(
        &'a self,
        statement: &'a str,
        options: &'a ExecuteOptions,
    ) -> BoxFuture<'a, Result<RowStream>> {
        let (reply, answer) = oneshot::channel();
        let sent = self.jobs.send(Msg::Open(OpenRequest {
            sql: statement.to_string(),
            batch: util::batch_size(options.batch_size),
            reply,
        }));
        let jobs = self.jobs.clone();
        Box::pin(async move {
            sent.map_err(|_| Error::Closed)?;
            let opened = answer.await.map_err(|_| Error::Closed)??;
            let batches = match opened.cursor {
                Some(id) => cursor_stream(CursorHandle { id, jobs }),
                None => Box::pin(futures::stream::empty()),
            };
            Ok(RowStream {
                columns: opened.columns,
                batches,
                affected: opened.affected,
                messages: Vec::new(),
            })
        })
    }

    fn apply_edits<'a>(
        &'a self,
        object: &'a ObjectRef,
        edits: &'a [RowEdit],
    ) -> BoxFuture<'a, Result<u64>> {
        let object = object.clone();
        let edits = edits.to_vec();
        self.call(move |conn| apply_edits(conn, &object, &edits))
    }

    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        Some(Arc::new(Interrupt(self.interrupt.clone())))
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        self.interrupt.interrupt();
        let _ = self.jobs.send(Msg::Shutdown);
        Box::pin(async {})
    }
}

struct Interrupt(Arc<rusqlite::InterruptHandle>);

impl CancelHandle for Interrupt {
    fn cancel(&self) -> BoxFuture<'static, Result<()>> {
        self.0.interrupt();
        Box::pin(async { Ok(()) })
    }
}

/// Tells the thread to close the cursor when the stream goes away.
struct CursorHandle {
    id: u64,
    jobs: mpsc::Sender<Msg>,
}

impl Drop for CursorHandle {
    fn drop(&mut self) {
        let _ = self.jobs.send(Msg::Close { cursor: self.id });
    }
}

fn cursor_stream(handle: CursorHandle) -> futures::stream::BoxStream<'static, Result<RowBatch>> {
    Box::pin(futures::stream::unfold(Some(handle), |state| async move {
        let handle = state?;
        let (reply, answer) = oneshot::channel();
        if handle
            .jobs
            .send(Msg::Next {
                cursor: handle.id,
                reply,
            })
            .is_err()
        {
            return Some((Err(Error::Closed), None));
        }
        match answer.await {
            Ok(Ok(Some(batch))) => Some((Ok(batch), Some(handle))),
            Ok(Ok(None)) => None,
            Ok(Err(e)) => Some((Err(e), None)),
            Err(_) => Some((Err(Error::Closed), None)),
        }
    }))
}

// ---------------------------------------------------------------------------------------
// The database thread.

fn run_thread(
    path: String,
    flags: OpenFlags,
    busy_ms: u64,
    ready: oneshot::Sender<Result<rusqlite::InterruptHandle>>,
    receiver: mpsc::Receiver<Msg>,
) {
    let conn = match rusqlite::Connection::open_with_flags(&path, flags) {
        Ok(conn) => conn,
        Err(e) => {
            let _ = ready.send(Err(Error::Connect(format!("could not open {path}: {e}"))));
            return;
        }
    };
    if let Err(e) = conn.busy_timeout(Duration::from_millis(busy_ms)) {
        tracing::debug!("could not set the SQLite busy timeout: {e}");
    }
    if ready.send(Ok(conn.get_interrupt_handle())).is_err() {
        return;
    }
    let mut ctx = Ctx {
        conn: &conn,
        receiver: &receiver,
        next_cursor: 1,
        open: 0,
    };
    // Messages for cursors that already finished end up here.
    let mut root = |msg: Msg| -> Option<Msg> {
        match msg {
            Msg::Next { reply, .. } => {
                let _ = reply.send(Ok(None));
                None
            }
            Msg::Close { .. } => None,
            other => Some(other),
        }
    };
    let _ = serve(&mut ctx, None, &mut root);
}

struct Ctx<'c> {
    conn: &'c rusqlite::Connection,
    receiver: &'c mpsc::Receiver<Msg>,
    next_cursor: u64,
    open: u32,
}

enum Flow {
    Continue,
    Shutdown,
}

/// A cursor being read: the rows of a statement owned by the frame below it.
struct Cursor<'s> {
    id: u64,
    rows: Option<rusqlite::Rows<'s>>,
    kinds: Vec<ValueKind>,
    batch: usize,
    pending: Option<Row>,
    finished: bool,
}

impl Cursor<'_> {
    /// Answers `msg` when it is for this cursor, or hands it back.
    fn handle(&mut self, msg: Msg) -> Option<Msg> {
        match msg {
            Msg::Next { cursor, reply } if cursor == self.id => {
                let _ = reply.send(self.next_batch());
                None
            }
            Msg::Close { cursor } if cursor == self.id => {
                self.finish();
                None
            }
            other => Some(other),
        }
    }

    fn finish(&mut self) {
        self.rows = None;
        self.pending = None;
        self.finished = true;
    }

    fn next_batch(&mut self) -> Result<Option<RowBatch>> {
        if self.finished {
            return Ok(None);
        }
        let mut out = Vec::with_capacity(self.batch.min(4096));
        out.extend(self.pending.take());
        while out.len() < self.batch {
            let Some(rows) = self.rows.as_mut() else {
                break;
            };
            match rows.next() {
                Ok(Some(row)) => out.push(read_row(row, &self.kinds)),
                Ok(None) => {
                    self.finish();
                    break;
                }
                Err(e) => {
                    self.finish();
                    return Err(error(e));
                }
            }
        }
        Ok((!out.is_empty()).then_some(RowBatch { rows: out }))
    }
}

/// Answers messages until this frame's cursor finishes or the connection shuts down.
/// Messages for cursors in frames below go to `outer`.
fn serve(
    ctx: &mut Ctx<'_>,
    mut mine: Option<&mut Cursor<'_>>,
    outer: &mut dyn FnMut(Msg) -> Option<Msg>,
) -> Flow {
    loop {
        if mine.as_ref().is_some_and(|cursor| cursor.finished) {
            return Flow::Continue;
        }
        let Ok(msg) = ctx.receiver.recv() else {
            return Flow::Shutdown;
        };
        let msg = match mine.as_deref_mut() {
            Some(cursor) => match cursor.handle(msg) {
                Some(msg) => msg,
                None => continue,
            },
            None => msg,
        };
        match msg {
            Msg::Run(job) => job(ctx.conn),
            Msg::Shutdown => return Flow::Shutdown,
            Msg::Open(request) => {
                let mut below = |msg: Msg| -> Option<Msg> {
                    let msg = match mine.as_deref_mut() {
                        Some(cursor) => cursor.handle(msg)?,
                        None => msg,
                    };
                    outer(msg)
                };
                if let Flow::Shutdown = open(ctx, request, &mut below) {
                    return Flow::Shutdown;
                }
            }
            other => {
                let _ = outer(other);
            }
        }
    }
}

/// Runs a script: every statement but the last to its end, then the last one either to
/// its end or, when it returns rows, as a new cursor frame.
fn open(
    ctx: &mut Ctx<'_>,
    request: OpenRequest,
    outer: &mut dyn FnMut(Msg) -> Option<Msg>,
) -> Flow {
    let OpenRequest { sql, batch, reply } = request;
    let pieces = sql::split_statements(&sql, DIALECT);
    let Some((last, before)) = pieces.split_last() else {
        let _ = reply.send(Ok(Opened::affected(None)));
        return Flow::Continue;
    };
    for piece in before {
        if let Err(e) = run_to_end(ctx.conn, &sql[piece.clone()]) {
            let _ = reply.send(Err(e));
            return Flow::Continue;
        }
    }
    let text = &sql[last.clone()];
    let stmt = match ctx.conn.prepare(text) {
        Ok(stmt) => stmt,
        Err(rusqlite::Error::MultipleStatement) => {
            // The splitter saw one statement where SQLite sees several.
            let result = ctx.conn.execute_batch(text).map_err(error);
            let _ = reply.send(result.map(|()| Opened::affected(None)));
            return Flow::Continue;
        }
        Err(e) => {
            let _ = reply.send(Err(error(e)));
            return Flow::Continue;
        }
    };
    if stmt.column_count() == 0 {
        let before = ctx.conn.total_changes();
        let result = step_to_end(stmt).map(|()| {
            let changed = ctx.conn.total_changes().saturating_sub(before);
            let dml = sql::counts_rows(&sql::first_keyword(text));
            Opened::affected((changed > 0 || dml).then_some(changed))
        });
        let _ = reply.send(result);
        return Flow::Continue;
    }
    if ctx.open >= MAX_OPEN_CURSORS {
        let _ = reply.send(Err(Error::Query(format!(
            "{MAX_OPEN_CURSORS} results are already open on this connection. Close one to run another statement."
        ))));
        return Flow::Continue;
    }
    let id = ctx.next_cursor;
    ctx.next_cursor += 1;
    ctx.open += 1;
    let flow = serve_cursor(ctx, stmt, id, batch, reply, outer);
    ctx.open -= 1;
    flow
}

fn serve_cursor(
    ctx: &mut Ctx<'_>,
    mut stmt: Statement<'_>,
    id: u64,
    batch: usize,
    reply: oneshot::Sender<Result<Opened>>,
    outer: &mut dyn FnMut(Msg) -> Option<Msg>,
) -> Flow {
    let mut columns = statement_columns(&stmt);
    let mut kinds: Vec<ValueKind> = columns.iter().map(|c| c.kind).collect();
    let mut rows = stmt.raw_query();
    // The first row is read now so columns without a declared type (expressions) can
    // take their kind from it. It is the only row read ahead.
    let pending = match rows.next() {
        Ok(Some(row)) => {
            for (i, column) in columns.iter_mut().enumerate() {
                if column.type_name.is_empty()
                    && let Ok(value) = row.get_ref(i)
                {
                    column.kind = kind_of_storage(value);
                    kinds[i] = column.kind;
                }
            }
            Some(read_row(row, &kinds))
        }
        Ok(None) => None,
        Err(e) => {
            let _ = reply.send(Err(error(e)));
            return Flow::Continue;
        }
    };
    let opened = Opened {
        cursor: pending.is_some().then_some(id),
        columns: columns.into(),
        affected: None,
    };
    if reply.send(Ok(opened)).is_err() || pending.is_none() {
        return Flow::Continue;
    }
    let mut cursor = Cursor {
        id,
        rows: Some(rows),
        kinds,
        batch,
        pending,
        finished: false,
    };
    serve(ctx, Some(&mut cursor), outer)
}

fn step_to_end(mut stmt: Statement<'_>) -> Result<()> {
    let mut rows = stmt.raw_query();
    while rows.next().map_err(error)?.is_some() {}
    Ok(())
}

fn run_to_end(conn: &rusqlite::Connection, sql: &str) -> Result<()> {
    match conn.prepare(sql) {
        Ok(stmt) => step_to_end(stmt),
        Err(rusqlite::Error::MultipleStatement) => conn.execute_batch(sql).map_err(error),
        Err(e) => Err(error(e)),
    }
}

// ---------------------------------------------------------------------------------------
// Values.

fn error(e: rusqlite::Error) -> Error {
    if let rusqlite::Error::SqliteFailure(failure, _) = &e
        && failure.code == rusqlite::ErrorCode::OperationInterrupted
    {
        return Error::Cancelled;
    }
    Error::Query(e.to_string())
}

fn read_row(row: &rusqlite::Row<'_>, kinds: &[ValueKind]) -> Row {
    let mut values = Vec::with_capacity(kinds.len());
    for (i, kind) in kinds.iter().enumerate() {
        values.push(match row.get_ref(i) {
            Ok(value) => to_value(value, *kind),
            Err(_) => Value::Null,
        });
    }
    util::row(values)
}

fn to_value(value: ValueRef<'_>, kind: ValueKind) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => match (kind, i) {
            (ValueKind::Bool, 0) => Value::Bool(false),
            (ValueKind::Bool, 1) => Value::Bool(true),
            _ => Value::Int(i),
        },
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Text(bytes) => match util::text_value(bytes) {
            Value::Text(text) if kind == ValueKind::Json => Value::Json(text),
            other => other,
        },
        ValueRef::Blob(bytes) => Value::Bytes(bytes.into()),
    }
}

fn to_sql(value: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as Sql;
    match value {
        Value::Null => Sql::Null,
        Value::Bool(b) => Sql::Integer(i64::from(*b)),
        Value::Int(i) => Sql::Integer(*i),
        Value::Float(f) => Sql::Real(*f),
        Value::Text(s) | Value::Json(s) => Sql::Text(s.to_string()),
        Value::Bytes(b) => Sql::Blob(b.to_vec()),
    }
}

fn params(values: &[Value]) -> rusqlite::ParamsFromIter<Vec<rusqlite::types::Value>> {
    rusqlite::params_from_iter(values.iter().map(to_sql).collect::<Vec<_>>())
}

fn kind_of_storage(value: ValueRef<'_>) -> ValueKind {
    match value {
        ValueRef::Null => ValueKind::Other,
        ValueRef::Integer(_) => ValueKind::Int,
        ValueRef::Real(_) => ValueKind::Float,
        ValueRef::Text(_) => ValueKind::Text,
        ValueRef::Blob(_) => ValueKind::Bytes,
    }
}

/// The kind for a declared column type, following SQLite's affinity rules with the
/// common names for dates, booleans and JSON on top.
pub fn kind_for_decl(decl: &str) -> ValueKind {
    let t = decl.to_ascii_uppercase();
    let has = |part: &str| t.contains(part);
    if t.is_empty() {
        ValueKind::Other
    } else if has("BOOL") {
        ValueKind::Bool
    } else if has("INT") {
        ValueKind::Int
    } else if has("DATETIME") || has("TIMESTAMP") {
        ValueKind::DateTime
    } else if has("DATE") {
        ValueKind::Date
    } else if has("TIME") {
        ValueKind::Time
    } else if has("JSON") {
        ValueKind::Json
    } else if has("UUID") || has("GUID") {
        ValueKind::Uuid
    } else if has("CHAR") || has("CLOB") || has("TEXT") {
        ValueKind::Text
    } else if has("BLOB") {
        ValueKind::Bytes
    } else if has("REAL") || has("FLOA") || has("DOUB") {
        ValueKind::Float
    } else if has("DEC") || has("NUMERIC") {
        ValueKind::Decimal
    } else {
        ValueKind::Other
    }
}

fn statement_columns(stmt: &Statement<'_>) -> Vec<Column> {
    stmt.columns()
        .into_iter()
        .map(|column| {
            let decl = column.decl_type().unwrap_or("");
            Column::new(column.name(), decl, kind_for_decl(decl))
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// Introspection and browsing.

fn schema_name(namespace: &[String]) -> &str {
    namespace.first().map_or("main", String::as_str)
}

fn master(schema: &str) -> String {
    format!("{}.sqlite_master", DIALECT.quote_ident(schema))
}

fn objects(conn: &rusqlite::Connection, schema: &str) -> Result<Vec<ObjectEntry>> {
    let sql = format!(
        "SELECT name, type FROM {} WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite!_%' ESCAPE '!' ORDER BY name COLLATE NOCASE",
        master(schema)
    );
    let mut stmt = conn.prepare(&sql).map_err(error)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(error)?;
    let stats = table_stats(conn, schema);
    Ok(rows
        .into_iter()
        .map(|(name, kind)| ObjectEntry {
            estimated_rows: stats
                .iter()
                .find(|(table, _)| table.eq_ignore_ascii_case(&name))
                .map(|(_, rows)| *rows),
            object: ObjectRef {
                namespace: vec![schema.to_string()],
                name,
                kind: if kind == "view" {
                    ObjectKind::View
                } else {
                    ObjectKind::Table
                },
            },
            size_bytes: None,
            comment: None,
        })
        .collect())
}

/// Row counts from `sqlite_stat1`, which `ANALYZE` fills. Empty when it was never run.
fn table_stats(conn: &rusqlite::Connection, schema: &str) -> Vec<(String, u64)> {
    let sql = format!(
        "SELECT tbl, stat FROM {}.sqlite_stat1",
        DIALECT.quote_ident(schema)
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    }) else {
        return Vec::new();
    };
    let mut out: Vec<(String, u64)> = Vec::new();
    for (table, stat) in rows.flatten() {
        let Some(n) = stat
            .as_deref()
            .and_then(|s| s.split_whitespace().next())
            .and_then(|n| n.parse().ok())
        else {
            continue;
        };
        if !out.iter().any(|(t, _)| *t == table) {
            out.push((table, n));
        }
    }
    out
}

struct TableColumn {
    name: String,
    decl: String,
    nullable: bool,
    default: Option<String>,
    pk: u16,
}

/// What browsing and describing need to know about a table or view.
struct TableInfo {
    schema: String,
    name: String,
    is_view: bool,
    sql: Option<String>,
    columns: Vec<TableColumn>,
    /// The columns that make the order total.
    tie_break: Vec<String>,
    /// A row identity column selected in addition to the visible ones.
    hidden: Option<String>,
}

impl TableInfo {
    fn load(conn: &rusqlite::Connection, object: &ObjectRef) -> Result<Self> {
        let schema = schema_name(&object.namespace).to_string();
        let name = object.name.clone();
        let mut stmt = conn
            .prepare(
                "SELECT name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo(?1, ?2) ORDER BY cid",
            )
            .map_err(error)?;
        let columns = stmt
            .query_map([&name, &schema], |row| {
                Ok((
                    TableColumn {
                        name: row.get(0)?,
                        decl: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        nullable: row.get::<_, i64>(2)? == 0,
                        default: row.get(3)?,
                        pk: row.get::<_, i64>(4)?.clamp(0, i64::from(u16::MAX)) as u16,
                    },
                    row.get::<_, i64>(5)?,
                ))
            })
            .map_err(error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(error)?
            .into_iter()
            .filter(|(_, hidden)| *hidden != 1)
            .map(|(column, _)| column)
            .collect::<Vec<_>>();
        if columns.is_empty() {
            return Err(Error::Query(format!(
                "There is no table or view named {name} in {schema}."
            )));
        }
        let (kind, sql): (String, Option<String>) = conn
            .query_row(
                &format!(
                    "SELECT type, sql FROM {} WHERE name = ?1 COLLATE NOCASE AND type IN ('table', 'view')",
                    master(&schema)
                ),
                [&name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(error)?;
        let is_view = kind == "view";

        let mut pk: Vec<&TableColumn> = columns.iter().filter(|c| c.pk > 0).collect();
        pk.sort_by_key(|c| c.pk);
        let mut tie_break = Vec::new();
        let mut hidden = None;
        if !is_view {
            let integer_key = pk.len() == 1 && pk[0].decl.eq_ignore_ascii_case("INTEGER");
            if integer_key {
                // An INTEGER PRIMARY KEY is the rowid itself.
                tie_break.push(pk[0].name.clone());
            } else if let Some(alias) = rowid_alias(conn, &schema, &name, &columns) {
                tie_break.push(alias.to_string());
                hidden = Some(alias.to_string());
            } else {
                // WITHOUT ROWID tables always have a primary key, and SQLite enforces
                // NOT NULL on it.
                tie_break.extend(pk.iter().map(|c| c.name.clone()));
            }
        }
        Ok(Self {
            schema,
            name,
            is_view,
            sql,
            columns,
            tie_break,
            hidden,
        })
    }

    fn table(&self) -> String {
        DIALECT.qualified(&[&self.schema], &self.name)
    }

    fn nullable(&self, column: &str) -> bool {
        if self.hidden.as_deref() == Some(column) {
            return false;
        }
        self.columns
            .iter()
            .find(|c| c.name == column)
            .is_none_or(|c| c.nullable && !(c.pk > 0 && self.hidden.is_none()))
    }
}

/// A name for the rowid that no column shadows, when the table has a rowid.
fn rowid_alias(
    conn: &rusqlite::Connection,
    schema: &str,
    table: &str,
    columns: &[TableColumn],
) -> Option<&'static str> {
    let alias = ["rowid", "_rowid_", "oid"]
        .into_iter()
        .find(|alias| !columns.iter().any(|c| c.name.eq_ignore_ascii_case(alias)))?;
    let probe = format!(
        "SELECT {} FROM {} LIMIT 0",
        DIALECT.quote_ident(alias),
        DIALECT.qualified(&[schema], table)
    );
    conn.prepare(&probe).ok().map(|_| alias)
}

fn browse(
    conn: &rusqlite::Connection,
    object: &ObjectRef,
    request: &BrowseRequest,
) -> Result<BrowsePage> {
    let info = TableInfo::load(conn, object)?;
    let order = order_keys(&request.sort, &info.tie_break, |c| info.nullable(c));
    let mut select_list = String::new();
    for (i, column) in info.columns.iter().enumerate() {
        if i > 0 {
            select_list.push_str(", ");
        }
        DIALECT.push_ident(&mut select_list, &column.name);
    }
    if let Some(hidden) = &info.hidden {
        select_list.push_str(", ");
        DIALECT.push_ident(&mut select_list, hidden);
    }
    let from = info.table();
    let built = BrowseSql {
        dialect: DIALECT,
        select_list: &select_list,
        from: &from,
        order: &order,
        total: !info.tie_break.is_empty(),
    }
    .build(request)?;

    let columns: Vec<Column> = info
        .columns
        .iter()
        .map(|c| Column {
            name: c.name.clone(),
            type_name: c.decl.clone(),
            kind: kind_for_decl(&c.decl),
            nullable: Some(c.nullable),
            primary_key: (c.pk > 0).then(|| c.pk - 1),
            default: c.default.clone(),
            comment: None,
        })
        .collect();
    let mut kinds: Vec<ValueKind> = columns.iter().map(|c| c.kind).collect();
    if info.hidden.is_some() {
        kinds.push(ValueKind::Int);
    }

    let mut stmt = conn.prepare(&built.sql).map_err(error)?;
    let mut rows = stmt.query(params(&built.params)).map_err(error)?;
    let limit = request.limit as usize;
    let mut out: Vec<Row> = Vec::with_capacity(limit.min(10_000));
    let mut more = false;
    while let Some(row) = rows.next().map_err(error)? {
        if out.len() == limit {
            more = true;
            break;
        }
        out.push(read_row(row, &kinds));
    }

    let position = |name: &str| -> Option<usize> {
        if info.hidden.as_deref() == Some(name) {
            Some(columns.len())
        } else {
            columns.iter().position(|c| c.name == name)
        }
    };
    let last_key = match out.last() {
        Some(last) if !info.tie_break.is_empty() => order
            .iter()
            .map(|key| {
                let i = position(&key.column)?;
                Some((key.column.clone(), last.get(i)?.clone()))
            })
            .collect::<Option<Vec<_>>>(),
        _ => None,
    };
    if info.hidden.is_some() {
        let width = columns.len();
        for row in &mut out {
            let mut values = std::mem::take(row).into_vec();
            values.truncate(width);
            *row = values.into_boxed_slice();
        }
    }
    Ok(BrowsePage {
        columns: columns.into(),
        rows: out,
        more,
        last_key,
    })
}

fn count(
    conn: &rusqlite::Connection,
    object: &ObjectRef,
    request: &BrowseRequest,
    exact: bool,
) -> Result<u64> {
    let schema = schema_name(&object.namespace);
    let unfiltered = request.filters.is_empty()
        && request
            .raw_where
            .as_deref()
            .is_none_or(|r| r.trim().is_empty());
    if !exact
        && unfiltered
        && let Some((_, n)) = table_stats(conn, schema)
            .into_iter()
            .find(|(t, _)| t.eq_ignore_ascii_case(&object.name))
    {
        return Ok(n);
    }
    let from = DIALECT.qualified(&[schema], &object.name);
    let (sql, values) = sql::count_sql(DIALECT, &from, request)?;
    let n: i64 = conn
        .query_row(&sql, params(&values), |row| row.get(0))
        .map_err(error)?;
    Ok(n.max(0) as u64)
}

fn describe(conn: &rusqlite::Connection, object: &ObjectRef) -> Result<ObjectInfo> {
    let info = TableInfo::load(conn, object)?;
    let schema = info.schema.as_str();
    let name = info.name.as_str();
    let columns: Vec<Column> = info
        .columns
        .iter()
        .map(|c| Column {
            name: c.name.clone(),
            type_name: c.decl.clone(),
            kind: kind_for_decl(&c.decl),
            nullable: Some(c.nullable && c.pk == 0),
            primary_key: (c.pk > 0).then(|| c.pk - 1),
            default: c.default.clone(),
            comment: None,
        })
        .collect();

    let mut indexes = Vec::new();
    let mut definition = info.sql.clone().map(|sql| format!("{sql};"));
    if !info.is_view {
        let mut stmt = conn
            .prepare("SELECT name, \"unique\", origin FROM pragma_index_list(?1, ?2) ORDER BY seq")
            .map_err(error)?;
        let list = stmt
            .query_map([name, schema], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? != 0,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(error)?;
        let mut columns_of = conn
            .prepare("SELECT name FROM pragma_index_info(?1, ?2) ORDER BY seqno")
            .map_err(error)?;
        for (index, unique, origin) in list {
            let names = columns_of
                .query_map([&index, &schema.to_string()], |row| {
                    Ok(row
                        .get::<_, Option<String>>(0)?
                        .unwrap_or_else(|| "(expression)".into()))
                })
                .map_err(error)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(error)?;
            indexes.push(Index {
                name: index,
                columns: names,
                unique,
                primary: origin == "pk",
                method: None,
            });
        }
        let key: Vec<String> = {
            let mut pk: Vec<&Column> = columns.iter().filter(|c| c.primary_key.is_some()).collect();
            pk.sort_by_key(|c| c.primary_key);
            pk.into_iter().map(|c| c.name.clone()).collect()
        };
        if !key.is_empty() && !indexes.iter().any(|i| i.primary) {
            indexes.insert(
                0,
                Index {
                    name: "PRIMARY KEY".to_string(),
                    columns: key,
                    unique: true,
                    primary: true,
                    method: None,
                },
            );
        }

        // CREATE INDEX and CREATE TRIGGER statements belong with the table's definition.
        let mut extra = conn
            .prepare(&format!(
                "SELECT sql FROM {} WHERE tbl_name = ?1 COLLATE NOCASE AND type IN ('index', 'trigger') AND sql IS NOT NULL ORDER BY type, name",
                master(schema)
            ))
            .map_err(error)?;
        let statements = extra
            .query_map([name], |row| row.get::<_, String>(0))
            .map_err(error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(error)?;
        if let Some(definition) = definition.as_mut() {
            for statement in statements {
                definition.push_str("\n\n");
                definition.push_str(&statement);
                definition.push(';');
            }
        }
    }

    let foreign_keys = foreign_keys(conn, schema, name)?;
    let estimated_rows = table_stats(conn, schema)
        .into_iter()
        .find(|(t, _)| t.eq_ignore_ascii_case(name))
        .map(|(_, n)| n);
    Ok(ObjectInfo {
        columns,
        indexes,
        foreign_keys,
        estimated_rows,
        size_bytes: None,
        comment: None,
        definition,
    })
}

fn foreign_keys(conn: &rusqlite::Connection, schema: &str, table: &str) -> Result<Vec<ForeignKey>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, \"table\", \"from\", \"to\" FROM pragma_foreign_key_list(?1, ?2) ORDER BY id, seq",
        )
        .map_err(error)?;
    let rows = stmt
        .query_map([table, schema], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(error)?;
    let mut out: Vec<(i64, ForeignKey)> = Vec::new();
    for (id, target, from, to) in rows {
        let entry = match out.iter_mut().find(|(i, _)| *i == id) {
            Some((_, fk)) => fk,
            None => {
                out.push((
                    id,
                    ForeignKey {
                        name: format!("fk_{table}_{id}"),
                        columns: Vec::new(),
                        target: ObjectRef {
                            namespace: vec![schema.to_string()],
                            name: target,
                            kind: ObjectKind::Table,
                        },
                        target_columns: Vec::new(),
                    },
                ));
                match out.last_mut() {
                    Some((_, fk)) => fk,
                    None => continue,
                }
            }
        };
        entry.columns.push(from);
        entry.target_columns.push(to.unwrap_or_default());
    }
    // A reference without target columns points at the target's primary key.
    for (_, fk) in &mut out {
        if fk.target_columns.iter().any(String::is_empty) {
            let mut stmt = conn
                .prepare("SELECT name FROM pragma_table_info(?1, ?2) WHERE pk > 0 ORDER BY pk")
                .map_err(error)?;
            let key = stmt
                .query_map([&fk.target.name, &schema.to_string()], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(error)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(error)?;
            if key.len() == fk.columns.len() {
                fk.target_columns = key;
            }
        }
    }
    Ok(out.into_iter().map(|(_, fk)| fk).collect())
}

fn apply_edits(conn: &rusqlite::Connection, object: &ObjectRef, edits: &[RowEdit]) -> Result<u64> {
    let table = DIALECT.qualified(&[schema_name(&object.namespace)], &object.name);
    // A savepoint works both inside and outside a transaction the person opened.
    conn.execute_batch("SAVEPOINT deltos_edits")
        .map_err(error)?;
    let result = (|| {
        let mut total = 0u64;
        for edit in edits {
            let Some(statement) = sql::edit_sql(DIALECT, &table, edit)? else {
                continue;
            };
            let n = conn
                .execute(&statement.sql, params(&statement.params))
                .map_err(error)? as u64;
            if statement.expect_one && n != 1 {
                return Err(sql::edit_mismatch(n));
            }
            total += n;
        }
        Ok(total)
    })();
    match result {
        Ok(total) => {
            conn.execute_batch("RELEASE deltos_edits").map_err(error)?;
            Ok(total)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK TO deltos_edits; RELEASE deltos_edits");
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests;

//! PostgreSQL, through tokio-postgres with rustls.
//!
//! A [`PostgresConnection`] keeps up to two sessions per database it has touched: a
//! metadata session for the sidebar, the table view and edits, and a query session for
//! statements the person runs. Sessions open on first use and reopen when the server
//! closed them. Other databases on the server are reached the same way, so the sidebar
//! can list every database without a second connection in the app.
//!
//! Statements stream with `query_raw` (see the crate documentation for why not portals).
//! The query session runs one statement at a time. Running a statement while the result
//! of the previous one is still open ends that result: the server gets a cancel request,
//! what it already sent is drained, and the old stream's next batch says it was closed.
//! Waiting instead would hang an app that runs the next statement before dropping the
//! last result. A stream dropped before its end is cancelled the same way.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use deltos_data::{
    BrowsePage, BrowseRequest, CancelHandle, Capabilities, Column, Connection, ConnectionConfig,
    Driver, DriverKind, Error, ExecuteOptions, Namespace, ObjectEntry, ObjectInfo, ObjectRef,
    Result, Row, RowBatch, RowEdit, RowStream, TlsMode, Value,
};
use futures::StreamExt;
use futures::future::BoxFuture;
use tokio::runtime::Handle;
use tokio_postgres::types::{Format, IsNull, ToSql, Type, to_sql_checked};
use tokio_postgres::{AsyncMessage, CancelToken, Client, Statement};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::sql::{self, Dialect};
use crate::{runtime, tls, util};

mod cancel;
mod decode;
mod introspect;

use decode::{Decoder, Raw};

const DIALECT: Dialect = Dialect::Postgres;

/// How many prepared statements each session keeps for reuse.
const STATEMENT_CACHE: usize = 64;

/// How many server notices a session keeps for the next result.
const MAX_NOTICES: usize = 100;

/// Opens PostgreSQL connections, and those of servers that speak its protocol.
#[derive(Debug, Clone, Copy, Default)]
pub struct PostgresDriver;

impl Driver for PostgresDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Postgres
    }

    fn connect<'a>(
        &'a self,
        config: &'a ConnectionConfig,
    ) -> BoxFuture<'a, Result<Arc<dyn Connection>>> {
        let config = config.clone();
        Box::pin(async move {
            let rt = runtime::handle()?;
            let connection = runtime::spawn(&rt.clone(), async move {
                PostgresConnection::open(&config, rt).await
            })
            .await?;
            Ok(Arc::new(connection) as Arc<dyn Connection>)
        })
    }
}

/// An open PostgreSQL server connection.
pub struct PostgresConnection {
    inner: Arc<Inner>,
}

struct Inner {
    rt: Handle,
    config: tokio_postgres::Config,
    tls: MakeRustlsConnect,
    default_db: String,
    version: String,
    databases: Mutex<HashMap<String, Arc<Database>>>,
    /// Every session is made read-only as it opens (see [`util::read_only`]).
    read_only: bool,
}

#[derive(Default)]
struct Database {
    meta: tokio::sync::Mutex<Option<Arc<Session>>>,
    query: tokio::sync::Mutex<Option<Arc<Session>>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Meta,
    Query,
}

/// One server session.
struct Session {
    client: Client,
    cancel: CancelToken,
    /// Where cancel requests go, when they can wait for the server (see [`cancel`]).
    cancel_target: Option<cancel::Target>,
    notices: Arc<Mutex<Vec<String>>>,
    /// Held for each operation, so statements on a session never interleave.
    lock: Arc<tokio::sync::Mutex<()>>,
    /// The result still being read on this session, if any.
    current: Mutex<Option<Arc<OpenResult>>>,
    statements: Mutex<Vec<(String, Statement)>>,
    search_path: Mutex<Option<String>>,
}

impl PostgresConnection {
    async fn open(config: &ConnectionConfig, rt: Handle) -> Result<Self> {
        let (pg_config, mode) = pg_config(config)?;
        let tls = MakeRustlsConnect::new(tls::client_config(mode).map_err(Error::Connect)?);
        let read_only = util::read_only(config);
        let first = Session::connect(&pg_config, &tls, &rt, read_only).await?;
        let row = first
            .client
            .query_one(
                "SELECT pg_catalog.current_database()::text, pg_catalog.version()",
                &[],
            )
            .await
            .map_err(pg_error)?;
        let default_db: String = row.try_get(0).map_err(pg_error)?;
        let banner: String = row.try_get(1).map_err(pg_error)?;
        let version = banner
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        let database = Database::default();
        *database.meta.lock().await = Some(Arc::new(first));
        let databases = HashMap::from([(default_db.clone(), Arc::new(database))]);
        Ok(Self {
            inner: Arc::new(Inner {
                rt,
                config: pg_config,
                tls,
                default_db,
                version,
                databases: Mutex::new(databases),
                read_only,
            }),
        })
    }

    fn spawn<T, F>(&self, f: impl FnOnce(Arc<Inner>) -> F) -> BoxFuture<'static, Result<T>>
    where
        T: Send + 'static,
        F: std::future::Future<Output = Result<T>> + Send + 'static,
    {
        runtime::spawn(&self.inner.rt, f(self.inner.clone()))
    }
}

impl Inner {
    /// The session for `db` in `role`, opening or reopening it as needed.
    async fn session(&self, db: &str, role: Role) -> Result<Arc<Session>> {
        let database = {
            let mut databases = self.databases.lock().map_err(|_| Error::Closed)?;
            databases.entry(db.to_string()).or_default().clone()
        };
        let slot = match role {
            Role::Meta => &database.meta,
            Role::Query => &database.query,
        };
        let mut slot = slot.lock().await;
        if let Some(session) = slot.as_ref()
            && !session.client.is_closed()
        {
            return Ok(session.clone());
        }
        let mut config = self.config.clone();
        config.dbname(db);
        let session =
            Arc::new(Session::connect(&config, &self.tls, &self.rt, self.read_only).await?);
        *slot = Some(session.clone());
        Ok(session)
    }

    /// Database and schema for a namespace path: `[db, schema]`, or `[schema]` in the
    /// database the connection opened.
    fn locate<'a>(&'a self, namespace: &'a [String]) -> (&'a str, Option<&'a str>) {
        match namespace {
            [db, schema, ..] => (db, Some(schema)),
            [schema] => (&self.default_db, Some(schema)),
            [] => (&self.default_db, None),
        }
    }

    fn sessions(&self) -> Vec<Arc<Session>> {
        let databases: Vec<Arc<Database>> = match self.databases.lock() {
            Ok(databases) => databases.values().cloned().collect(),
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for database in databases {
            for slot in [&database.meta, &database.query] {
                if let Ok(slot) = slot.try_lock() {
                    out.extend(slot.clone());
                }
            }
        }
        out
    }
}

impl Session {
    /// Opens a session; a `read_only` one has the server refuse writes in every
    /// transaction, and is not opened when the server will not do that.
    async fn connect(
        config: &tokio_postgres::Config,
        tls: &MakeRustlsConnect,
        rt: &Handle,
        read_only: bool,
    ) -> Result<Session> {
        let (client, connection) = config.connect(tls.clone()).await.map_err(connect_error)?;
        let notices = Arc::new(Mutex::new(Vec::new()));
        let sink = notices.clone();
        rt.spawn(async move {
            let mut connection = connection;
            let mut messages = futures::stream::poll_fn(move |cx| connection.poll_message(cx));
            while let Some(message) = messages.next().await {
                match message {
                    Ok(AsyncMessage::Notice(notice)) => {
                        if let Ok(mut notices) = sink.lock()
                            && notices.len() < MAX_NOTICES
                        {
                            notices.push(format!("{}: {}", notice.severity(), notice.message()));
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!("PostgreSQL connection ended: {e}");
                        break;
                    }
                }
            }
        });
        if read_only {
            client
                .batch_execute("SET default_transaction_read_only = on")
                .await
                .map_err(|e| {
                    Error::Connect(format!(
                        "the server could not make the session read-only: {}",
                        pg_error(e)
                    ))
                })?;
        }
        Ok(Session {
            cancel: client.cancel_token(),
            cancel_target: cancel::Target::of(config),
            client,
            notices,
            lock: Arc::new(tokio::sync::Mutex::new(())),
            current: Mutex::new(None),
            statements: Mutex::new(Vec::new()),
            search_path: Mutex::new(None),
        })
    }

    /// Whether a statement runs, or a result is open, on this session.
    fn is_busy(&self) -> bool {
        self.lock.try_lock().is_err() || self.current.lock().is_ok_and(|c| c.is_some())
    }

    /// Ends `result` if it is still open: cancels what the server is still producing and
    /// drops the rows, which the client then drains. Call with `lock` held, so the cancel
    /// request goes out before any later statement.
    async fn retire(&self, result: &Arc<OpenResult>, tls: &MakeRustlsConnect) {
        if let Ok(mut current) = self.current.lock()
            && current.as_ref().is_some_and(|c| Arc::ptr_eq(c, result))
        {
            *current = None;
        }
        // A stream in the middle of reading a batch holds the rows; cancelling first
        // makes that read end at once.
        let mut cancelled = false;
        if result.rows.try_lock().is_err() {
            self.send_cancel(tls).await;
            cancelled = true;
        }
        let rows = result.rows.lock().await.take();
        if rows.is_some() && !cancelled {
            self.send_cancel(tls).await;
        }
        drop(rows);
    }

    async fn send_cancel(&self, tls: &MakeRustlsConnect) {
        if let Err(e) = cancel::cancel(&self.cancel, self.cancel_target.as_ref(), tls).await {
            tracing::debug!("could not cancel a PostgreSQL statement: {e}");
        }
    }

    fn take_notices(&self) -> Vec<String> {
        self.notices
            .lock()
            .map(|mut notices| std::mem::take(&mut *notices))
            .unwrap_or_default()
    }

    /// A prepared statement for `sql`, reused while it stays in the cache.
    async fn prepare_cached(&self, sql: &str) -> Result<Statement> {
        if let Ok(mut cache) = self.statements.lock()
            && let Some(i) = cache.iter().position(|(text, _)| text == sql)
        {
            let entry = cache.remove(i);
            let statement = entry.1.clone();
            cache.insert(0, entry);
            return Ok(statement);
        }
        let statement = self.client.prepare(sql).await.map_err(pg_error)?;
        if let Ok(mut cache) = self.statements.lock() {
            cache.insert(0, (sql.to_string(), statement.clone()));
            cache.truncate(STATEMENT_CACHE);
        }
        Ok(statement)
    }

    fn forget(&self, sql: &str) {
        if let Ok(mut cache) = self.statements.lock() {
            cache.retain(|(text, _)| text != sql);
        }
    }

    /// Runs `sql` with text parameters and returns its rows.
    async fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<tokio_postgres::Row>> {
        Ok(self.query_statement(sql, params).await?.1)
    }

    /// Runs `sql` with text parameters and returns the statement it ran with its rows.
    /// A cached statement whose result shape changed (after an `ALTER TABLE`) is
    /// prepared again once.
    async fn query_statement(
        &self,
        sql: &str,
        params: &[Value],
    ) -> Result<(Statement, Vec<tokio_postgres::Row>)> {
        let params = text_params(params);
        let mut retried = false;
        loop {
            let statement = self.prepare_cached(sql).await?;
            let result = match self.client.query_raw(&statement, params.iter()).await {
                Ok(stream) => stream
                    .collect::<Vec<_>>()
                    .await
                    .into_iter()
                    .collect::<std::result::Result<Vec<_>, _>>(),
                Err(e) => Err(e),
            };
            match result {
                Ok(rows) => return Ok((statement, rows)),
                Err(e) if !retried && is_stale_plan(&e) => {
                    self.forget(sql);
                    retried = true;
                }
                Err(e) => return Err(pg_error(e)),
            }
        }
    }

    async fn set_search_path(&self, schema: &str) -> Result<()> {
        if self
            .search_path
            .lock()
            .is_ok_and(|current| current.as_deref() == Some(schema))
        {
            return Ok(());
        }
        let mut path = DIALECT.quote_ident(schema);
        if schema != "public" {
            path.push_str(", public");
        }
        self.client
            .query(
                "SELECT pg_catalog.set_config('search_path', $1, false)",
                &[&path],
            )
            .await
            .map_err(pg_error)?;
        if let Ok(mut current) = self.search_path.lock() {
            *current = Some(schema.to_string());
        }
        Ok(())
    }
}

impl Connection for PostgresConnection {
    fn kind(&self) -> DriverKind {
        DriverKind::Postgres
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            schemas: true,
            transactions: true,
            edit_rows: true,
            cancel: true,
            language: "sql",
        }
    }

    fn server_version(&self) -> String {
        self.inner.version.clone()
    }

    fn namespaces(&self) -> BoxFuture<'_, Result<Vec<Namespace>>> {
        self.spawn(|inner| async move {
            let session = inner.session(&inner.default_db, Role::Meta).await?;
            let _guard = session.lock.lock().await;
            introspect::namespaces(&session, &inner.default_db).await
        })
    }

    fn objects<'a>(&'a self, namespace: &'a [String]) -> BoxFuture<'a, Result<Vec<ObjectEntry>>> {
        let namespace = namespace.to_vec();
        self.spawn(|inner| async move {
            let (db, schema) = inner.locate(&namespace);
            let Some(schema) = schema else {
                return Ok(Vec::new());
            };
            let session = inner.session(db, Role::Meta).await?;
            let _guard = session.lock.lock().await;
            introspect::objects(&session, db, schema).await
        })
    }

    fn describe<'a>(&'a self, object: &'a ObjectRef) -> BoxFuture<'a, Result<ObjectInfo>> {
        let object = object.clone();
        self.spawn(|inner| async move {
            let (db, schema) = inner.locate(&object.namespace);
            let session = inner.session(db, Role::Meta).await?;
            let _guard = session.lock.lock().await;
            introspect::describe(&session, db, schema.unwrap_or("public"), &object).await
        })
    }

    fn browse<'a>(
        &'a self,
        object: &'a ObjectRef,
        request: &'a BrowseRequest,
    ) -> BoxFuture<'a, Result<BrowsePage>> {
        let object = object.clone();
        let request = request.clone();
        self.spawn(|inner| async move {
            let (db, schema) = inner.locate(&object.namespace);
            let session = inner.session(db, Role::Meta).await?;
            let _guard = session.lock.lock().await;
            browse(&session, schema.unwrap_or("public"), &object.name, &request).await
        })
    }

    fn count<'a>(
        &'a self,
        object: &'a ObjectRef,
        request: &'a BrowseRequest,
        exact: bool,
    ) -> BoxFuture<'a, Result<u64>> {
        let object = object.clone();
        let request = request.clone();
        self.spawn(|inner| async move {
            let (db, schema) = inner.locate(&object.namespace);
            let session = inner.session(db, Role::Meta).await?;
            let _guard = session.lock.lock().await;
            count(
                &session,
                schema.unwrap_or("public"),
                &object.name,
                &request,
                exact,
            )
            .await
        })
    }

    fn execute<'a>(
        &'a self,
        statement: &'a str,
        options: &'a ExecuteOptions,
    ) -> BoxFuture<'a, Result<RowStream>> {
        let sql = statement.to_string();
        let options = options.clone();
        self.spawn(|inner| async move { execute(inner, sql, options).await })
    }

    fn apply_edits<'a>(
        &'a self,
        object: &'a ObjectRef,
        edits: &'a [RowEdit],
    ) -> BoxFuture<'a, Result<u64>> {
        let object = object.clone();
        let edits = edits.to_vec();
        self.spawn(|inner| async move {
            let (db, schema) = inner.locate(&object.namespace);
            let session = inner.session(db, Role::Meta).await?;
            let _guard = session.lock.lock().await;
            let table = DIALECT.qualified(&[schema.unwrap_or("public")], &object.name);
            apply_edits(&session, &table, &edits).await
        })
    }

    fn apply_schema<'a>(
        &'a self,
        namespace: &'a [String],
        statements: &'a [String],
    ) -> BoxFuture<'a, Result<()>> {
        let namespace = namespace.to_vec();
        let statements = statements.to_vec();
        self.spawn(|inner| async move {
            let (db, _) = inner.locate(&namespace);
            let session = inner.session(db, Role::Meta).await?;
            let _guard = session.lock.lock().await;
            apply_schema(&session, &statements).await
        })
    }

    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        Some(Arc::new(Cancel(self.inner.clone())))
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        if let Ok(mut databases) = self.inner.databases.lock() {
            databases.clear();
        }
        Box::pin(async {})
    }
}

/// Cancels whatever any session of the connection is running.
struct Cancel(Arc<Inner>);

impl CancelHandle for Cancel {
    fn cancel(&self) -> BoxFuture<'static, Result<()>> {
        let inner = self.0.clone();
        runtime::spawn(&self.0.rt, async move {
            for session in inner.sessions() {
                // Idle sessions are left alone.
                if session.is_busy() {
                    cancel::cancel(&session.cancel, session.cancel_target.as_ref(), &inner.tls)
                        .await?;
                }
            }
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------------------
// Statements.

async fn execute(inner: Arc<Inner>, sql: String, options: ExecuteOptions) -> Result<RowStream> {
    let namespace = options.namespace.clone().unwrap_or_default();
    let (db, schema) = inner.locate(&namespace);
    let session = inner.session(db, Role::Query).await?;
    let _guard = session.lock.lock().await;
    let previous = session.current.lock().ok().and_then(|mut c| c.take());
    if let Some(previous) = previous {
        session.retire(&previous, &inner.tls).await;
    }
    session.take_notices();
    if let Some(schema) = schema {
        session.set_search_path(schema).await?;
    }
    let pieces = sql::split_statements(&sql, DIALECT);
    let Some(last) = pieces.last().cloned() else {
        return Ok(RowStream::affected(None));
    };
    if pieces.len() > 1 {
        session
            .client
            .batch_execute(&sql[..last.start])
            .await
            .map_err(pg_error)?;
    }
    let text = &sql[last];
    let statement = session.client.prepare(text).await.map_err(pg_error)?;
    let no_params: [&(dyn ToSql + Sync); 0] = [];
    let rows = session
        .client
        .query_raw(&statement, no_params.iter().copied())
        .await
        .map_err(pg_error)?;
    let mut rows = Box::pin(rows);

    if statement.columns().is_empty() {
        while let Some(row) = rows.next().await {
            row.map_err(pg_error)?;
        }
        let n = rows.rows_affected().unwrap_or(0);
        let mut result = RowStream::affected(
            (n > 0 || sql::counts_rows(&sql::first_keyword(text))).then_some(n),
        );
        result.messages = session.take_notices();
        return Ok(result);
    }

    let columns: Vec<Column> = statement
        .columns()
        .iter()
        .map(|c| {
            let decoder = Decoder::for_type(c.type_());
            Column::new(c.name(), type_name(c.type_()), decoder.kind())
        })
        .collect();
    let decoders: Arc<[Decoder]> = statement
        .columns()
        .iter()
        .map(|c| Decoder::for_type(c.type_()))
        .collect();
    let messages = session.take_notices();
    let open = Arc::new(OpenResult {
        rows: tokio::sync::Mutex::new(Some(rows)),
    });
    if let Ok(mut current) = session.current.lock() {
        *current = Some(open.clone());
    }
    let state = Streaming {
        open,
        decoders,
        batch: util::batch_size(options.batch_size),
        finished: false,
        session: session.clone(),
        tls: inner.tls.clone(),
        rt: inner.rt.clone(),
    };
    let batches = futures::stream::unfold(state, |mut state| async move {
        let batch = state.next_batch().await?;
        Some((batch, state))
    });
    Ok(RowStream {
        columns: columns.into(),
        batches: Box::pin(batches),
        affected: None,
        messages,
    })
}

/// The rows of the result open on a session. `None` once it ended or was closed.
struct OpenResult {
    rows: tokio::sync::Mutex<Option<Pin<Box<tokio_postgres::RowStream>>>>,
}

/// A result being read.
struct Streaming {
    open: Arc<OpenResult>,
    decoders: Arc<[Decoder]>,
    batch: usize,
    finished: bool,
    session: Arc<Session>,
    tls: MakeRustlsConnect,
    rt: Handle,
}

impl Streaming {
    async fn next_batch(&mut self) -> Option<Result<RowBatch>> {
        if self.finished {
            return None;
        }
        let mut slot = self.open.rows.lock().await;
        let Some(rows) = slot.as_mut() else {
            self.finished = true;
            return Some(Err(Error::Other(
                "This result was closed because another statement ran on the connection."
                    .to_string(),
            )));
        };
        let mut out = Vec::with_capacity(self.batch.min(4096));
        let mut error = None;
        while out.len() < self.batch {
            match rows.next().await {
                Some(Ok(row)) => out.push(decode_row(&row, &self.decoders)),
                Some(Err(e)) => {
                    error = Some(pg_error(e));
                    break;
                }
                None => break,
            }
        }
        if error.is_some() || out.len() < self.batch {
            *slot = None;
            drop(slot);
            self.finished = true;
            if let Ok(mut current) = self.session.current.lock()
                && current.as_ref().is_some_and(|c| Arc::ptr_eq(c, &self.open))
            {
                *current = None;
            }
        }
        match error {
            Some(e) => Some(Err(e)),
            None => (!out.is_empty()).then_some(Ok(RowBatch { rows: out })),
        }
    }
}

impl Drop for Streaming {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // The server would otherwise keep sending rows that are read only to be thrown
        // away. Retiring under the session lock keeps the cancel request from reaching
        // a later statement.
        let open = self.open.clone();
        let session = self.session.clone();
        let tls = self.tls.clone();
        self.rt.spawn(async move {
            let _guard = session.lock.lock().await;
            session.retire(&open, &tls).await;
        });
    }
}

fn decode_row(row: &tokio_postgres::Row, decoders: &[Decoder]) -> Row {
    let mut values = Vec::with_capacity(decoders.len());
    for (i, decoder) in decoders.iter().enumerate() {
        values.push(match row.try_get::<_, Raw<'_>>(i) {
            Ok(raw) => decoder.decode(raw.0),
            Err(_) => Value::Null,
        });
    }
    util::row(values)
}

fn type_name(ty: &Type) -> String {
    match ty.kind() {
        tokio_postgres::types::Kind::Array(element) => format!("{}[]", element.name()),
        _ => ty.name().to_string(),
    }
}

// ---------------------------------------------------------------------------------------
// Browsing.

async fn browse(
    session: &Session,
    schema: &str,
    table: &str,
    request: &BrowseRequest,
) -> Result<BrowsePage> {
    let info = introspect::TableInfo::load(session, schema, table).await?;
    let order = sql::order_keys(&request.sort, &info.tie_break, |c| info.nullable(c));
    let mut select_list = String::new();
    for (i, column) in info.columns.iter().enumerate() {
        if i > 0 {
            select_list.push_str(", ");
        }
        if column.text_cast {
            select_list.push_str("CAST(");
            DIALECT.push_ident(&mut select_list, &column.name);
            select_list.push_str(" AS text) AS ");
        }
        DIALECT.push_ident(&mut select_list, &column.name);
    }
    for hidden in &info.hidden {
        select_list.push_str(", ");
        DIALECT.push_ident(&mut select_list, hidden);
    }
    let from = DIALECT.qualified(&[schema], table);
    let built = sql::BrowseSql {
        dialect: DIALECT,
        select_list: &select_list,
        from: &from,
        order: &order,
        total: !info.tie_break.is_empty(),
    }
    .build(request)?;

    let (statement, rows) = session.query_statement(&built.sql, &built.params).await?;
    let decoders: Vec<Decoder> = statement
        .columns()
        .iter()
        .map(|c| Decoder::for_type(c.type_()))
        .collect();
    let limit = request.limit as usize;
    let more = rows.len() > limit;
    let mut out: Vec<Row> = rows
        .iter()
        .take(limit)
        .map(|row| decode_row(row, &decoders))
        .collect();

    let columns: Vec<Column> = info.columns.iter().map(|c| c.column()).collect();
    let position = |name: &str| -> Option<usize> {
        columns.iter().position(|c| c.name == name).or_else(|| {
            info.hidden
                .iter()
                .position(|h| h == name)
                .map(|i| columns.len() + i)
        })
    };
    let last_key = match out.last() {
        Some(last) if !info.tie_break.is_empty() => order
            .iter()
            .map(|key| {
                Some((
                    key.column.clone(),
                    last.get(position(&key.column)?)?.clone(),
                ))
            })
            .collect::<Option<Vec<_>>>(),
        _ => None,
    };
    if !info.hidden.is_empty() {
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

async fn count(
    session: &Session,
    schema: &str,
    table: &str,
    request: &BrowseRequest,
    exact: bool,
) -> Result<u64> {
    let from = DIALECT.qualified(&[schema], table);
    let (count_sql, params) = sql::count_sql(DIALECT, &from, request)?;
    if !exact {
        // The planner's estimate answers at once on any table size and follows the
        // filters. It reads the same statistics as pg_class.reltuples, scaled to the
        // table's current size.
        let explain = format!("EXPLAIN (FORMAT JSON) {count_sql}");
        match session.query(&explain, &params).await {
            Ok(rows) => {
                let plan: Option<String> = rows.first().and_then(|row| row.try_get(0).ok());
                if let Some(n) = plan.as_deref().and_then(plan_rows) {
                    return Ok(n);
                }
            }
            Err(e) => tracing::debug!("could not estimate the row count: {e}"),
        }
    }
    let rows = session.query(&count_sql, &params).await?;
    let n: i64 = rows
        .first()
        .ok_or_else(|| Error::Query("The count returned nothing.".to_string()))?
        .try_get(0)
        .map_err(pg_error)?;
    Ok(n.max(0) as u64)
}

/// `Plan Rows` of the node under the aggregate in `EXPLAIN (FORMAT JSON)` output.
fn plan_rows(json: &str) -> Option<u64> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let plan = value.get(0)?.get("Plan")?;
    let node = plan
        .get("Plans")
        .and_then(|plans| plans.get(0))
        .unwrap_or(plan);
    let rows = node.get("Plan Rows")?.as_f64()?;
    Some(rows.max(0.0) as u64)
}

async fn apply_edits(session: &Session, table: &str, edits: &[RowEdit]) -> Result<u64> {
    session
        .client
        .batch_execute("BEGIN")
        .await
        .map_err(pg_error)?;
    let result = async {
        let mut total = 0u64;
        for edit in edits {
            let Some(statement) = sql::edit_sql(DIALECT, table, edit)? else {
                continue;
            };
            let params = text_params(&statement.params);
            let n = session
                .client
                .execute_raw(&statement.sql, params.iter())
                .await
                .map_err(pg_error)?;
            if statement.expect_one && n != 1 {
                return Err(sql::edit_mismatch(n));
            }
            total += n;
        }
        Ok(total)
    }
    .await;
    match result {
        Ok(total) => {
            session
                .client
                .batch_execute("COMMIT")
                .await
                .map_err(pg_error)?;
            Ok(total)
        }
        Err(e) => {
            let _ = session.client.batch_execute("ROLLBACK").await;
            Err(e)
        }
    }
}

/// Runs schema statements in one transaction. PostgreSQL's DDL is transactional, so a
/// failure part way leaves the tables as they were.
async fn apply_schema(session: &Session, statements: &[String]) -> Result<()> {
    session
        .client
        .batch_execute("BEGIN")
        .await
        .map_err(pg_error)?;
    let mut result = Ok(());
    for statement in statements {
        if let Err(e) = session.client.batch_execute(statement).await {
            result = Err(pg_error(e).in_statement(statement));
            break;
        }
    }
    match result {
        Ok(()) => {
            session
                .client
                .batch_execute("COMMIT")
                .await
                .map_err(pg_error)?;
            // Cached plans may describe the old shape of a table.
            if let Ok(mut cache) = session.statements.lock() {
                cache.clear();
            }
            Ok(())
        }
        Err(e) => {
            let _ = session.client.batch_execute("ROLLBACK").await;
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------------------
// Parameters, configuration and errors.

/// A parameter sent in text form, so the server parses it as whatever type the statement
/// needs there. Values read from the server go back the same way they came.
#[derive(Debug)]
struct TextParam(Option<String>);

impl ToSql for TextParam {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut bytes::BytesMut,
    ) -> std::result::Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match &self.0 {
            Some(text) => {
                out.extend_from_slice(text.as_bytes());
                Ok(IsNull::No)
            }
            None => Ok(IsNull::Yes),
        }
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> Format {
        Format::Text
    }

    to_sql_checked!();
}

fn text_params(values: &[Value]) -> Vec<TextParam> {
    values
        .iter()
        .map(|v| TextParam(util::value_text(v).map(|t| t.into_owned())))
        .collect()
}

/// Options tokio-postgres reads from a URL. libpq takes more; the rest are dropped
/// rather than failing the connection.
const KNOWN_PARAMS: &[&str] = &[
    "user",
    "password",
    "dbname",
    "options",
    "application_name",
    "sslmode",
    "sslnegotiation",
    "host",
    "hostaddr",
    "port",
    "connect_timeout",
    "tcp_user_timeout",
    "keepalives",
    "keepalives_idle",
    "keepalives_interval",
    "keepalives_retries",
    "target_session_attrs",
    "channel_binding",
    "load_balance_hosts",
];

/// The tokio-postgres configuration and the TLS mode for `config`.
fn pg_config(config: &ConnectionConfig) -> Result<(tokio_postgres::Config, TlsMode)> {
    let mut mode = config.tls;
    let mut pg = match config
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        Some(url) => {
            let (url, url_mode) = clean_url(url);
            if let Some(url_mode) = url_mode {
                mode = url_mode;
            }
            url.parse::<tokio_postgres::Config>()
                .map_err(|e| Error::Connect(format!("the connection URL is not valid: {e}")))?
        }
        None => {
            let mut pg = tokio_postgres::Config::new();
            pg.host(if config.host.trim().is_empty() {
                "localhost"
            } else {
                config.host.trim()
            });
            pg.port(config.port.unwrap_or(5432));
            pg
        }
    };
    if pg.get_user().is_none() {
        pg.user(if config.user.is_empty() {
            "postgres"
        } else {
            config.user.as_str()
        });
    }
    if pg.get_password().is_none()
        && let Some(password) = &config.password
    {
        pg.password(password.as_str());
    }
    if pg.get_dbname().is_none() && !config.database.trim().is_empty() {
        pg.dbname(config.database.trim());
    }
    if pg.get_hosts().is_empty() {
        pg.host("localhost");
    }
    pg.application_name("Deltos");
    for (key, value) in &config.options {
        match key.as_str() {
            "application_name" => {
                pg.application_name(value.as_str());
            }
            "options" => {
                pg.options(value.as_str());
            }
            "connect_timeout" => {
                if let Ok(seconds) = value.parse::<u64>() {
                    pg.connect_timeout(std::time::Duration::from_secs(seconds));
                }
            }
            // Applied to each session as it opens.
            "read_only" => {}
            other => tracing::debug!("ignoring PostgreSQL option {other}"),
        }
    }
    if pg.get_connect_timeout().is_none() {
        pg.connect_timeout(std::time::Duration::from_secs(15));
    }
    pg.ssl_mode(match mode {
        TlsMode::Disable => tokio_postgres::config::SslMode::Disable,
        TlsMode::Prefer => tokio_postgres::config::SslMode::Prefer,
        TlsMode::Require | TlsMode::VerifyFull => tokio_postgres::config::SslMode::Require,
    });
    Ok((pg, mode))
}

/// Drops the URL options tokio-postgres would refuse and reads `sslmode`, including the
/// `verify-ca` and `verify-full` it does not know.
fn clean_url(url: &str) -> (String, Option<TlsMode>) {
    let Some((base, query)) = url.split_once('?') else {
        return (url.to_string(), None);
    };
    let mut mode = None;
    let mut kept: Vec<&str> = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "sslmode" {
            mode = Some(match value {
                "disable" => TlsMode::Disable,
                "allow" | "prefer" => TlsMode::Prefer,
                "require" => TlsMode::Require,
                _ => TlsMode::VerifyFull,
            });
            continue;
        }
        if KNOWN_PARAMS.contains(&key) {
            kept.push(pair);
        } else {
            tracing::debug!("ignoring PostgreSQL URL option {key}");
        }
    }
    let url = if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    };
    (url, mode)
}

fn is_stale_plan(e: &tokio_postgres::Error) -> bool {
    e.code() == Some(&tokio_postgres::error::SqlState::FEATURE_NOT_SUPPORTED)
}

fn pg_error(e: tokio_postgres::Error) -> Error {
    if let Some(db) = e.as_db_error() {
        if db.code() == &tokio_postgres::error::SqlState::QUERY_CANCELED {
            return Error::Cancelled;
        }
        let mut message = db.message().to_string();
        if let Some(c) = message.get(..1) {
            message.replace_range(..1, &c.to_uppercase());
        }
        if !message.ends_with('.') {
            message.push('.');
        }
        if let Some(detail) = db.detail() {
            message.push('\n');
            message.push_str(detail);
        }
        if let Some(hint) = db.hint() {
            message.push_str("\nHint: ");
            message.push_str(hint);
        }
        return Error::Query(message);
    }
    if e.is_closed() {
        return Error::Closed;
    }
    Error::Query(e.to_string())
}

fn connect_error(e: tokio_postgres::Error) -> Error {
    match e.as_db_error() {
        Some(db) => Error::Connect(db.message().to_string()),
        None => Error::Connect(e.to_string()),
    }
}

#[cfg(test)]
mod tests;
